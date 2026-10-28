//! Cooperative descriptor writes for CLI output and runtime trace pipes.
use super::{check_deadline, Signals};
use anyhow::{bail, Context, Result};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

// A worker blocked in a kernel write cannot be cancelled. Keep one active worker per output
// target, with a fixed process-wide cap, so one stalled pipe does not block unrelated outputs.
const MAX_ACTIVE_OUTPUT_WRITERS: usize = 16;
static ACTIVE_OUTPUT_TARGETS: Mutex<Vec<OutputTarget>> = Mutex::new(Vec::new());
static OUTPUT_WRITER_POOL: OnceLock<Arc<OutputWriterPoolInner>> = OnceLock::new();

/// A handle to the process-wide pool of blocking output workers. Sharing workers across runs
/// keeps concurrent long-lived runs from accumulating their own idle thread sets.
pub(super) struct OutputWriterPool {
    inner: Arc<OutputWriterPoolInner>,
}

struct OutputWriterPoolInner {
    workers: Mutex<Vec<OutputWorker>>,
}

struct OutputWorker {
    sender: mpsc::SyncSender<WriteRequest>,
    busy: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
}

struct WriteRequest {
    fd: OwnedFd,
    bytes: Vec<u8>,
    stop: Arc<AtomicBool>,
    write_limit: usize,
    may_sigpipe: bool,
    reservation: WriterReservation,
    worker_gate: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    result: Option<mpsc::SyncSender<Result<()>>>,
}

impl OutputWriterPool {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::clone(OUTPUT_WRITER_POOL.get_or_init(|| {
                Arc::new(OutputWriterPoolInner {
                    workers: Mutex::new(Vec::new()),
                })
            })),
        }
    }

    fn submit(
        &self,
        mut request: WriteRequest,
        cancelled: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<mpsc::Receiver<Result<()>>> {
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        request.result = Some(result_sender);
        let mut request = Some(request);
        loop {
            super::check_deadline(deadline)?;
            if cancelled.load(Ordering::Relaxed) {
                bail!("program cancelled by signal");
            }

            let mut workers = self
                .inner
                .workers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            workers.retain(|worker| worker.alive.load(Ordering::Acquire));
            for worker in workers.iter() {
                if worker
                    .busy
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    match worker
                        .sender
                        .try_send(request.take().expect("request is available"))
                    {
                        Ok(()) => return Ok(result_receiver),
                        Err(mpsc::TrySendError::Full(returned)) => {
                            worker.busy.store(false, Ordering::Release);
                            request = Some(returned);
                        }
                        Err(mpsc::TrySendError::Disconnected(returned)) => {
                            worker.busy.store(false, Ordering::Release);
                            request = Some(returned);
                        }
                    }
                }
            }

            if workers.len() < MAX_ACTIVE_OUTPUT_WRITERS {
                let (sender, receiver) = mpsc::sync_channel(1);
                let busy = Arc::new(AtomicBool::new(true));
                let worker_busy = Arc::clone(&busy);
                let alive = Arc::new(AtomicBool::new(true));
                let worker_alive = Arc::clone(&alive);
                std::thread::Builder::new()
                    .name("execute-output-writer".to_string())
                    .spawn(move || output_worker(receiver, worker_busy, worker_alive))
                    .context("starting output writer")?;
                sender
                    .send(request.take().expect("request is available"))
                    .map_err(|_| {
                        anyhow::anyhow!("output writer stopped before accepting output")
                    })?;
                workers.push(OutputWorker {
                    sender,
                    busy,
                    alive,
                });
                return Ok(result_receiver);
            }
            drop(workers);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn output_worker(
    receiver: mpsc::Receiver<WriteRequest>,
    busy: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
) {
    while let Ok(request) = receiver.recv() {
        let WriteRequest {
            fd,
            bytes,
            stop,
            write_limit,
            may_sigpipe,
            reservation,
            worker_gate,
            result,
        } = request;
        let result_value =
            blocking_write_all(fd, &bytes, &stop, write_limit, may_sigpipe, worker_gate);
        drop(reservation);
        busy.store(false, Ordering::Release);
        let _ = result
            .expect("submitted output requests have a result sender")
            .send(result_value);
    }
    alive.store(false, Ordering::Release);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OutputTarget {
    device: libc::dev_t,
    inode: libc::ino_t,
    file_type: libc::mode_t,
    special_device: libc::dev_t,
}

struct WriterReservation(OutputTarget);
impl Drop for WriterReservation {
    fn drop(&mut self) {
        let mut active = ACTIVE_OUTPUT_TARGETS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(position) = active.iter().position(|target| *target == self.0) {
            active.swap_remove(position);
        }
    }
}

fn reserve_output_target(
    target: OutputTarget,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<WriterReservation> {
    loop {
        super::check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) {
            bail!("program cancelled by signal");
        }
        let mut active = ACTIVE_OUTPUT_TARGETS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let wait_for_target = output_target_is_reserved(&active, target)?;
        if !wait_for_target {
            active.push(target);
            return Ok(WriterReservation(target));
        }
        drop(active);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Return true when this target is already reserved. A new target at the worker cap fails
/// immediately, so a deadline-free write cannot wait forever behind unrelated stalled targets.
fn output_target_is_reserved(active: &[OutputTarget], target: OutputTarget) -> Result<bool> {
    if active.contains(&target) {
        return Ok(true);
    }
    if active.len() >= MAX_ACTIVE_OUTPUT_WRITERS {
        bail!("output writer limit ({MAX_ACTIVE_OUTPUT_WRITERS}) reached by other active targets");
    }
    Ok(false)
}

fn output_target_and_write_limit(fd: RawFd) -> Result<(OutputTarget, usize, bool)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat is writable storage for one libc::stat, and fstat initializes it on success.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error()).context("inspecting output descriptor");
    }
    // SAFETY: fstat returned 0 above, so it initialized stat.
    let stat = unsafe { stat.assume_init() };
    let file_type = stat.st_mode & libc::S_IFMT;
    let target = OutputTarget {
        device: stat.st_dev,
        inode: stat.st_ino,
        file_type,
        special_device: stat.st_rdev,
    };
    let may_sigpipe = file_type == libc::S_IFIFO || file_type == libc::S_IFSOCK;
    let write_limit = if may_sigpipe {
        libc::PIPE_BUF
    } else {
        64 * 1024
    };
    Ok((target, write_limit, may_sigpipe))
}

/// Write bytes without changing flags on inherited descriptors. A bounded per-target worker owns
/// the blocking write while this caller checks cancellation and deadlines; the worker checks the
/// stop flag between bounded chunks.
pub fn write_cancellable(fd: RawFd, bytes: &[u8], cancelled: &AtomicBool) -> Result<()> {
    write_with_deadline(fd, bytes, cancelled, None)
}
pub fn write_with_deadline(
    fd: RawFd,
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    let pool = OutputWriterPool::new();
    write_with_deadline_inner_with_pool(&pool, fd, bytes, cancelled, deadline, None)
}

#[cfg(test)]
fn write_with_deadline_inner(
    fd: RawFd,
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
    worker_gate: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
) -> Result<()> {
    let pool = OutputWriterPool::new();
    write_with_deadline_inner_with_pool(&pool, fd, bytes, cancelled, deadline, worker_gate)
}

#[cfg(test)]
pub(super) fn write_with_deadline_reusing(
    pool: &OutputWriterPool,
    fd: RawFd,
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    write_with_deadline_inner_with_pool(pool, fd, bytes, cancelled, deadline, None)
}

fn write_with_deadline_inner_with_pool(
    pool: &OutputWriterPool,
    fd: RawFd,
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
    worker_gate: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
) -> Result<()> {
    if bytes.is_empty() {
        super::check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) {
            bail!("program cancelled by signal");
        }
        return Ok(());
    }
    // Under GitHub Actions hardening, terminal-bound bytes are neutralized at
    // the single choke point all runtime terminal writes share, so streamed
    // step output, redirected stdout-to-stderr, trace events and diagnostics
    // are all covered without touching framed protocols or file targets.
    // `check --format github` intentionally emits workflow commands and never
    // enables hardening, so it stays raw. Hardened fd 1/2 writes hold the
    // merged-sink lock across filter and worker completion so classification
    // matches landing order in the merged runner log; file targets stay
    // concurrent.
    let hardened_terminal = crate::github_actions::enabled() && (fd == 1 || fd == 2);
    let _sink = hardened_terminal.then(crate::github_actions::terminal_sink_lock);
    let owned = if crate::github_actions::enabled() {
        if fd == 1 {
            crate::github_actions::filter_stdout(bytes)
        } else if fd == 2 {
            crate::github_actions::filter_stderr(bytes)
        } else {
            bytes.to_vec()
        }
    } else {
        bytes.to_vec()
    };
    // A split `::` held back by the streaming sanitizer must not be lost when
    // the write is the last of its stream in this call; flushing is harmless
    // when nothing is pending. Flushing per write keeps each write self
    // contained; cross-write line state is still preserved for the next call.
    // Note: flushing here would emit a pending `:` immediately, breaking the
    // split-`::` protection across writes. Only flush when the caller signals
    // end-of-stream (final output paths do this explicitly); live streams keep
    // the pending colon for the next write.
    write_with_deadline_owned_inner(
        pool,
        fd,
        owned,
        cancelled,
        deadline,
        worker_gate,
        hardened_terminal,
    )
}

pub(super) fn write_with_deadline_owned_reusing(
    pool: &OutputWriterPool,
    fd: RawFd,
    bytes: Vec<u8>,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    if bytes.is_empty() {
        super::check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) {
            bail!("program cancelled by signal");
        }
        return Ok(());
    }
    // Same terminal choke point as the borrowing path: owned streamed writes
    // (the hot path for step output) are neutralized before reaching the fd.
    // Hardened fd 1/2 writes hold the merged-sink lock across filter and
    // worker completion, matching the borrowing path above.
    let hardened_terminal = crate::github_actions::enabled() && (fd == 1 || fd == 2);
    let _sink = hardened_terminal.then(crate::github_actions::terminal_sink_lock);
    let filtered = if crate::github_actions::enabled() {
        if fd == 1 {
            crate::github_actions::filter_stdout(&bytes)
        } else if fd == 2 {
            crate::github_actions::filter_stderr(&bytes)
        } else {
            bytes
        }
    } else {
        bytes
    };
    // A pending split `:` stays buffered across live writes; only an empty
    // filtered write (a lone held colon) skips the syscall and keeps the state
    // for the next chunk. End-of-stream callers flush explicitly.
    if filtered.is_empty() {
        super::check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) {
            bail!("program cancelled by signal");
        }
        return Ok(());
    }
    write_with_deadline_owned_inner(
        pool,
        fd,
        filtered,
        cancelled,
        deadline,
        None,
        hardened_terminal,
    )
}

