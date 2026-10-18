/// Own all children from spawn until wait. Errors kill and reap the whole group.
struct ManagedChild {
    child: Child,
    reaped: bool,
    stderr: Option<std::process::ChildStderr>,
    diagnostic: Vec<u8>,
    diagnostic_truncated: bool,
}
#[derive(Default)]
struct Children(Vec<ManagedChild>, Option<Arc<Mutex<Vec<bool>>>>);
/// Kill the process group that `pid` leads. Callers use this only while the leader is still
/// unreaped (see `reap_exited`), so `pid` still names the group that was spawned.
fn kill_group(pid: u32) {
    // Each applet starts its own group. A negative PID addresses descendants
    // that retain that group, without touching the runner or its terminal.
    // SAFETY: kill(2) takes no pointers. Applets are spawned with process_group(0), so `pid`
    // is their process group id and -pid names that group. The pid is reserved while the
    // leader is running or an unreaped zombie, so it cannot have been recycled for another
    // process group.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

/// Whether the child has exited without being reaped. `waitid` with WNOWAIT leaves the
/// zombie in place, which keeps its pid reserved.
fn exited_unreaped(pid: u32) -> std::io::Result<bool> {
    // SAFETY: siginfo_t is plain data, and all-zero is a valid bit pattern for it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid writes only into `info`, a live siginfo_t. WNOWAIT leaves the child
    // unreaped, and WNOHANG makes the call return at once. While the child runs, si_signo
    // stays 0; once it has exited, si_signo is SIGCHLD.
    if unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.si_signo == libc::SIGCHLD)
}

/// Reap a child that has exited, signalling its process group first. The group signal
/// happens while the leader is still a zombie, so its pid cannot be recycled in between.
/// Returns Ok(None) while the child is still running.
fn reap_exited(child: &mut Child) -> std::io::Result<Option<std::process::ExitStatus>> {
    if !exited_unreaped(child.id())? {
        return Ok(None);
    }
    kill_group(child.id());
    child.wait().map(Some)
}
impl Drop for Children {
    fn drop(&mut self) {
        if let Some(target) = &self.1 {
            if let Ok(mut target) = target.lock() {
                for (target, child) in target.iter_mut().zip(&self.0) {
                    *target = child.diagnostic_truncated;
                }
            }
        }
        for child in &mut self.0 {
            if !child.reaped {
                kill_group(child.child.id());
            }
        }
        for child in &mut self.0 {
            if !child.reaped {
                let _ = child.child.wait();
            }
        }
    }
}

#[derive(Default)]
struct InputWriter {
    handle: Option<std::thread::JoinHandle<Result<()>>>,
    stop: Option<Arc<AtomicBool>>,
}

impl InputWriter {
    fn spawn<W>(
        &mut self,
        mut sink: W,
        data: Vec<u8>,
        offset: usize,
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) where
        W: Write + AsRawFd + Send + 'static,
    {
        debug_assert!(offset <= data.len());
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());
        self.handle = Some(std::thread::spawn(move || {
            write_input(&mut sink, &data[offset..], &cancelled, &stop, deadline)
        }));
    }

    fn join(&mut self) -> Result<()> {
        if let Some(writer) = self.handle.take() {
            writer
                .join()
                .map_err(|_| anyhow!("input writer panicked"))??;
        }
        Ok(())
    }
}

impl Drop for InputWriter {
    fn drop(&mut self) {
        // Never join during error propagation. Stop the private writer even if
        // an escaped descendant retains a reader outside our process groups.
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

fn write_input(
    sink: &mut (impl Write + AsRawFd),
    data: &[u8],
    cancelled: &AtomicBool,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    // This is the newly created pipe's private write end, owned by this worker.
    // It must not hold the global stdout/trace lock while feeding native stdin.
    let fd = sink.as_raw_fd();
    // `fd` is the write end of the pipe owned by `sink`, which is borrowed for the whole call
    // and so stays open.
    crate::platform::unix_ffi::set_nonblocking(fd).context("setting applet input nonblocking")?;
    let mut offset = 0;
    loop {
        check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
            bail!("workflow cancelled while feeding applet input");
        }
        if offset == data.len() {
            return Ok(());
        }
        match sink.write(&data[offset..]) {
            Ok(0) => bail!("writing applet input made no progress"),
            Ok(count) => offset += count,
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                // Wait for `sink`'s open pipe to become writable. EINTR just retries the loop.
                match crate::platform::unix_ffi::poll_one(
                    fd,
                    libc::POLLOUT,
                    deadline_poll_timeout(deadline, 20),
                ) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error).context("waiting for applet input"),
                }
            }
            Err(error) => return Err(error).context("writing applet input"),
        }
    }
}

