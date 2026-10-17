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
fn kill_group(child: &Child) {
    // Each applet starts its own group. A negative PID addresses descendants
    // that retain that group, without touching the runner or its terminal.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
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
                kill_group(&child.child);
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
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error()).context("setting applet input nonblocking");
    }
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
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ready =
                    unsafe { libc::poll(&mut descriptor, 1, deadline_poll_timeout(deadline, 20)) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(error).context("waiting for applet input");
                    }
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
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error()).context("getting applet stdin pipe flags");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error())
            .context("setting applet stdin pipe nonblocking");
    }

    // Exactly one nonblocking write is attempted. EINTR and WouldBlock leave
    // the unaccepted suffix for the post-spawn writer thread.
    let write_result = pipe_writer.write(&data);
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } == -1 {
        return Err(std::io::Error::last_os_error()).context("restoring applet stdin pipe flags");
    }
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
    pidfds: Mutex<Vec<OwnedFd>>,
}
impl ChildObserver {
    #[cfg(target_os = "macos")]
    fn new() -> Option<Self> {
        let fd = unsafe { libc::kqueue() };
        if fd == -1 {
            return None;
        }
        // Darwin installs close-on-exec and close-on-fork before publishing
        // kqueue descriptors (XNU kqueue creation); no fcntl race or extra
        // descriptor configuration is needed here.
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
            let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
            let no_wait = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            loop {
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
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd == -1 {
            return None;
        }
        Some(Self {
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
            unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
        if raw == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
        let mut event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: raw as u64,
        };
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
            .push(pidfd);
        Ok(())
    }

    fn drain(&self) -> std::io::Result<()> {
        let mut events: [libc::epoll_event; 16] = unsafe { std::mem::zeroed() };
        loop {
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
                    return Ok(());
                }
                return Err(error);
            }
            if ready == 0 {
                return Ok(());
            }
            let mut pidfds = self
                .pidfds
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for event in &events[..ready as usize] {
                // Each registration stores its pidfd number in `u64`.
                let fd = event.u64 as libc::c_int;
                unsafe {
                    libc::epoll_ctl(
                        self.epoll.as_raw_fd(),
                        libc::EPOLL_CTL_DEL,
                        fd,
                        std::ptr::null_mut(),
                    );
                }
                pidfds.retain(|pidfd| pidfd.as_raw_fd() != fd);
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod pidfd_observer_tests {
    use super::*;

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
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 10_000) }, 1);
        observer.drain().expect("draining exit notification");
        let mut quiet = libc::pollfd {
            fd: observer.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut quiet, 1, 0) },
            0,
            "a drained observer must not stay readable"
        );
        assert!(child.wait().expect("waiting for child").success());
    }
}