fn write_with_deadline_owned_inner(
    pool: &OutputWriterPool,
    fd: RawFd,
    data: Vec<u8>,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
    worker_gate: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    terminal_serialized: bool,
) -> Result<()> {
    let (target, write_limit, may_sigpipe) = output_target_and_write_limit(fd)?;
    let reservation = reserve_output_target(target, cancelled, deadline)?;
    let worker_fd = crate::platform::unix_ffi::duplicate_cloexec(fd, 3)
        .context("duplicating output descriptor")?;
    let stop = Arc::new(AtomicBool::new(false));
    let receiver = pool.submit(
        WriteRequest {
            fd: worker_fd,
            bytes: data,
            stop: Arc::clone(&stop),
            write_limit,
            may_sigpipe,
            reservation,
            worker_gate,
            result: None,
        },
        cancelled,
        deadline,
    )?;

    // When `terminal_serialized` the caller holds the merged-sink lock across
    // this wait. A cancel or deadline must not release it while the worker is
    // still in flight: the next filter would then classify before the prior
    // bytes land, re-opening the reorder forgery. Fail closed by signalling
    // `stop`, waiting for the worker to finish (it checks `stop` between
    // bounded chunks), and only then reporting the cancellation.
    let wait_for_worker_then = |result: Result<()>| -> Result<()> {
        if !terminal_serialized {
            return result;
        }
        match receiver.recv() {
            Ok(worker_result) => {
                let _ = worker_result;
                result
            }
            Err(_) => result,
        }
    };
    loop {
        if cancelled.load(Ordering::Relaxed) {
            stop.store(true, Ordering::Release);
            return wait_for_worker_then(Err(anyhow::anyhow!("program cancelled by signal")));
        }
        if let Err(error) = super::check_deadline(deadline) {
            stop.store(true, Ordering::Release);
            return wait_for_worker_then(Err(error));
        }
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(result) => {
                super::check_deadline(deadline)?;
                if cancelled.load(Ordering::Relaxed) {
                    stop.store(true, Ordering::Release);
                    return wait_for_worker_then(Err(anyhow::anyhow!(
                        "program cancelled by signal"
                    )));
                }
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("output writer stopped without returning a result");
            }
        }
    }
}

/// Write in bounded chunks without changing the open-file description's status flags. If the
/// inherited descriptor was already nonblocking, poll only after EAGAIN; blocking descriptors
/// write directly, so no readiness-to-write race can hang the caller (the caller waits elsewhere).
fn blocking_write_all(
    fd: OwnedFd,
    bytes: &[u8],
    stop: &AtomicBool,
    write_limit: usize,
    may_sigpipe: bool,
    mut worker_gate: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
) -> Result<()> {
    let raw_fd = fd.as_raw_fd();
    #[cfg(target_os = "linux")]
    let mut signal_mask = if may_sigpipe {
        Some(LinuxSigpipeMask::block()?)
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let _ = may_sigpipe;

    let mut generated_sigpipe = false;
    let result = (|| {
        let mut offset = 0;
        while offset < bytes.len() {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let length = (bytes.len() - offset).min(write_limit);
            // In tests, hold the worker immediately after its stop check and before the syscall.
            // Releasing this gate models a blocking syscall that is already in flight: cancellation
            // may return to the caller, but that one bounded syscall can still finish.
            if let Some((ready, release)) = worker_gate.take() {
                let _ = ready.send(());
                let _ = release.recv_timeout(Duration::from_secs(3));
            }
            // SAFETY: offset < bytes.len() and length is at most the remaining slice. fd is owned
            // by this worker for the duration of the call; no status flags are changed.
            let count = unsafe { libc::write(raw_fd, bytes[offset..].as_ptr().cast(), length) };
            if count > 0 {
                offset += count as usize;
                continue;
            }
            if count == 0 {
                bail!("writing output made no progress");
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::BrokenPipe {
                generated_sigpipe = true;
            }
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    match crate::platform::unix_ffi::poll_one(raw_fd, libc::POLLOUT, 20) {
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            return Err(error).context("waiting for output to become writable")
                        }
                    }
                }
                _ => return Err(error).context("writing output"),
            }
        }
        Ok(())
    })();
    #[cfg(target_os = "linux")]
    if let Some(signal_mask) = &mut signal_mask {
        signal_mask.finish(generated_sigpipe)?;
    }
    result
}