type StdinRemainder = (std::io::PipeWriter, Vec<u8>, usize);

fn prefill_stdin_pipe(data: Vec<u8>) -> Result<(std::io::PipeReader, Option<StdinRemainder>)> {
    let (reader, mut pipe_writer) = std::io::pipe().context("creating applet stdin pipe")?;
    let fd = pipe_writer.as_raw_fd();
    // `fd` is the write end of the pipe created above, owned by `pipe_writer`, which is still
    // alive here.
    let flags =
        crate::platform::unix_ffi::status_flags(fd).context("getting applet stdin pipe flags")?;
    crate::platform::unix_ffi::set_status_flags(fd, flags | libc::O_NONBLOCK)
        .context("setting applet stdin pipe nonblocking")?;

    // Exactly one nonblocking write is attempted. EINTR and WouldBlock leave
    // the unaccepted suffix for the post-spawn writer thread.
    let write_result = pipe_writer.write(&data);
    // `flags` is the word read above, and `fd` is still the open pipe writer. This restores the
    // blocking mode saved before the write.
    crate::platform::unix_ffi::set_status_flags(fd, flags)
        .context("restoring applet stdin pipe flags")?;
    let written = match write_result {
        Ok(written) => written,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) =>
        {
            0
        }
        Err(error) => return Err(error).context("prefilling applet stdin pipe"),
    };

    if written == data.len() {
        drop(pipe_writer);
        Ok((reader, None))
    } else {
        Ok((reader, Some((pipe_writer, data, written))))
    }
}