/// Block SIGPIPE on one Linux worker while it writes to a pipe or socket. A failed `write(2)`
/// queues SIGPIPE for the calling thread; consume only a newly generated signal when the process
/// uses the default action, then restore the worker's exact prior mask.
#[cfg(target_os = "linux")]
struct LinuxSigpipeMask {
    old_mask: libc::sigset_t,
    sigpipe_set: libc::sigset_t,
    already_pending: bool,
    active: bool,
}

#[cfg(target_os = "linux")]
impl LinuxSigpipeMask {
    fn block() -> io::Result<Self> {
        let mut sigpipe_set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigpipe_set is writable storage for one sigset_t.
        if unsafe { libc::sigemptyset(sigpipe_set.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sigemptyset initialized the set and SIGPIPE is valid.
        if unsafe { libc::sigaddset(sigpipe_set.as_mut_ptr(), libc::SIGPIPE) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut old_mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigpipe_set is initialized, old_mask is writable, and pthread_sigmask returns
        // its error number directly.
        let error = unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, sigpipe_set.as_ptr(), old_mask.as_mut_ptr())
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }

        // SAFETY: pthread_sigmask initialized old_mask on success; sigpipe_set was initialized.
        let mut guard = Self {
            // SAFETY: the successful pthread_sigmask call above initialized old_mask.
            old_mask: unsafe { old_mask.assume_init() },
            // SAFETY: the successful sigaddset call above initialized sigpipe_set.
            sigpipe_set: unsafe { sigpipe_set.assume_init() },
            already_pending: false,
            active: true,
        };
        let mut pending = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: pending is writable storage for sigpending.
        if unsafe { libc::sigpending(pending.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sigpending initialized pending; sigismember only reads it.
        let member = unsafe { libc::sigismember(pending.as_ptr(), libc::SIGPIPE) };
        if member < 0 {
            return Err(io::Error::last_os_error());
        }
        guard.already_pending = member == 1;
        Ok(guard)
    }

    fn finish(&mut self, broken_pipe: bool) -> io::Result<()> {
        if broken_pipe && !self.already_pending {
            let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
            // SAFETY: new_action is null and action is writable storage for sigaction.
            if unsafe { libc::sigaction(libc::SIGPIPE, std::ptr::null(), action.as_mut_ptr()) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: sigaction initialized action.
            if unsafe { action.assume_init() }.sa_sigaction == libc::SIG_DFL {
                let mut pending = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
                // SAFETY: pending is writable storage for sigpending.
                if unsafe { libc::sigpending(pending.as_mut_ptr()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: sigpending initialized pending; sigismember only reads it.
                let member = unsafe { libc::sigismember(pending.as_ptr(), libc::SIGPIPE) };
                if member < 0 {
                    return Err(io::Error::last_os_error());
                }
                if member == 1 {
                    let timeout = libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    };
                    // SAFETY: the set contains only blocked SIGPIPE, no SIGPIPE was pending
                    // before this request, and EPIPE generated this pending signal. The zero
                    // timeout avoids waiting if another thread consumed a process-pending signal.
                    loop {
                        // SAFETY: the set contains only blocked SIGPIPE, no SIGPIPE was pending
                        // before this request, and EPIPE generated this pending signal. The zero
                        // timeout avoids waiting if another thread consumed a process-pending
                        // signal.
                        let received = unsafe {
                            libc::sigtimedwait(&self.sigpipe_set, std::ptr::null_mut(), &timeout)
                        };
                        if received == libc::SIGPIPE {
                            break;
                        }
                        let error = io::Error::last_os_error();
                        if received == -1 && error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        if received == -1 && error.kind() == io::ErrorKind::WouldBlock {
                            break;
                        }
                        return Err(if received == -1 {
                            error
                        } else {
                            io::Error::other("sigtimedwait returned an unexpected signal")
                        });
                    }
                }
            }
        }
        self.restore()
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        // SAFETY: old_mask was initialized by the successful pthread_sigmask call above.
        let error = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.old_mask, std::ptr::null_mut())
        };
        self.active = false;
        if error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(error))
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxSigpipeMask {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn poll_timeout_ms(deadline: Instant, now: Instant) -> libc::c_int {
    const MAX_WAIT_MS: u128 = 20;
    let remaining = deadline.saturating_duration_since(now).as_nanos();
    let milliseconds = remaining / 1_000_000;
    milliseconds.min(MAX_WAIT_MS) as libc::c_int
}

/// Final diagnostics are bounded so shutdown cannot wait forever for a full or abandoned pipe.
/// The exit status is already non-zero, so a failed write leaves nothing else to report.
pub fn write_diagnostic(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let deadline = Instant::now() + DIAGNOSTIC_MAX_WAIT;
    let _ = write_with_deadline(
        libc::STDERR_FILENO,
        bytes,
        &AtomicBool::new(false),
        Some(deadline),
    );
}

/// Maximum time a final diagnostic waits for stderr.
const DIAGNOSTIC_MAX_WAIT: Duration = Duration::from_secs(2);

/// The size of stdin when it is a regular file, which can be read without polling, or
/// `None` for a pipe, terminal or socket, or when fstat fails.
fn stdin_regular_file_size() -> Option<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` is writable storage for one libc::stat, and STDIN_FILENO is a plain
    // descriptor number; fstat writes the struct only when it returns 0.
    if unsafe { libc::fstat(libc::STDIN_FILENO, stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: fstat returned 0 above, so it initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    (stat.st_mode & libc::S_IFMT == libc::S_IFREG).then(|| u64::try_from(stat.st_size).unwrap_or(0))
}

/// Read inherited stdin without changing its shared open-file-description flags.
/// Poll before each read so a quiet pipe remains cancellable and deadline-bound.
pub(super) fn capture_stdin(
    limit: usize,
    signals: &Signals,
    timeout: Option<Duration>,
) -> Result<Vec<u8>> {
    let deadline = timeout
        .map(|duration| {
            Instant::now()
                .checked_add(duration)
                .context("stdin timeout is too large")
        })
        .transpose()?;
    let regular_file_size = stdin_regular_file_size();
    let regular_file = regular_file_size.is_some();
    let initial_capacity = regular_file_size
        .map_or(0, |size| usize::try_from(size).unwrap_or(0))
        .min(limit);
    let mut output = Vec::with_capacity(initial_capacity);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        check_deadline(deadline)?;
        if !regular_file {
            let wait = deadline.map_or(20, |end| poll_timeout_ms(end, Instant::now()));
            // wait is 20 or poll_timeout_ms(), which lies in 0..=20, so it is a valid bounded timeout.
            let (ready, revents) =
                match crate::platform::unix_ffi::poll_one(libc::STDIN_FILENO, libc::POLLIN, wait) {
                    Ok(result) => result,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error).context("waiting for stdin"),
                };
            if ready == 0 {
                continue;
            }
            if signals.cancelled.load(Ordering::Relaxed) {
                bail!("workflow cancelled by signal");
            }
            check_deadline(deadline)?;
            if revents & libc::POLLNVAL != 0 {
                bail!("stdin is not an open file descriptor");
            }
        }
        let count =
            // SAFETY: chunk is a live local array, and chunk.len() is the length passed, so the
            // kernel writes at most that many bytes. POLLNVAL was rejected above, and poll just
            // reported readiness, so the read should return without waiting.
            unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return Err(error).context("reading stdin");
        }
        if count == 0 {
            return Ok(output);
        }
        let count = count as usize;
        if count > limit.saturating_sub(output.len()) {
            bail!("stdin exceeds maximum of {limit} bytes");
        }
        output.extend_from_slice(&chunk[..count]);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};

    fn run_isolated_test(test_name: &str, child_mode: &str, body: impl FnOnce()) {
        if std::env::var_os(child_mode).is_some() {
            body();
            return;
        }
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .env(child_mode, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "isolated test process failed: {status}");
    }

    #[test]
    fn poll_wait_is_bounded_by_the_command_deadline() {
        let now = Instant::now();
        assert_eq!(poll_timeout_ms(now + Duration::from_millis(3), now), 3);
        assert_eq!(poll_timeout_ms(now + Duration::from_micros(1), now), 0);
        assert_eq!(poll_timeout_ms(now, now), 0);
        assert_eq!(poll_timeout_ms(now + Duration::from_secs(1), now), 20);
    }

    #[test]
    fn empty_write_still_observes_cancellation_and_deadline() {
        assert!(write_with_deadline(-1, &[], &AtomicBool::new(true), None).is_err());
        let error = write_with_deadline(-1, &[], &AtomicBool::new(false), Some(Instant::now()))
            .unwrap_err();
        assert!(error.is::<super::super::CommandTimedOut>());
        write_with_deadline(-1, &[], &AtomicBool::new(false), None).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sigpipe_mask_preserves_default_action_pending_signal_and_inflight_teardown() {
        const CHILD_MODE: &str = "EXECUTE_LINUX_SIGPIPE_CHILD";
        if let Some(mode) = std::env::var_os(CHILD_MODE) {
            // Isolate the process-wide default disposition to this child. The production writer
            // must turn EPIPE into an ordinary error without changing this action.
            // SAFETY: sigaction is a C struct whose all-zero representation is valid storage; all
            // fields used below are initialized before it is installed.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = libc::SIG_DFL;
            // SAFETY: action.sa_mask is writable storage for sigemptyset.
            assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
            assert_eq!(
                // SAFETY: action is fully initialized for SIGPIPE and the old-action pointer is null.
                unsafe { libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut()) },
                0
            );

            match mode.to_string_lossy().as_ref() {
                "closed" => {
                    let (reader, writer) = test_pipe();
                    drop(reader);
                    let error = write_with_deadline(
                        writer.as_raw_fd(),
                        b"closed pipe",
                        &AtomicBool::new(false),
                        Some(Instant::now() + Duration::from_secs(1)),
                    )
                    .unwrap_err();
                    assert!(error.chain().any(|cause| {
                        cause
                            .downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
                    }));

                    let (reader, writer) = test_pipe();
                    let mut reader = std::fs::File::from(reader);
                    write_with_deadline(
                        writer.as_raw_fd(),
                        b"open pipe",
                        &AtomicBool::new(false),
                        Some(Instant::now() + Duration::from_secs(1)),
                    )
                    .unwrap();
                    drop(writer);
                    let mut output = Vec::new();
                    reader.read_to_end(&mut output).unwrap();
                    assert_eq!(output, b"open pipe");
                }
                "pending" => {
                    let mut sigpipe_set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
                    // SAFETY: sigpipe_set is writable storage for one sigset_t.
                    assert_eq!(unsafe { libc::sigemptyset(sigpipe_set.as_mut_ptr()) }, 0);
                    assert_eq!(
                        // SAFETY: sigemptyset initialized sigpipe_set, and SIGPIPE is valid.
                        unsafe { libc::sigaddset(sigpipe_set.as_mut_ptr(), libc::SIGPIPE) },
                        0
                    );
                    // SAFETY: the successful sigaddset call above initialized sigpipe_set.
                    let sigpipe_set = unsafe { sigpipe_set.assume_init() };
                    let mut original_mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
                    assert_eq!(
                        // SAFETY: sigpipe_set is initialized, original_mask is writable, and
                        // pthread_sigmask returns its error number directly.
                        unsafe {
                            libc::pthread_sigmask(
                                libc::SIG_BLOCK,
                                &sigpipe_set,
                                original_mask.as_mut_ptr(),
                            )
                        },
                        0
                    );
                    // SAFETY: the successful pthread_sigmask call above initialized original_mask.
                    let original_mask = unsafe { original_mask.assume_init() };
                    // SAFETY: raise targets only this thread; SIGPIPE is blocked above.
                    assert_eq!(unsafe { libc::raise(libc::SIGPIPE) }, 0);

                    let (reader, writer) = test_pipe();
                    drop(reader);
                    let error = blocking_write_all(
                        writer,
                        b"closed pipe with an older pending SIGPIPE",
                        &AtomicBool::new(false),
                        libc::PIPE_BUF,
                        true,
                        None,
                    )
                    .unwrap_err();
                    assert!(error.chain().any(|cause| {
                        cause
                            .downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
                    }));

                    let mut pending = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
                    // SAFETY: pending is writable storage for sigpending.
                    assert_eq!(unsafe { libc::sigpending(pending.as_mut_ptr()) }, 0);
                    // SAFETY: the successful sigpending call above initialized pending.
                    let pending = unsafe { pending.assume_init() };
                    // SAFETY: pending is initialized and sigismember only reads it.
                    assert_eq!(unsafe { libc::sigismember(&pending, libc::SIGPIPE) }, 1);
                    let mut received = 0;
                    // SAFETY: sigpipe_set is initialized and SIGPIPE remains blocked on this
                    // thread until the original mask is restored below.
                    assert_eq!(unsafe { libc::sigwait(&sigpipe_set, &mut received) }, 0);
                    assert_eq!(received, libc::SIGPIPE);
                    assert_eq!(
                        // SAFETY: original_mask was initialized by pthread_sigmask above; the null
                        // output pointer is permitted when the previous mask is not needed.
                        unsafe {
                            libc::pthread_sigmask(
                                libc::SIG_SETMASK,
                                &original_mask,
                                std::ptr::null_mut(),
                            )
                        },
                        0
                    );
                }
                "cancelled_inflight" => {
                    let (reader, writer) = test_pipe();
                    let writer_fd = writer.as_raw_fd();
                    // SAFETY: writer_fd is open and F_GETFL only reads its status flags.
                    let original_flags = unsafe { libc::fcntl(writer_fd, libc::F_GETFL) };
                    assert_ne!(original_flags, -1);
                    assert_eq!(
                        // SAFETY: writer_fd is open and original_flags is the value returned by
                        // F_GETFL above.
                        unsafe {
                            libc::fcntl(writer_fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK)
                        },
                        0
                    );
                    let filler = [b'f'; 4096];
                    loop {
                        let count =
                            // SAFETY: filler is live storage, writer_fd is open and nonblocking,
                            // and the pipe reader remains open during this fill loop.
                            unsafe { libc::write(writer_fd, filler.as_ptr().cast(), filler.len()) };
                        if count > 0 {
                            continue;
                        }
                        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                        break;
                    }
                    assert_eq!(
                        // SAFETY: writer_fd remains open and original_flags came from F_GETFL above.
                        unsafe { libc::fcntl(writer_fd, libc::F_SETFL, original_flags) },
                        0
                    );

                    let pool = Arc::new(OutputWriterPool::new());
                    let cancelled = Arc::new(AtomicBool::new(false));
                    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
                    let (release_sender, release_receiver) = mpsc::sync_channel(1);
                    let (result_sender, result_receiver) = mpsc::sync_channel(1);
                    let writer_pool = Arc::clone(&pool);
                    let writer_cancelled = Arc::clone(&cancelled);
                    let caller = std::thread::spawn(move || {
                        let result = write_with_deadline_inner_with_pool(
                            &writer_pool,
                            writer_fd,
                            &vec![b'x'; 1024 * 1024],
                            &writer_cancelled,
                            Some(Instant::now() + Duration::from_secs(3)),
                            Some((ready_sender, release_receiver)),
                        );
                        let _ = result_sender.send(result);
                    });
                    ready_receiver
                        .recv_timeout(Duration::from_secs(2))
                        .expect("worker did not reach its pre-syscall gate");
                    cancelled.store(true, Ordering::Release);
                    let cancelled_result = result_receiver
                        .recv_timeout(Duration::from_secs(2))
                        .expect("caller did not return after cancellation")
                        .unwrap_err();
                    assert!(cancelled_result.to_string().contains("cancelled"));
                    drop(reader);
                    release_sender.send(()).unwrap();

                    // The target reservation keeps this probe behind the late in-flight write.
                    // Both writes see EPIPE after the reader is closed; neither may signal-kill
                    // this process under SIG_DFL.
                    let probe_error = write_with_deadline_reusing(
                        &pool,
                        writer_fd,
                        b"probe",
                        &AtomicBool::new(false),
                        Some(Instant::now() + Duration::from_secs(2)),
                    )
                    .unwrap_err();
                    assert!(probe_error.chain().any(|cause| {
                        cause
                            .downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
                    }));
                    caller.join().unwrap();
                }
                other => panic!("unknown child mode {other}"),
            }

            // SAFETY: sigaction is a C struct whose all-zero representation is valid writable
            // storage for the query below.
            let mut observed: libc::sigaction = unsafe { std::mem::zeroed() };
            assert_eq!(
                // SAFETY: new_action is null and observed is writable output storage for sigaction.
                unsafe { libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut observed) },
                0
            );
            assert_eq!(observed.sa_sigaction, libc::SIG_DFL);
            return;
        }

        for mode in ["closed", "pending", "cancelled_inflight"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("runtime::stdio::tests::linux_sigpipe_mask_preserves_default_action_pending_signal_and_inflight_teardown")
                .env(CHILD_MODE, mode)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(
                status.success(),
                "SIGPIPE child mode {mode} failed: {status}"
            );
        }
    }

    #[test]
    fn independent_run_pools_share_the_bounded_process_pool() {
        let (reader, writer) = test_pipe();
        let mut reader = std::fs::File::from(reader);
        let pool = OutputWriterPool::new();
        let next_run_pool = OutputWriterPool::new();
        assert!(Arc::ptr_eq(&pool.inner, &next_run_pool.inner));
        let cancelled = AtomicBool::new(false);
        write_with_deadline_inner_with_pool(
            &next_run_pool,
            writer.as_raw_fd(),
            b"first",
            &cancelled,
            Some(Instant::now() + Duration::from_secs(1)),
            None,
        )
        .unwrap();
        write_with_deadline_inner_with_pool(
            &pool,
            writer.as_raw_fd(),
            b" second",
            &cancelled,
            Some(Instant::now() + Duration::from_secs(1)),
            None,
        )
        .unwrap();
        assert!(pool.inner.workers.lock().unwrap().len() <= MAX_ACTIVE_OUTPUT_WRITERS);
        drop(writer);
        let mut output = String::new();
        reader.read_to_string(&mut output).unwrap();
        assert_eq!(output, "first second");
    }

    #[test]
    fn pooled_writes_preserve_regular_file_output() {
        use std::io::{Seek, SeekFrom};

        let mut file = tempfile::tempfile().unwrap();
        let pool = OutputWriterPool::new();
        let cancelled = AtomicBool::new(false);
        for bytes in [b"first".as_slice(), b" second".as_slice()] {
            write_with_deadline_reusing(
                &pool,
                file.as_raw_fd(),
                bytes,
                &cancelled,
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .unwrap();
        }
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut output = String::new();
        file.read_to_string(&mut output).unwrap();
        assert_eq!(output, "first second");
        assert!(pool.inner.workers.lock().unwrap().len() <= MAX_ACTIVE_OUTPUT_WRITERS);
    }

    #[test]
    fn pooled_same_target_writes_are_serialized() {
        run_isolated_test(
            "runtime::stdio::tests::pooled_same_target_writes_are_serialized",
            "EXECUTE_TEST_POOLED_SAME_TARGET",
            || {
                let (reader, writer) = test_pipe();
                let pool = Arc::new(OutputWriterPool::new());
                let fd = writer.as_raw_fd();
                let output = std::thread::scope(|scope| {
                    let reader = std::fs::File::from(reader);
                    let reader_thread = scope.spawn(move || {
                        let mut output = Vec::new();
                        reader
                            .take((libc::PIPE_BUF as u64) * 5)
                            .read_to_end(&mut output)
                            .unwrap();
                        output
                    });
                    let barrier = Arc::new(std::sync::Barrier::new(3));
                    let payload_len = libc::PIPE_BUF * 2;
                    let writes = [(b'a', payload_len), (b'b', payload_len)]
                        .into_iter()
                        .map(|(byte, count)| {
                            let barrier = Arc::clone(&barrier);
                            let pool = Arc::clone(&pool);
                            scope.spawn(move || {
                                barrier.wait();
                                let bytes = vec![byte; count];
                                write_with_deadline_reusing(
                                    &pool,
                                    fd,
                                    &bytes,
                                    &AtomicBool::new(false),
                                    Some(Instant::now() + Duration::from_secs(1)),
                                )
                                .unwrap();
                            })
                        })
                        .collect::<Vec<_>>();
                    barrier.wait();
                    for write in writes {
                        write.join().unwrap();
                    }
                    drop(writer);
                    reader_thread.join().unwrap()
                });
                let payload_len = libc::PIPE_BUF * 2;
                let alpha_then_beta = output.len() == payload_len * 2
                    && output[..payload_len].iter().all(|byte| *byte == b'a')
                    && output[payload_len..].iter().all(|byte| *byte == b'b');
                let beta_then_alpha = output.len() == payload_len * 2
                    && output[..payload_len].iter().all(|byte| *byte == b'b')
                    && output[payload_len..].iter().all(|byte| *byte == b'a');
                assert!(alpha_then_beta || beta_then_alpha, "writes interleaved");
            },
        );
    }

    #[test]
    fn blocked_write_observes_cancellation_without_reader() {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors is a two-element c_int array, which pipe() fills on success.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: pipe() returned this read descriptor, and it has exactly one owner here.
        let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: pipe() returned this write descriptor, and it has exactly one owner here.
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let fd = writer.as_raw_fd();
        // Fill the pipe nonblocking, then restore its flags before starting the runtime writer.
        // SAFETY: fd is writer's open descriptor and F_GETFL only reads its status flags.
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(original_flags, -1);
        assert_eq!(
            // SAFETY: fd remains open and F_SETFL takes flags by value without reading pointers.
            unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) },
            0
        );
        let filler = [b'f'; 4096];
        loop {
            // SAFETY: filler is a live array and fd is the open pipe writer.
            let count = unsafe { libc::write(fd, filler.as_ptr().cast(), filler.len()) };
            if count > 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            break;
        }
        // SAFETY: fd remains open and F_SETFL takes flags by value without reading pointers.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags) }, 0);

        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let pool = Arc::new(OutputWriterPool::new());
        let worker_pool = Arc::clone(&pool);
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
        let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
        let writer_thread = std::thread::spawn(move || {
            let result = write_with_deadline_inner_with_pool(
                &worker_pool,
                fd,
                &[b'x'; 1024 * 1024],
                worker_cancelled.as_ref(),
                Some(Instant::now() + Duration::from_secs(5)),
                Some((ready_sender, release_receiver)),
            );
            let _ = result_sender.send(result);
        });
        let worker_started = ready_receiver.recv_timeout(Duration::from_secs(3)).is_ok();
        cancelled.store(true, Ordering::Release);
        let result = result_receiver.recv_timeout(Duration::from_secs(3));
        // Cancel while the test seam is holding the worker between the stop check and its first
        // syscall. Releasing it forces one syscall attempt despite cancellation; it blocks on the
        // full pipe until the reader below opens space.
        let _ = release_sender.send(());
        let _ = writer_thread.join();
        assert!(
            worker_started,
            "worker did not reach the blocked-write barrier"
        );
        let error = result
            .expect("blocked writer ignored cancellation watchdog")
            .unwrap_err();
        assert!(!error.is::<super::super::CommandTimedOut>());
        assert!(error.to_string().contains("cancelled"));

        // The caller returned while the test-controlled syscall attempt was held. Once the reader
        // opens space, the one already-authorized write may finish at most one bounded chunk; the
        // worker sees the stop flag before issuing another write. A same-target probe below
        // verifies that the original worker released its target reservation.
        let mut reader = std::fs::File::from(reader);
        let reader_fd = reader.as_raw_fd();
        // SAFETY: reader_fd is live and F_GETFL only reads its flags.
        let reader_flags = unsafe { libc::fcntl(reader_fd, libc::F_GETFL) };
        assert_ne!(reader_flags, -1);
        let set_reader_flags = {
            // SAFETY: reader_fd remains open and F_SETFL receives flags by value.
            unsafe { libc::fcntl(reader_fd, libc::F_SETFL, reader_flags | libc::O_NONBLOCK) }
        };
        assert_eq!(set_reader_flags, 0);
        let mut late_output = Vec::new();
        let mut buffer = [0u8; 8192];
        let probe_pool = Arc::new(OutputWriterPool::new());
        let (probe_sender, probe_receiver) = mpsc::sync_channel(1);
        let probe_fd = writer.as_raw_fd();
        let probe_caller = std::thread::spawn(move || {
            let result = write_with_deadline_reusing(
                &probe_pool,
                probe_fd,
                b"probe",
                &AtomicBool::new(false),
                Some(Instant::now() + Duration::from_secs(2)),
            );
            let _ = probe_sender.send(result);
        });
        let mut completed_probe = None;
        let worker_deadline = Instant::now() + Duration::from_secs(2);
        while completed_probe.is_none() && Instant::now() < worker_deadline {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => late_output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("reading released pipe failed: {error}"),
            }
            completed_probe = probe_receiver.try_recv().ok();
        }
        completed_probe
            .expect("same-target write remained blocked behind the cancelled worker")
            .expect("same-target probe write failed after cancellation");
        probe_caller.join().unwrap();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => late_output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("draining completed pipe failed: {error}"),
            }
        }
        drop(writer);
        let late_bytes = late_output.iter().filter(|byte| **byte == b'x').count();
        assert!(
            late_bytes > 0,
            "the released write attempt produced no late bytes"
        );
        assert!(late_bytes <= libc::PIPE_BUF);
        assert!(late_output.windows(5).any(|window| window == b"probe"));
    }

    #[test]
    fn concurrent_writes_keep_deadlines_and_preserve_flags() {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors is a 2-element c_int array, which is the size pipe() writes. The
        // assert on this line checks the result before either descriptor is used.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: descriptors[0] is the read end from that successful pipe() call. This OwnedFd
        // is its only owner, so the descriptor is closed exactly once, when _reader drops.
        let _reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: descriptors[1] is the write end from the same pipe() call, and no other owner
        // wraps it, so OwnedFd closes it exactly once.
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let fd = writer.as_raw_fd();
        // SAFETY: fd is writer's raw descriptor. writer lives until the end of this test, so
        // F_GETFL reads an open descriptor and only reads flags.
        let initial = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        std::thread::scope(|scope| {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
            let jobs = (0..2)
                .map(|_| {
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        write_with_deadline(
                            fd,
                            &vec![0; 4 * 1024 * 1024],
                            &AtomicBool::new(false),
                            Some(Instant::now() + Duration::from_millis(100)),
                        )
                    })
                })
                .collect::<Vec<_>>();
            barrier.wait();
            for job in jobs {
                assert!(job
                    .join()
                    .unwrap()
                    .unwrap_err()
                    .is::<super::super::CommandTimedOut>());
            }
        });
        assert_eq!(
            // SAFETY: fd is still writer's open descriptor, which lives until the end of the
            // test. F_GETFL only reads flags.
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            initial & libc::O_NONBLOCK
        );
    }

    #[test]
    fn inherited_stderr_child_writes_without_nonblocking_flag_during_timeout() {
        const CHILD_MODE: &str = "EXECUTE_STDIO_INHERITED_STDERR_CHILD";
        const CHILD_BYTES: usize = 1024 * 1024;

        if std::env::var_os(CHILD_MODE).is_some() {
            // The parent releases the child only after pre-filling the shared pipe.
            let mut start = [0u8; 1];
            std::io::stdin().read_exact(&mut start).unwrap();
            let monitor_running = Arc::new(AtomicBool::new(true));
            let nonblocking_seen = Arc::new(AtomicBool::new(false));
            let monitor_stop = monitor_running.clone();
            let monitor_seen = nonblocking_seen.clone();
            let monitor = std::thread::spawn(move || {
                while monitor_stop.load(Ordering::Acquire) {
                    // SAFETY: fd 2 is the child's inherited stderr, open for the whole test.
                    let flags = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL) };
                    if flags == -1 || flags & libc::O_NONBLOCK != 0 {
                        monitor_seen.store(true, Ordering::Release);
                    }
                    std::hint::spin_loop();
                }
            });
            println!("CHILD_READY=1");
            std::io::stdout().flush().unwrap();

            let bytes = vec![b'c'; CHILD_BYTES];
            let mut offset = 0;
            while offset < bytes.len() {
                // SAFETY: bytes[offset..] is a live slice and the child keeps stderr inherited.
                let count = unsafe {
                    libc::write(
                        libc::STDERR_FILENO,
                        bytes[offset..].as_ptr().cast(),
                        bytes.len() - offset,
                    )
                };
                if count > 0 {
                    offset += count as usize;
                    continue;
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("native child stderr write failed: {error}");
            }
            monitor_running.store(false, Ordering::Release);
            monitor.join().unwrap();
            // SAFETY: STDERR_FILENO is valid in this child process and F_GETFL only reads its flags.
            let flags = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL) };
            println!(
                "CHILD_FLAGS={}",
                if flags == -1 || nonblocking_seen.load(Ordering::Acquire) {
                    libc::O_NONBLOCK
                } else {
                    flags & libc::O_NONBLOCK
                }
            );
            std::io::stdout().flush().unwrap();
            assert!(
                !nonblocking_seen.load(Ordering::Acquire),
                "child observed O_NONBLOCK"
            );
            assert_ne!(flags, -1, "reading inherited stderr flags failed");
            return;
        }

        let mut descriptors = [-1; 2];
        // SAFETY: descriptors is a two-element c_int array, which pipe() fills on success.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: pipe() returned this read descriptor, and it has exactly one owner here.
        let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: pipe() returned this write descriptor, and it has exactly one owner here.
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let fd = writer.as_raw_fd();
        // Fill the pipe with a nonblocking writer, then restore its original flags before
        // launching either writer. No reader drains it until both are ready.
        // SAFETY: fd is writer's open descriptor and F_GETFL only reads its status flags.
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(original_flags, -1, "reading test pipe flags failed");
        assert_eq!(
            // SAFETY: fd remains open and F_SETFL takes flags by value without reading pointers.
            unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) },
            0,
            "making test pipe temporarily nonblocking failed"
        );
        let filler = [b'f'; 4096];
        let mut filled = 0usize;
        loop {
            // SAFETY: filler is a live 4096-byte array and fd is the open pipe writer.
            let count = unsafe { libc::write(fd, filler.as_ptr().cast(), filler.len()) };
            if count > 0 {
                filled += count as usize;
                continue;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            break;
        }
        assert!(filled > 0, "test pipe was not filled");
        assert_eq!(
            // SAFETY: fd remains open and F_SETFL takes flags by value without reading pointers.
            unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags) },
            0,
            "restoring blocking test pipe flags failed"
        );

        // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor referring to the same pipe write end.
        let child_writer = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        assert_ne!(child_writer, -1, "duplicating pipe writer failed");
        // SAFETY: child_writer is the fresh descriptor returned above and becomes owned by File.
        let child_writer = unsafe { std::fs::File::from_raw_fd(child_writer) };

        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .arg("--exact")
            .arg("runtime::stdio::tests::inherited_stderr_child_writes_without_nonblocking_flag_during_timeout")
            .arg("--nocapture")
            .env(CHILD_MODE, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(child_writer))
            .spawn()
            .unwrap();

        let (child_ready_sender, child_ready_receiver) = std::sync::mpsc::channel();
        let (child_flags_sender, child_flags_receiver) = std::sync::mpsc::channel();
        let mut child_stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let child_output = std::thread::spawn(move || {
            let mut all_output = String::new();
            loop {
                let mut line = String::new();
                match std::io::BufRead::read_line(&mut child_stdout, &mut line) {
                    Ok(0) => return all_output,
                    Ok(_) => {
                        if line.starts_with("CHILD_READY=") {
                            let _ = child_ready_sender.send(line.clone());
                        }
                        if line.starts_with("CHILD_FLAGS=") {
                            let _ = child_flags_sender.send(line.clone());
                        }
                        all_output.push_str(&line);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => panic!("reading child status failed: {error}"),
                }
            }
        });
        let (drain_start, drain_gate) = std::sync::mpsc::channel();
        let reader = std::fs::File::from(reader);
        let drain = std::thread::spawn(move || {
            let _ = drain_gate.recv();
            let mut reader = reader;
            let mut all_bytes = Vec::with_capacity(filled + CHILD_BYTES * 2);
            let mut chunk = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut reader, &mut chunk) {
                    Ok(0) => return all_bytes,
                    Ok(count) => {
                        all_bytes.extend_from_slice(&chunk[..count]);
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => panic!("draining test pipe failed: {error}"),
                }
            }
        });

        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
        let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
        let output_thread = std::thread::spawn(move || {
            let result = write_with_deadline_inner(
                fd,
                &vec![b'p'; CHILD_BYTES],
                &AtomicBool::new(false),
                Some(Instant::now() + Duration::from_millis(200)),
                Some((ready_sender, release_receiver)),
            );
            let _ = result_sender.send(result);
        });
        let worker_started = ready_receiver.recv_timeout(Duration::from_secs(3)).is_ok();
        let _ = release_sender.send(());
        child.stdin.as_mut().unwrap().write_all(b"start").unwrap();
        drop(child.stdin.take());
        let child_ready = child_ready_receiver.recv_timeout(Duration::from_secs(3));
        drain_start.send(()).unwrap();
        let result_with_slow_reader = result_receiver.recv_timeout(Duration::from_secs(3));
        let timed_out_with_slow_reader = matches!(
            &result_with_slow_reader,
            Ok(Err(error)) if error.is::<super::super::CommandTimedOut>()
        );

        // A bounded watchdog keeps a child or writer regression from hanging the test.
        drop(writer);
        let child_deadline = Instant::now() + Duration::from_secs(5);
        let child_status = loop {
            match child.try_wait().unwrap() {
                Some(status) => break status,
                None if Instant::now() < child_deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child stderr writer did not finish before the watchdog");
                }
            }
        };
        let result = match result_with_slow_reader {
            Ok(result) => result,
            Err(_) => result_receiver
                .recv_timeout(Duration::from_secs(3))
                .expect("runtime writer remained stuck after the deadline watchdog"),
        };
        let _ = output_thread.join();
        assert!(child_status.success(), "child stderr writer failed");
        let child_output = child_output.join().unwrap();
        let child_flags = child_flags_receiver.recv_timeout(Duration::from_secs(3));
        let child_flags_ok = child_flags
            .as_ref()
            .is_ok_and(|line| line.trim() == "CHILD_FLAGS=0");
        assert!(child_ready.is_ok(), "child did not reach its blocked write");
        assert!(
            child_flags_ok,
            "child observed O_NONBLOCK or failed its flag sample"
        );
        assert!(child_output.contains("CHILD_FLAGS=0"));
        assert!(
            worker_started,
            "runtime writer did not reach the blocked-write barrier"
        );
        assert!(
            timed_out_with_slow_reader,
            "runtime write did not return a deadline error with the slow reader active: {result:?}"
        );
        assert!(result.is_err());
        let drained = drain.join().unwrap();
        assert!(drained.len() >= filled + CHILD_BYTES);
        assert!(
            drained.iter().filter(|byte| **byte == b'p').count() < CHILD_BYTES,
            "cancelled output continued until the full parent write completed"
        );
    }

    #[test]
    fn full_descriptor_write_times_out_without_changing_flags() {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors is a 2-element c_int array, which is the size pipe() writes. The
        // assert on this line checks the result before either descriptor is used.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: descriptors[0] is the read end from that successful pipe() call. This OwnedFd
        // is its only owner, so the descriptor is closed exactly once, when _unread drops.
        let _unread = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: descriptors[1] is the write end from the same pipe() call, and no other owner
        // wraps it, so OwnedFd closes it exactly once.
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let fd = writer.as_raw_fd();
        let pool = OutputWriterPool::new();
        // SAFETY: fd is writer's raw descriptor. writer lives until the end of this test, so
        // F_GETFL reads an open descriptor and only reads flags.
        let before = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let error = write_with_deadline_reusing(
            &pool,
            fd,
            &vec![0; 4 * 1024 * 1024],
            &AtomicBool::new(false),
            Some(Instant::now() + Duration::from_millis(20)),
        )
        .unwrap_err();
        assert!(error.is::<super::super::CommandTimedOut>());
        // The output path must preserve whether this shared descriptor is nonblocking.
        assert_eq!(
            // SAFETY: fd is still writer's open descriptor, which lives until the end of the
            // test. F_GETFL only reads flags.
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            before & libc::O_NONBLOCK
        );
        drop((_unread, writer));
    }

    #[test]
    fn timed_out_writer_does_not_block_an_unrelated_descriptor() {
        run_isolated_test(
            "runtime::stdio::tests::timed_out_writer_does_not_block_an_unrelated_descriptor",
            "EXECUTE_TEST_TIMED_OUT_WRITER_UNRELATED",
            || {
                let (stalled_reader, stalled_writer) = test_pipe();
                let stalled_error = write_with_deadline(
                    stalled_writer.as_raw_fd(),
                    &vec![b's'; 4 * 1024 * 1024],
                    &AtomicBool::new(false),
                    Some(Instant::now() + Duration::from_millis(30)),
                )
                .unwrap_err();
                assert!(stalled_error.is::<super::super::CommandTimedOut>());

                let (other_reader, other_writer) = test_pipe();
                write_with_deadline(
                    other_writer.as_raw_fd(),
                    b"unrelated output",
                    &AtomicBool::new(false),
                    Some(Instant::now() + Duration::from_secs(1)),
                )
                .expect("a timed-out writer for another pipe must not reserve this target");
                let mut other_reader = std::fs::File::from(other_reader);
                let mut received = [0u8; 16];
                std::io::Read::read_exact(&mut other_reader, &mut received).unwrap();
                assert_eq!(&received, b"unrelated output");

                drop(stalled_reader);
                drop(stalled_writer);
            },
        );
    }

    #[test]
    fn saturated_output_worker_cap_rejects_unrelated_targets() {
        run_isolated_test(
            "runtime::stdio::tests::saturated_output_worker_cap_rejects_unrelated_targets",
            "EXECUTE_TEST_SATURATED_OUTPUT_WORKER_CAP",
            || {
                let mut pipes = Vec::with_capacity(MAX_ACTIVE_OUTPUT_WRITERS + 1);
                let mut releases = Vec::with_capacity(MAX_ACTIVE_OUTPUT_WRITERS);
                let mut callers = Vec::with_capacity(MAX_ACTIVE_OUTPUT_WRITERS);
                let pools = [
                    Arc::new(OutputWriterPool::new()),
                    Arc::new(OutputWriterPool::new()),
                ];

                for index in 0..MAX_ACTIVE_OUTPUT_WRITERS {
                    let (reader, writer) = test_pipe();
                    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
                    let (release_sender, release_receiver) = mpsc::sync_channel(1);
                    let fd = writer.as_raw_fd();
                    let pool = Arc::clone(&pools[index % pools.len()]);
                    let caller = std::thread::spawn(move || {
                        write_with_deadline_inner_with_pool(
                            &pool,
                            fd,
                            b"x",
                            &AtomicBool::new(false),
                            Some(Instant::now() + Duration::from_secs(10)),
                            Some((ready_sender, release_receiver)),
                        )
                    });
                    ready_receiver
                        .recv_timeout(Duration::from_secs(3))
                        .expect("writer did not reach its reservation gate");
                    pipes.push((reader, writer));
                    releases.push(release_sender);
                    callers.push(caller);
                }

                // Every worker is now holding a real reservation for a distinct pipe target. The
                // deadline-free public API must reject another target promptly instead of waiting for
                // a slot that may be retained forever by a kernel-blocked writer.
                let (extra_reader, extra_writer) = test_pipe();
                let started = Instant::now();
                let error = write_with_deadline_reusing(
                    &OutputWriterPool::new(),
                    extra_writer.as_raw_fd(),
                    b"unrelated",
                    &AtomicBool::new(false),
                    None,
                )
                .unwrap_err();
                assert!(started.elapsed() < Duration::from_secs(1));
                let message = format!("output writer limit ({MAX_ACTIVE_OUTPUT_WRITERS})");
                assert!(error.to_string().contains(&message));

                for release in releases {
                    release.send(()).expect("gated output worker exited early");
                }
                for caller in callers {
                    caller
                        .join()
                        .expect("output caller panicked")
                        .expect("released output worker failed");
                }
                assert!(pools[0].inner.workers.lock().unwrap().len() <= MAX_ACTIVE_OUTPUT_WRITERS);
                drop((pipes, extra_reader, extra_writer));
            },
        );
    }

    #[test]
    fn nested_run_scopes_share_the_process_pool_across_scope_lifetimes() {
        let parent = super::super::Signals::with_cancellation_and_deadline(
            Arc::new(AtomicBool::new(false)),
            false,
            None,
        )
        .unwrap();
        let nested = parent.cleanup_scope(None);
        assert!(Arc::ptr_eq(&parent.output_writers, &nested.output_writers));

        let (reader, writer) = test_pipe();
        write_with_deadline_owned_reusing(
            &parent.output_writers,
            writer.as_raw_fd(),
            b"parent".to_vec(),
            &parent.cancelled,
            None,
        )
        .unwrap();
        drop(parent);
        write_with_deadline_owned_reusing(
            &nested.output_writers,
            writer.as_raw_fd(),
            b" nested".to_vec(),
            &nested.cancelled,
            None,
        )
        .unwrap();
        drop(nested);
        drop(writer);
        let mut output = Vec::new();
        std::fs::File::from(reader)
            .read_to_end(&mut output)
            .unwrap();
        assert_eq!(output, b"parent nested");
    }

    fn test_pipe() -> (OwnedFd, OwnedFd) {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors is a two-element c_int array, which pipe() fills on success.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: pipe() returned the read descriptor, and it has exactly one owner here.
        let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: pipe() returned the write descriptor, and it has exactly one owner here.
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        (reader, writer)
    }
}