struct ChildObserver {
    #[cfg(target_os = "macos")]
    queue: OwnedFd,
    #[cfg(target_os = "linux")]
    epoll: OwnedFd,
    // Linux pidfds stay open while registered: closing one removes it from the set.
    #[cfg(target_os = "linux")]
    pidfds: Mutex<Vec<(u32, OwnedFd)>>,
}
impl ChildObserver {
    #[cfg(target_os = "macos")]
    fn new() -> Option<Self> {
        // SAFETY: kqueue takes no arguments and no pointers. A -1 result returns None just
        // below, so a non-negative `fd` is a new descriptor with no other owner.
        let fd = unsafe { libc::kqueue() };
        if fd == -1 {
            return None;
        }
        // Darwin installs close-on-exec and close-on-fork before publishing
        // kqueue descriptors (XNU kqueue creation); no fcntl race or extra
        // descriptor configuration is needed here.
        // SAFETY: `fd` is a new kqueue descriptor, checked against -1 just above and not
        // shared, so OwnedFd becomes its only owner and closes it exactly once.
        let queue = unsafe { OwnedFd::from_raw_fd(fd) };
        Some(Self { queue })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn new() -> Option<Self> {
        None
    }

    #[cfg(target_os = "macos")]
    fn raw_fd(&self) -> libc::c_int {
        self.queue.as_raw_fd()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn raw_fd(&self) -> libc::c_int {
        -1
    }

    #[cfg(not(target_os = "linux"))]
    fn observe(&self, pid: u32) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let change = libc::kevent {
                ident: pid as _,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let no_wait = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `change` and `no_wait` are live locals passed by pointer, with a change
            // count of 1 and no event buffer (null, 0). `self.queue` is the open kqueue.
            let result = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    &no_wait,
                )
            };
            if result == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = pid;
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn drain(&self) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: kevent is a C struct of integers and pointers, so the all-zero bit
            // pattern is a valid value.
            let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
            let no_wait = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            loop {
                // SAFETY: no changes are submitted (null, 0). `events` has 16 entries and
                // `events.len()` is 16, so the kernel writes only inside the array. `no_wait`
                // is a live zero timeout, so the call does not block.
                let ready = unsafe {
                    libc::kevent(
                        self.queue.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        events.len() as _,
                        &no_wait,
                    )
                };
                if ready == -1 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        return Ok(());
                    }
                    return Err(error);
                }
                if ready == 0 {
                    return Ok(());
                }
                // Only direct-child NOTE_EXIT registrations can enter this
                // queue, so draining until empty is bounded by this group.
            }
        }
        #[cfg(not(target_os = "macos"))]
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl ChildObserver {
    // An epoll set of pidfds: the set becomes readable as soon as any observed
    // child exits, so the wait loop wakes on the exit itself, not on a timer.
    fn new() -> Option<Self> {
        // Readiness replaces WNOWAIT only when the host retains child zombies.
        // Preserve an embedding host's SIGCHLD disposition and use the existing
        // waitid fallback if it requests automatic reaping instead.
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: this only queries SIGCHLD into live writable storage. No new
        // action is installed and a successful query initializes the structure.
        if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: the successful sigaction query above initialized action.
        let action = unsafe { action.assume_init() };
        if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
            return None;
        }
        // SAFETY: epoll_create1 takes only flags, and EPOLL_CLOEXEC keeps the descriptor out
        // of child processes. A -1 result returns None just below, so `fd` is a new one.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd == -1 {
            return None;
        }
        Some(Self {
            // SAFETY: `fd` is a new epoll descriptor, checked against -1 just above and not
            // shared, so OwnedFd becomes its only owner and closes it exactly once.
            epoll: unsafe { OwnedFd::from_raw_fd(fd) },
            pidfds: Mutex::new(Vec::new()),
        })
    }

    fn raw_fd(&self) -> libc::c_int {
        self.epoll.as_raw_fd()
    }

    fn observe(&self, pid: u32) -> std::io::Result<()> {
        // pidfd_open needs Linux 5.3. Older kernels fail with ENOSYS and the
        // caller falls back to timed polling; ESRCH means the child is gone.
        let raw =
            // SAFETY: pidfd_open takes a pid and a flags word, and no pointers. Flags is 0,
            // the default blocking mode. A -1 result is returned as an error just below.
            unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
        if raw == -1 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `raw` is a descriptor number returned by pidfd_open and checked against -1
        // earlier in this function. Nothing else holds it, so OwnedFd becomes its only owner.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
        let mut event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: u64::from(pid),
        };
        // SAFETY: `self.epoll` is the open epoll set, and `pidfd` is a live descriptor that
        // this frame owns until it is stored in `pidfds`. `event` is a live local that the
        // kernel only reads for EPOLL_CTL_ADD. On failure, dropping `pidfd` unregisters it.
        let added = unsafe {
            libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                pidfd.as_raw_fd(),
                &mut event,
            )
        };
        if added == -1 {
            return Err(std::io::Error::last_os_error());
        }
        self.pidfds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((pid, pidfd));
        Ok(())
    }

    fn drain(&self) -> std::io::Result<Vec<u32>> {
        let mut exited = Vec::new();
        // SAFETY: epoll_event is a C struct of integers, so the all-zero bit pattern is valid.
        let mut events: [libc::epoll_event; 16] = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: `self.epoll` is the open epoll set. `events` has 16 entries and 16 is
            // passed as maxevents, so the kernel writes only inside the array. A zero timeout
            // means the call does not block.
            let ready = unsafe {
                libc::epoll_wait(
                    self.epoll.as_raw_fd(),
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    0,
                )
            };
            if ready == -1 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(exited);
                }
                return Err(error);
            }
            if ready == 0 {
                return Ok(exited);
            }
            let mut pidfds = self
                .pidfds
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for event in &events[..ready as usize] {
                // A pidfd reports leader exit without reaping it. Closing our
                // descriptor automatically removes its epoll registration; the
                // Child owner still reserves the PID until group cleanup/reap.
                let pid = event.u64 as u32;
                // Error/hangup notifications do not authorize skipping the
                // identity-preserving waitid check (for example, an auto-reaped
                // child). Keep the ordinary status probe for those events.
                if event.events & libc::EPOLLIN as u32 != 0
                    && event.events & (libc::EPOLLERR | libc::EPOLLHUP) as u32 == 0
                {
                    exited.push(pid);
                }
                pidfds.retain(|(registered_pid, _)| *registered_pid != pid);
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod pidfd_observer_tests {
    use super::*;

    #[test]
    #[ignore = "isolated process-wide SIGCHLD disposition check"]
    fn lane_v2_pidfd_fast_reap_refuses_automatic_reaping() {
        let mut original = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        let queried =
            // SAFETY: this queries the current action into valid writable storage.
            unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), original.as_mut_ptr()) };
        assert_eq!(queried, 0);
        // SAFETY: the query succeeded and initialized the structure.
        let original = unsafe { original.assume_init() };
        struct Restore(libc::sigaction);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: the saved action was initialized by sigaction above.
                unsafe {
                    libc::sigaction(libc::SIGCHLD, &self.0, std::ptr::null_mut());
                }
            }
        }
        let _restore = Restore(original);
        for ignore in [false, true] {
            let mut action = original;
            if ignore {
                action.sa_sigaction = libc::SIG_IGN;
            } else {
                action.sa_flags |= libc::SA_NOCLDWAIT;
            }
            let installed =
                // SAFETY: action is initialized and this isolated test restores the
                // process-wide disposition before returning, including on panic.
                unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) };
            assert_eq!(installed, 0);
            assert!(ChildObserver::new().is_none());
        }
    }

    #[test]
    fn lane_v2_pidfd_ready_identities_survive_exit_before_registration() {
        let observer = ChildObserver::new().expect("creating epoll set");
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .process_group(0)
            .spawn()
            .expect("spawning child");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !exited_unreaped(child.id()).expect("probing child without reaping") {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Err(error) = observer.observe(child.id()) {
            child.wait().expect("reaping child on unsupported kernel");
            assert_eq!(error.raw_os_error(), Some(libc::ENOSYS));
            return;
        }
        assert_eq!(
            observer.drain().expect("draining ready pidfd"),
            [child.id()]
        );
        assert!(observer.drain().expect("no duplicate exit").is_empty());
        // The observer did not reap the zombie. Killing its group is still safe;
        // Child::wait, rather than readiness, remains the exit-status authority.
        kill_group(child.id());
        assert_eq!(child.wait().expect("reaping child").code(), Some(7));
    }

    #[test]
    fn exited_child_wakes_observer_before_it_is_reaped() {
        let observer = ChildObserver::new().expect("creating epoll set");
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawning child");
        match observer.observe(child.id()) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {
                // Kernels before 5.3 have no pidfd_open; timed polling covers them.
                child.wait().expect("waiting for child");
                return;
            }
            Err(error) => panic!("observing child: {error}"),
        }
        // The child is a zombie until waited on, so its pidfd is already readable.
        let mut descriptor = libc::pollfd {
            fd: observer.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` is a live pollfd and the count is 1. Its fd is the epoll set
        // owned by `observer`, which lives until the end of this test.
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 10_000) }, 1);
        assert_eq!(
            observer.drain().expect("draining exit notification"),
            vec![child.id()]
        );
        assert!(observer
            .drain()
            .expect("draining empty observer")
            .is_empty());
        let mut quiet = libc::pollfd {
            fd: observer.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            // SAFETY: `quiet` is a live pollfd on the epoll set owned by `observer`, with a
            // count of 1 and a zero timeout, so the call returns without blocking.
            unsafe { libc::poll(&mut quiet, 1, 0) },
            0,
            "a drained observer must not stay readable"
        );
        assert!(child.wait().expect("waiting for child").success());
    }
}

/// PID 1 mode for `run --init`. The supervisor starts the workflow as a worker
/// process and never runs runtime children itself, so reaping unrelated exits
/// with `waitpid(-1)` here cannot consume a status the runtime is waiting for.
/// It forwards SIGINT, SIGTERM and SIGHUP to the worker, reaps orphans, and
/// returns the worker's status: its exit code, or 128 plus the signal number.
pub fn supervise_init(worker_arguments: &[String]) -> Result<i32> {
    let executable = std::env::current_exe().context("locating the running interpreter")?;
    #[cfg(target_os = "linux")]
    {
        // Orphans of the worker re-parent to this process even when it is not
        // PID 1. Kernels older than 3.4 refuse the request; orphans then go to
        // PID 1 as usual.
        // SAFETY: prctl with these arguments only sets this process's flag.
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    }
    // Flags record delivery; the wake-up pipe interrupts the wait loop. Both are
    // needed because the pipe carries no signal number.
    let forwarded = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP]
        .into_iter()
        .map(|signal| {
            let flag = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(signal, Arc::clone(&flag))
                .context("registering supervisor signal forwarding")?;
            Ok((signal, flag))
        })
        .collect::<Result<Vec<_>>>()?;
    let (wake_read, wake_write) =
        std::os::unix::net::UnixStream::pair().context("creating supervisor wake-up pipe")?;
    wake_read
        .set_nonblocking(true)
        .context("configuring supervisor wake-up pipe")?;
    wake_write
        .set_nonblocking(true)
        .context("configuring supervisor wake-up pipe")?;
    for signal in [libc::SIGCHLD, libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let writer = wake_write
            .try_clone()
            .context("configuring supervisor wake-up pipe")?;
        signal_hook::low_level::pipe::register(signal, writer)
            .context("registering supervisor wake-up")?;
    }

    let worker = std::process::Command::new(&executable)
        .args(worker_arguments)
        .spawn()
        .context("starting the workflow worker")?;
    let worker_pid = worker.id() as libc::pid_t;
    let mut worker_status = None;
    loop {
        drain_wake_pipe(&wake_read);
        if worker_status.is_none() {
            for (signal, flag) in &forwarded {
                if flag.swap(false, Ordering::SeqCst) {
                    // SAFETY: the worker is not reaped yet, so its pid still names it.
                    unsafe { libc::kill(worker_pid, *signal) };
                }
            }
        }
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: the status pointer refers to a valid local.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid == 0 {
                break;
            }
            if pid < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if pid == worker_pid {
                worker_status = Some(status);
            }
        }
        if let Some(status) = worker_status {
            return Ok(supervised_exit_code(status));
        }
        wait_for_supervisor_wakeup(&wake_read)?;
    }
}

fn drain_wake_pipe(pipe: &std::os::unix::net::UnixStream) {
    let mut buffer = [0u8; 64];
    let mut reader = pipe;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

fn wait_for_supervisor_wakeup(pipe: &std::os::unix::net::UnixStream) -> Result<()> {
    // A signal wakes the wait; the caller re-checks its flags.
    match crate::platform::unix_ffi::poll_one(pipe.as_raw_fd(), libc::POLLIN, -1) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
        Err(error) => return Err(error).context("waiting for the supervised workflow"),
    }
    Ok(())
}

fn supervised_exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

#[cfg(all(test, target_os = "linux"))]
mod supervisor_tests {
    use super::*;

    #[test]
    fn signal_and_exit_statuses_map_to_shell_codes() {
        // Status words for exit 5 and for death by SIGTERM (15).
        assert_eq!(supervised_exit_code(5 << 8), 5);
        assert_eq!(supervised_exit_code(15), 128 + 15);
    }
}

/// Exit status for a run ended by an interrupt (128 plus the signal number).
pub fn interrupt_exit_code() -> Option<i32> {
    crate::platform::cancel_exit_code()
}

/// Record the signal that interrupted a run, so `main` can exit with 128 plus it.
/// Intended for handlers that only perform atomic stores.
pub fn record_interrupt(signal: i32) {
    crate::platform::record_cancel_signal(signal);
}

/// Foreground ownership for one `interactively` command. The child leads its own
/// process group and is the terminal's foreground group while this value lives.
/// Dropping it returns the foreground to the parent's group and restores the
/// parent's SIGTTOU disposition, on success and on error.
struct TerminalForeground {
    parent_group: libc::pid_t,
    previous_ttou: libc::sighandler_t,
}

impl TerminalForeground {
    /// Prepare `command` for the handoff. Returns `None` when stdin is not a
    /// terminal; the command then runs like any other native command.
    fn begin(command: &mut std::process::Command) -> Option<Self> {
        // SAFETY: isatty only inspects descriptor 0.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return None;
        }
        // SAFETY: getpgrp has no preconditions.
        let parent_group = unsafe { libc::getpgrp() };
        // A background process group may change the foreground only while
        // SIGTTOU is ignored. The child inherits this until its pre-exec hook
        // restores the default disposition.
        // SAFETY: installing SIG_IGN has no memory safety preconditions.
        let previous_ttou = unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
        // SAFETY: the hook calls only async-signal-safe functions.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1
                    || libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpid()) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                Ok(())
            });
        }
        Some(Self {
            parent_group,
            previous_ttou,
        })
    }

    /// Make the spawned child the terminal's foreground process group.
    fn give(&self, child: u32) -> std::io::Result<()> {
        // SAFETY: tcsetpgrp reads descriptor 0 and the given group id only.
        if unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, child as libc::pid_t) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for TerminalForeground {
    fn drop(&mut self) {
        // SAFETY: as in `begin`. SIGTTOU stays ignored until after the parent
        // has reclaimed the foreground, which a background process needs.
        unsafe {
            libc::tcsetpgrp(libc::STDIN_FILENO, self.parent_group);
            libc::signal(libc::SIGTTOU, self.previous_ttou);
        }
    }
}
