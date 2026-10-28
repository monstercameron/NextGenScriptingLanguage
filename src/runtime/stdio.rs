//! Cooperative descriptor writes for CLI output and runtime trace pipes.
use super::{check_deadline, Signals};
use anyhow::{bail, Context, Result};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};
// The direct terminal writers share the pooled path's cancel-drain grace so
// every hardened terminal write reports cancellation on the same horizon.
use crate::github_actions::TERMINAL_CANCEL_DRAIN_GRACE;

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
    /// Bytes the worker has durably written so far. The terminal-serialized
    /// cancel drain watches this: steady progress extends the drain (a slow
    /// reader still converges with order intact), while no progress lets the
    /// bounded drain expire instead of hanging on a wedged pipe.
    progress: Arc<AtomicU64>,
    write_limit: usize,
    may_sigpipe: bool,
    reservation: WriterReservation,
    /// Hardened terminal writes only: the merged-terminal single-flight
    /// claim, held by the worker until the kernel write finishes so no later
    /// hardened filter — either descriptor, pooled or direct — can classify
    /// before these bytes land, even after a forced cancel-drain expiry.
    merged_terminal: Option<crate::github_actions::MergedTerminalReservation>,
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
            progress,
            write_limit,
            may_sigpipe,
            reservation,
            merged_terminal,
            worker_gate,
            result,
        } = request;
        let result_value = blocking_write_all(
            fd,
            &bytes,
            &stop,
            &progress,
            write_limit,
            may_sigpipe,
            worker_gate,
        );
        drop(reservation);
        // Release the merged single-flight claim only after the kernel write
        // above finished: the next hardened filter (either descriptor) must
        // wait for these bytes to land, even when the caller already gave up
        // waiting at its cancel-drain bound.
        drop(merged_terminal);
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

/// Claim the hardened merged-terminal single flight before filtering. The
/// caller holds the merged-sink lock; the claim moves into the pool worker
/// so it is still held when the caller stops waiting at its cancel-drain
/// bound. Later hardened writes (either descriptor, pooled or direct) block
/// here — still holding the sink — before they may filter, so no filter can
/// classify before these bytes land. The wait keeps checking the caller's
/// own cancel flag and deadline, so cancellation stays bounded: a cancelled
/// waiter reports cancellation without writing anything, while an
/// uncancelled waiter without a deadline stalls together with the wedged
/// terminal rather than forging (fail-closed).
fn reserve_merged_terminal(
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<crate::github_actions::MergedTerminalReservation> {
    // Single implementation lives in `github_actions` so direct (mask/final)
    // and pooled writers poll the same owned claim with the same
    // cancel/deadline semantics; the merged single flight is what preserves
    // cross-fd order after a forced drain expiry. Map a wait failure back
    // onto the caller's own deadline/cancel errors so timeout classification
    // (`CommandTimedOut`) stays intact.
    match crate::github_actions::reserve_merged_terminal(cancelled, deadline) {
        Ok(reservation) => Ok(reservation),
        Err(_) => {
            super::check_deadline(deadline)?;
            if cancelled.load(Ordering::Relaxed) {
                bail!("program cancelled by signal");
            }
            super::check_deadline(deadline)?;
            bail!("program cancelled by signal");
        }
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
    // concurrent. Filtering itself happens in `write_with_deadline_owned_inner`
    // after the merged-terminal claim and the output target reservation (see
    // below), so a forced cancel drain still leaves every later hardened
    // writer — either descriptor — ordered behind this write.
    let hardened_terminal = crate::github_actions::enabled() && (fd == 1 || fd == 2);
    let _sink = hardened_terminal.then(crate::github_actions::terminal_sink_lock);
    write_with_deadline_owned_inner(
        pool,
        fd,
        bytes.to_vec(),
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
    // (the hot path for step output) are neutralized in
    // `write_with_deadline_owned_inner` after the target reservation.
    // Hardened fd 1/2 writes hold the merged-sink lock across filter and
    // worker completion, matching the borrowing path above.
    let hardened_terminal = crate::github_actions::enabled() && (fd == 1 || fd == 2);
    let _sink = hardened_terminal.then(crate::github_actions::terminal_sink_lock);
    write_with_deadline_owned_inner(
        pool,
        fd,
        bytes,
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
    // Claim before filtering (the caller holds the merged-sink lock across
    // this whole call when `terminal_serialized`). The merged claim is the
    // cross-fd single flight: it moves into the worker and is still held
    // when the cancel drain below expires while the worker is
    // kernel-blocked, so a later hardened filter — either descriptor, pooled
    // or direct — waits for these bytes to land before it may classify. The
    // per-target reservation additionally keeps same-target writers ordered
    // behind this write (and bounds unrelated-target fan-out at the worker
    // cap). Both are taken before filtering, in sink, merged, target order.
    let merged_terminal = terminal_serialized
        .then(|| reserve_merged_terminal(cancelled, deadline))
        .transpose()?;
    let reservation = reserve_output_target(target, cancelled, deadline)?;
    // Filter under the caller's sink guard so classification matches landing
    // order. A pending split `:` stays buffered across live writes; only an
    // empty filtered write (a lone held colon) skips the syscall and keeps the
    // state for the next chunk. End-of-stream callers flush explicitly.
    // Note: flushing here would emit a pending `:` immediately, breaking the
    // split-`::` protection across writes.
    let data = if crate::github_actions::enabled() {
        if fd == 1 {
            crate::github_actions::filter_stdout(&data)
        } else if fd == 2 {
            crate::github_actions::filter_stderr(&data)
        } else {
            data
        }
    } else {
        data
    };
    if data.is_empty() {
        super::check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) {
            bail!("program cancelled by signal");
        }
        return Ok(());
    }
    let worker_fd = crate::platform::unix_ffi::duplicate_cloexec(fd, 3)
        .context("duplicating output descriptor")?;
    let stop = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(AtomicU64::new(0));
    let receiver = pool.submit(
        WriteRequest {
            fd: worker_fd,
            bytes: data,
            stop: Arc::clone(&stop),
            progress: Arc::clone(&progress),
            write_limit,
            may_sigpipe,
            reservation,
            merged_terminal,
            worker_gate,
            result: None,
        },
        cancelled,
        deadline,
    )?;

    // When `terminal_serialized` the caller holds the merged-sink lock across
    // this wait so classification matches landing order in the merged runner
    // log. A cancel or deadline signals `stop` and then drains the worker
    // instead of releasing the lock early (which would let the next filter
    // classify before the prior bytes land, re-opening the reorder forgery)
    // or hanging without a bound (a `write(2)` blocked in-kernel on a full
    // pipe with no reader never returns, so an unbounded wait wedges every
    // hardened write behind this lock). The drain is progress-gated: steady
    // worker progress (a slow reader) extends it up to the caller deadline so
    // order stays intact, while no progress lets it expire — at the caller
    // deadline, or after `TERMINAL_CANCEL_DRAIN_GRACE` without one — and the
    // cancellation is reported. The in-flight worker keeps its target
    // reservation and its merged-terminal claim until it finishes, so every
    // later hardened writer (either descriptor, pooled or direct) still
    // filters behind it even after a forced drain expiry: cross-fd order is
    // preserved by the single flight, not by the released sink lock.
    let mut drain_until: Option<Instant> = None;
    let mut last_progress = 0u64;
    loop {
        let cancelled_now = cancelled.load(Ordering::Relaxed);
        let past_deadline = super::check_deadline(deadline).is_err();
        if cancelled_now || past_deadline {
            stop.store(true, Ordering::Release);
            if !terminal_serialized {
                if cancelled_now {
                    return Err(anyhow::anyhow!("program cancelled by signal"));
                }
                return Err(super::check_deadline(deadline).unwrap_err());
            }
            let now = Instant::now();
            let made = progress.load(Ordering::Acquire);
            if made != last_progress {
                // The worker is still landing bytes: extend the drain so order
                // stays intact, but never past the caller deadline.
                last_progress = made;
                drain_until = Some(match deadline {
                    Some(end) => (now + TERMINAL_CANCEL_DRAIN_GRACE).min(end),
                    None => now + TERMINAL_CANCEL_DRAIN_GRACE,
                });
            } else if drain_until.is_none() {
                drain_until = Some(match deadline {
                    Some(end) if end < now + TERMINAL_CANCEL_DRAIN_GRACE => end,
                    _ => now + TERMINAL_CANCEL_DRAIN_GRACE,
                });
            }
        }
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(result) => {
                // The worker finished: its bytes landed in order. Report any
                // pending cancel/deadline first, else the worker's own result.
                if cancelled.load(Ordering::Relaxed) {
                    return Err(anyhow::anyhow!("program cancelled by signal"));
                }
                super::check_deadline(deadline)?;
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if terminal_serialized
                    && (cancelled.load(Ordering::Relaxed)
                        || super::check_deadline(deadline).is_err())
                {
                    // Draining with a dead worker: nothing is still in flight,
                    // so report the pending cancel/deadline without waiting.
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(anyhow::anyhow!("program cancelled by signal"));
                    }
                    return Err(super::check_deadline(deadline).unwrap_err());
                }
                bail!("output writer stopped without returning a result");
            }
        }
        if terminal_serialized
            && (cancelled.load(Ordering::Relaxed) || super::check_deadline(deadline).is_err())
        {
            match drain_until {
                Some(until) if Instant::now() >= until => {
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(anyhow::anyhow!("program cancelled by signal"));
                    }
                    return Err(super::check_deadline(deadline).unwrap_err());
                }
                _ => {}
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
    progress: &AtomicU64,
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
                progress.store(offset as u64, Ordering::Release);
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
///
/// Streaming diagnostics (warnings, progress) use this: the stderr sanitizer
/// keeps any held-back prefix for the next chunk, so split `::` protection
/// survives across writes. True end-of-stream diagnostics must use
/// [`write_exit_diagnostic`] instead so a trailing held prefix is delivered
/// rather than silently truncated.
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

/// Write a true end-of-stream stderr diagnostic: sink-atomic filter+flush when
/// hardening is on (so a trailing held prefix is delivered, not truncated),
/// otherwise the bounded pooled [`write_diagnostic`] path.
///
/// Only for diagnostics after which this run or process writes no more
/// stderr: flushing mid-stream would release a held `:` that a following
/// chunk's `:` then completes into a forged line-leading `::`. Like
/// `github_actions::write_stdout_final` this is a direct (non-pooled) write
/// when hardening is on, without the pooled deadline: it runs at stream end
/// when completeness matters most. Cancellation during the write itself is
/// still bounded through the direct worker's progress-gated drain.
pub fn write_exit_diagnostic(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if crate::github_actions::enabled() {
        let _ = crate::github_actions::write_stderr_final(bytes);
    } else {
        write_diagnostic(bytes);
    }
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
                        &AtomicU64::new(0),
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

    /// Real pooled-fd proof for the merged sink: both terminal fds are dup2'd
    /// onto one pipe (the runner's merged job log), and the production path —
    /// `write_with_deadline` through the shared pool, `dup`, reservation and
    /// `PIPE_BUF` chunking — races concurrent 8 KiB mask lines (kernel-split
    /// past `PIPE_BUF` on every platform) against forged `::error::` chunks on
    /// both streams. Every mask must land intact on its own line and no
    /// forgery may parse, whichever order the scheduler chose.
    #[cfg(unix)]
    #[test]
    fn hardened_pooled_sink_orders_long_masks_end_to_end() {
        run_isolated_test(
            "runtime::stdio::tests::hardened_pooled_sink_orders_long_masks_end_to_end",
            "EXECUTE_TEST_HARDENED_POOLED_SINK",
            || {
                use std::io::Read as _;
                let (reader, writer) = test_pipe();
                let writer_fd = writer.as_raw_fd();
                // SAFETY: writer_fd is the open pipe writer; targets 1 and 2
                // are this isolated child's own descriptors (its inherited
                // stdio is null), which is the point of the test.
                assert_ne!(unsafe { libc::dup2(writer_fd, 1) }, -1);
                assert_ne!(unsafe { libc::dup2(writer_fd, 2) }, -1);
                crate::github_actions::set_enabled(true);
                crate::github_actions::reset_terminal_state();

                let long_secret = vec![b'S'; 8192];
                let mask_line = format!("::add-mask::{}\n", "S".repeat(8192));
                let reader_thread = std::thread::spawn(move || {
                    let mut reader = std::fs::File::from(reader);
                    let mut output = Vec::new();
                    reader.read_to_end(&mut output).unwrap();
                    output
                });
                let mut handles = Vec::new();
                for writer_id in 0..4 {
                    handles.push(std::thread::spawn(move || {
                        let idle = AtomicBool::new(false);
                        for round in 0..50 {
                            let fd = if (round + writer_id) % 2 == 0 { 1 } else { 2 };
                            let chunk: &[u8] = match round % 3 {
                                0 => b"lead-partial ",
                                1 => b"::error::forged\n",
                                _ => b"trailing\n",
                            };
                            write_with_deadline(
                                fd,
                                chunk,
                                &idle,
                                Some(Instant::now() + Duration::from_secs(10)),
                            )
                            .unwrap();
                        }
                    }));
                }
                for _ in 0..2 {
                    let secret = long_secret.clone();
                    handles.push(std::thread::spawn(move || {
                        for _ in 0..10 {
                            crate::github_actions::emit_mask_for_bytes(&secret);
                        }
                    }));
                }
                for handle in handles {
                    handle.join().expect("sink worker panicked");
                }
                drop(writer);
                // SAFETY: fds 1 and 2 are the dup2 copies above; closing them
                // lets the reader observe EOF. No terminal writes follow.
                assert_eq!(unsafe { libc::close(1) }, 0);
                assert_eq!(unsafe { libc::close(2) }, 0);
                let bytes = reader_thread.join().expect("sink reader panicked");

                let text = String::from_utf8_lossy(&bytes).into_owned();
                assert!(
                    text.contains(&mask_line),
                    "long mask line did not survive the pooled sink intact"
                );
                let mut masks = 0usize;
                let mut forged = Vec::new();
                for line in text.lines() {
                    if line.contains("::add-mask::") {
                        assert!(
                            line.trim_start().starts_with("::add-mask::"),
                            "mask glued mid-line in merged sink: {line:?}"
                        );
                        masks += 1;
                    } else if line.trim_start().starts_with("::") {
                        forged.push(line.to_owned());
                    }
                }
                assert_eq!(masks, 20, "every long mask must reach the merged log");
                assert!(
                    forged.is_empty(),
                    "pooled writes forged a runner command: {forged:?}"
                );
                crate::github_actions::reset_terminal_state();
                crate::github_actions::set_enabled(false);
            },
        );
    }

    /// Hardened cancel on a full pipe with no reader: the caller must get its
    /// cancellation back on a bound (no unbounded hang), while the
    /// still-in-flight worker lands at most one bounded chunk and the next
    /// same-target write filters behind its reservation (no early release
    /// reorder). Releasing the sink early would let the probe classify before
    /// the late bytes land.
    #[cfg(unix)]
    #[test]
    fn hardened_cancel_on_full_pipe_returns_without_releasing_the_sink_early() {
        run_isolated_test(
            "runtime::stdio::tests::hardened_cancel_on_full_pipe_returns_without_releasing_the_sink_early",
            "EXECUTE_TEST_HARDENED_CANCEL_DRAIN",
            || {
                use std::io::Read as _;
                let (reader, writer) = test_pipe();
                let fd = writer.as_raw_fd();
                // Fill the pipe, then restore blocking mode: the runtime
                // writer below blocks in-kernel with no reader until cancelled.
                // SAFETY: fd is the open pipe writer; F_GETFL only reads flags.
                let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                assert_ne!(original, -1);
                // SAFETY: fd stays open; F_SETFL takes flags by value.
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_SETFL, original | libc::O_NONBLOCK) },
                    0
                );
                let filler = [b'f'; 4096];
                let mut filled = 0usize;
                loop {
                    // SAFETY: filler is live; fd is the open pipe writer.
                    let count =
                        unsafe { libc::write(fd, filler.as_ptr().cast(), filler.len()) };
                    if count > 0 {
                        filled += count as usize;
                        continue;
                    }
                    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                    break;
                }
                assert!(filled > 0);
                // SAFETY: fd stays open; F_SETFL takes flags by value.
                assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, original) }, 0);
                // SAFETY: target 1 is this isolated child's own stdout.
                assert_ne!(unsafe { libc::dup2(fd, 1) }, -1);
                crate::github_actions::set_enabled(true);
                crate::github_actions::reset_terminal_state();

                let cancelled = Arc::new(AtomicBool::new(false));
                let worker_cancelled = Arc::clone(&cancelled);
                let (result_sender, result_receiver) = mpsc::sync_channel(1);
                let caller = std::thread::spawn(move || {
                    let result = write_with_deadline(
                        1,
                        &vec![b'x'; 1024 * 1024],
                        &worker_cancelled,
                        None,
                    );
                    let _ = result_sender.send(result);
                });
                // Let the caller take the sink, reserve the target and block
                // its worker in-kernel on the full pipe.
                std::thread::sleep(Duration::from_millis(500));
                cancelled.store(true, Ordering::Release);
                let error = result_receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("hardened cancel did not return on a wedged pipe")
                    .unwrap_err();
                assert!(error.to_string().contains("cancelled"));
                caller.join().expect("cancelled caller panicked");

                // Drain the filler so the in-flight write can land its one
                // bounded chunk, see `stop`, and release the reservation.
                let mut reader = std::fs::File::from(reader);
                let mut filler_drained = vec![0u8; filled];
                reader.read_exact(&mut filler_drained).unwrap();
                assert!(filler_drained.iter().all(|byte| *byte == b'f'));
                // The probe filters behind the late chunk's reservation, so it
                // can only land after it.
                write_with_deadline(
                    1,
                    b"probe",
                    &AtomicBool::new(false),
                    Some(Instant::now() + Duration::from_secs(5)),
                )
                .unwrap();
                let mut late = Vec::new();
                let mut chunk = [0u8; 512];
                while !late.windows(5).any(|window| window == b"probe") {
                    match reader.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(count) => late.extend_from_slice(&chunk[..count]),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => panic!("draining wedged pipe failed: {error}"),
                    }
                }
                let position = late
                    .windows(5)
                    .position(|window| window == b"probe")
                    .expect("probe did not land after the cancelled write");
                let late_bytes = &late[..position];
                assert!(
                    !late_bytes.is_empty()
                        && late_bytes.len() <= libc::PIPE_BUF as usize
                        && late_bytes.iter().all(|byte| *byte == b'x'),
                    "cancelled write leaked past one bounded chunk: {} bytes",
                    late_bytes.len()
                );
                drop(writer);
                // SAFETY: fd 1 is the dup2 copy above; no terminal writes follow.
                assert_eq!(unsafe { libc::close(1) }, 0);
                crate::github_actions::reset_terminal_state();
                crate::github_actions::set_enabled(false);
            },
        );
    }

    /// Cross-fd single flight after a forced cancel-drain expiry. Fd 1 is
    /// wedged full with no reader while fd 2 is a healthy separate pipe —
    /// a different reservation target, so only the merged claim serializes
    /// the two. Cancelling the wedged write must return on a bound; while
    /// its worker is still kernel-blocked the merged claim must stay held
    /// (no later hardened filter may classify before those uncertain bytes
    /// land); a cross-fd probe must wait instead of filtering early, then
    /// land intact once the pipe drains, and the claim must release without
    /// leaking. The old split admitted the probe's filter before the late
    /// bytes landed; this test pins the fail-closed order with real pipes,
    /// the real pool, and real cancellation.
    #[cfg(unix)]
    #[test]
    fn hardened_cancel_wedge_single_flight_orders_cross_fd() {
        run_isolated_test(
            "runtime::stdio::tests::hardened_cancel_wedge_single_flight_orders_cross_fd",
            "EXECUTE_TEST_HARDENED_CROSS_FD_WEDGE",
            || {
                use std::io::Read as _;
                let (reader_a, writer_a) = test_pipe();
                let (reader_b, writer_b) = test_pipe();
                let fd_a = writer_a.as_raw_fd();
                let fd_b = writer_b.as_raw_fd();
                // Fill pipe A, then restore blocking mode: the runtime
                // writer below blocks in-kernel with no reader until cancelled.
                // SAFETY: fd_a is the open pipe writer; F_GETFL only reads flags.
                let original = unsafe { libc::fcntl(fd_a, libc::F_GETFL) };
                assert_ne!(original, -1);
                // SAFETY: fd_a stays open; F_SETFL takes flags by value.
                assert_eq!(
                    unsafe { libc::fcntl(fd_a, libc::F_SETFL, original | libc::O_NONBLOCK) },
                    0
                );
                let filler = [b'f'; 4096];
                let mut filled = 0usize;
                loop {
                    // SAFETY: filler is live; fd_a is the open pipe writer.
                    let count = unsafe { libc::write(fd_a, filler.as_ptr().cast(), filler.len()) };
                    if count > 0 {
                        filled += count as usize;
                        continue;
                    }
                    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                    break;
                }
                assert!(filled > 0);
                // SAFETY: fd_a stays open; F_SETFL takes flags by value.
                assert_eq!(unsafe { libc::fcntl(fd_a, libc::F_SETFL, original) }, 0);
                // SAFETY: targets 1 and 2 are this isolated child's own
                // descriptors (its inherited stdio is null).
                assert_ne!(unsafe { libc::dup2(fd_a, 1) }, -1);
                assert_ne!(unsafe { libc::dup2(fd_b, 2) }, -1);
                crate::github_actions::set_enabled(true);
                crate::github_actions::reset_terminal_state();

                // Pipe B stays healthy: a reader drains it for the whole test.
                let reader_thread_b = std::thread::spawn(move || {
                    let mut reader = std::fs::File::from(reader_b);
                    let mut output = Vec::new();
                    reader.read_to_end(&mut output).unwrap();
                    output
                });

                let cancelled = Arc::new(AtomicBool::new(false));
                let worker_cancelled = Arc::clone(&cancelled);
                let (result_sender, result_receiver) = mpsc::sync_channel(1);
                let caller = std::thread::spawn(move || {
                    let result =
                        write_with_deadline(1, &vec![b'x'; 1024 * 1024], &worker_cancelled, None);
                    let _ = result_sender.send(result);
                });
                // Let the caller take the sink and the merged claim and block
                // its worker in-kernel on the full pipe.
                std::thread::sleep(Duration::from_millis(500));
                cancelled.store(true, Ordering::Release);
                let error = result_receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("hardened cancel did not return on a wedged pipe")
                    .unwrap_err();
                assert!(error.to_string().contains("cancelled"));
                caller.join().expect("cancelled caller panicked");

                // The forced drain expiry released the sink, but the
                // kernel-blocked worker still owns the merged single flight:
                // no later hardened filter may classify yet.
                assert!(crate::github_actions::try_claim_merged_terminal().is_none());

                // A cross-fd write to the healthy pipe must wait for those
                // uncertain bytes instead of filtering early. It cannot
                // complete while nobody drains pipe A, so a short timeout
                // here is a deterministic order assertion, not a race.
                let (probe_sender, probe_receiver) = mpsc::sync_channel(1);
                let probe_caller = std::thread::spawn(move || {
                    let result = write_with_deadline(
                        2,
                        b"probe-after-wedge",
                        &AtomicBool::new(false),
                        Some(Instant::now() + Duration::from_secs(10)),
                    );
                    let _ = probe_sender.send(result);
                });
                assert!(
                    matches!(
                        probe_receiver.recv_timeout(Duration::from_millis(300)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ),
                    "cross-fd write filtered before the wedged write landed"
                );

                // Drain the filler so the in-flight write can land its one
                // bounded chunk, see `stop`, and release the claim; the probe
                // then filters behind it and lands intact.
                let mut reader = std::fs::File::from(reader_a);
                let mut filler_drained = vec![0u8; filled];
                reader.read_exact(&mut filler_drained).unwrap();
                assert!(filler_drained.iter().all(|byte| *byte == b'f'));
                probe_receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("cross-fd probe did not land after the drain")
                    .expect("cross-fd probe failed");
                probe_caller.join().expect("probe caller panicked");
                // The in-flight worker saw `stop` after landing at most one
                // bounded chunk and released the single flight: polling must
                // observe the release instead of hanging on a leak.
                let release_deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if crate::github_actions::try_claim_merged_terminal().is_some() {
                        break;
                    }
                    assert!(
                        Instant::now() < release_deadline,
                        "merged single flight leaked after the wedged write landed"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                // A mask registered after the wedge still lands intact on its
                // own line instead of being skipped or glued mid-line.
                crate::github_actions::emit_mask_for_bytes(b"wedge-secret-499");
                drop(writer_a);
                drop(writer_b);
                // SAFETY: fds 1 and 2 are the dup2 copies above; no terminal
                // writes follow.
                assert_eq!(unsafe { libc::close(1) }, 0);
                assert_eq!(unsafe { libc::close(2) }, 0);
                let pipe_b = reader_thread_b.join().expect("pipe B reader panicked");
                assert_eq!(pipe_b, b"probe-after-wedge");
                let mut late_a = Vec::new();
                reader.read_to_end(&mut late_a).unwrap();
                let x_run = late_a.iter().take_while(|byte| **byte == b'x').count();
                assert!(
                    (1..=libc::PIPE_BUF as usize).contains(&x_run),
                    "cancelled write leaked past one bounded chunk: {x_run} bytes"
                );
                // The mask opened its own line immediately after the late
                // bytes and survived intact; nothing else parses.
                assert_eq!(late_a.get(x_run), Some(&b'\n'));
                let text = String::from_utf8_lossy(&late_a).into_owned();
                assert!(
                    text.contains("\n::add-mask::wedge-secret-499\n"),
                    "mask did not survive the wedge intact: {text:?}"
                );
                for line in text.lines() {
                    if line.trim_start().starts_with("::") {
                        assert!(
                            line.contains("add-mask"),
                            "wedged writes forged a runner command: {line:?}"
                        );
                    }
                }
                crate::github_actions::reset_terminal_state();
                crate::github_actions::set_enabled(false);
            },
        );
    }

    /// Direct waits and reset stay bounded while a pooled worker is still
    /// kernel-blocked. Fd 1 is wedged full with no reader; the pooled write is
    /// cancelled and its drain expires (sink released) while the worker still
    /// holds the merged claim. Cancelled mask/final waits and a cancelled
    /// reset must each report on a bound without writing or clearing state —
    /// no unbounded hang, no false unlock — and after the pipe drains the
    /// late bytes, the mask, and the final land in order.
    #[cfg(unix)]
    #[test]
    fn hardened_direct_cancel_and_reset_stay_bounded_while_worker_in_flight() {
        run_isolated_test(
            "runtime::stdio::tests::hardened_direct_cancel_and_reset_stay_bounded_while_worker_in_flight",
            "EXECUTE_TEST_HARDENED_DIRECT_CANCEL_RESET",
            || {
                use std::io::Read as _;
                let (reader, writer) = test_pipe();
                let fd = writer.as_raw_fd();
                // Fill the pipe, then restore blocking mode: the runtime
                // writer below blocks in-kernel with no reader until cancelled.
                // SAFETY: fd is the open pipe writer; F_GETFL only reads flags.
                let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                assert_ne!(original, -1);
                // SAFETY: fd stays open; F_SETFL takes flags by value.
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_SETFL, original | libc::O_NONBLOCK) },
                    0
                );
                let filler = [b'f'; 4096];
                let mut filled = 0usize;
                loop {
                    // SAFETY: filler is live; fd is the open pipe writer.
                    let count =
                        unsafe { libc::write(fd, filler.as_ptr().cast(), filler.len()) };
                    if count > 0 {
                        filled += count as usize;
                        continue;
                    }
                    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                    break;
                }
                assert!(filled > 0);
                // SAFETY: fd stays open; F_SETFL takes flags by value.
                assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, original) }, 0);
                // SAFETY: target 1 is this isolated child's own stdout.
                assert_ne!(unsafe { libc::dup2(fd, 1) }, -1);
                crate::github_actions::set_enabled(true);
                crate::github_actions::reset_terminal_state();

                // Wedge one pooled write on the full pipe.
                let cancelled = Arc::new(AtomicBool::new(false));
                let worker_cancelled = Arc::clone(&cancelled);
                let (result_sender, result_receiver) = mpsc::sync_channel(1);
                let caller = std::thread::spawn(move || {
                    let result = write_with_deadline(
                        1,
                        &vec![b'x'; 1024 * 1024],
                        &worker_cancelled,
                        None,
                    );
                    let _ = result_sender.send(result);
                });
                std::thread::sleep(Duration::from_millis(500));
                cancelled.store(true, Ordering::Release);
                let error = result_receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("hardened cancel did not return on a wedged pipe")
                    .unwrap_err();
                assert!(error.to_string().contains("cancelled"));
                caller.join().expect("cancelled caller panicked");

                // The drain expired while the worker is still kernel-blocked:
                // the merged single flight is still held, so no later filter
                // may classify yet.
                assert!(crate::github_actions::try_claim_merged_terminal().is_none());

                // Cancelled direct waits report on a bound instead of hanging
                // behind the wedged worker. Nothing is resynced or written, so
                // merged order stays fail-closed.
                let direct_cancelled = AtomicBool::new(true);
                let mask_error = crate::github_actions::emit_mask_for_bytes_with_cancel(
                    b"direct-cancel-499",
                    &direct_cancelled,
                    None,
                )
                .unwrap_err();
                assert!(mask_error.to_string().contains("cancelled"));
                let final_error =
                    crate::github_actions::write_stdout_final_with_cancel(
                        b"late-final",
                        &direct_cancelled,
                        None,
                    )
                    .unwrap_err();
                assert!(final_error.to_string().contains("cancelled"));
                let diagnostic_error =
                    crate::github_actions::write_stderr_final_with_cancel(
                        b"late-diagnostic",
                        &direct_cancelled,
                        None,
                    )
                    .unwrap_err();
                assert!(diagnostic_error.to_string().contains("cancelled"));

                // A cancelled reset reports without touching sanitizer state;
                // a past-deadline reset does too. The in-flight claim stays
                // owned by its worker in both cases: no false unlock.
                let reset_error =
                    crate::github_actions::reset_terminal_state_with_cancel(
                        &direct_cancelled,
                        None,
                    )
                    .unwrap_err();
                assert!(reset_error.to_string().contains("cancelled"));
                assert!(crate::github_actions::reset_terminal_state_with_cancel(
                    &AtomicBool::new(false),
                    Some(Instant::now())
                )
                .is_err());
                assert!(
                    crate::github_actions::try_claim_merged_terminal().is_none(),
                    "cancelled direct wait or reset released the in-flight claim"
                );

                // Drain the filler so the in-flight write lands its one
                // bounded chunk, sees `stop`, and releases the claim.
                let mut reader = std::fs::File::from(reader);
                let mut filler_drained = vec![0u8; filled];
                reader.read_exact(&mut filler_drained).unwrap();
                assert!(filler_drained.iter().all(|byte| *byte == b'f'));
                let release_deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if crate::github_actions::try_claim_merged_terminal().is_some() {
                        break;
                    }
                    assert!(
                        Instant::now() < release_deadline,
                        "merged single flight leaked after the wedged write landed"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }

                // Direct writes and the reset work again once the worker
                // finished, and the bytes land in merged order.
                crate::github_actions::emit_mask_for_bytes(b"direct-after-drain-499");
                crate::github_actions::write_stdout_final(b"final-after-drain")
                    .expect("final write after the drain");
                drop(writer);
                // SAFETY: fd 1 is the dup2 copy above; no terminal writes follow.
                assert_eq!(unsafe { libc::close(1) }, 0);
                let mut late = Vec::new();
                reader.read_to_end(&mut late).unwrap();
                let x_run = late.iter().take_while(|byte| **byte == b'x').count();
                assert!(
                    (1..=libc::PIPE_BUF as usize).contains(&x_run),
                    "cancelled write leaked past one bounded chunk: {x_run} bytes"
                );
                let text = String::from_utf8_lossy(&late).into_owned();
                assert!(
                    text.contains("\n::add-mask::direct-after-drain-499\n"),
                    "mask did not survive the wedge intact: {text:?}"
                );
                assert!(
                    text.ends_with("final-after-drain"),
                    "final did not land after the mask: {text:?}"
                );
                for line in text.lines() {
                    if line.trim_start().starts_with("::") {
                        assert!(
                            line.contains("add-mask"),
                            "wedged writes forged a runner command: {line:?}"
                        );
                    }
                }
                crate::github_actions::reset_terminal_state();
                crate::github_actions::set_enabled(false);
            },
        );
    }

    /// Direct writes wedged in the write phase itself stay cancellable.
    /// Fd 1 and 2 share one full pipe with no reader, so the direct worker
    /// (not a pooled worker, and not a claim waiter) is the thread blocked
    /// in-kernel. Cancelling the final must return on the drain bound while
    /// the worker keeps the merged claim; a cancellable streaming stderr
    /// write (the live serve-notice path) must also report while waiting
    /// behind that claim instead of hanging. After the drain, the late
    /// bounded chunk, the notice, and a final land in merged order.
    #[cfg(unix)]
    #[test]
    fn hardened_direct_write_cancel_returns_while_direct_write_wedged() {
        run_isolated_test(
            "runtime::stdio::tests::hardened_direct_write_cancel_returns_while_direct_write_wedged",
            "EXECUTE_TEST_HARDENED_DIRECT_WRITE_WEDGE",
            || {
                use std::io::Read as _;
                let (reader, writer) = test_pipe();
                let fd = writer.as_raw_fd();
                // Fill the pipe, then restore blocking mode: the direct
                // worker below blocks in-kernel with no reader.
                // SAFETY: fd is the open pipe writer; F_GETFL only reads flags.
                let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                assert_ne!(original, -1);
                // SAFETY: fd stays open; F_SETFL takes flags by value.
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_SETFL, original | libc::O_NONBLOCK) },
                    0
                );
                let filler = [b'f'; 4096];
                let mut filled = 0usize;
                loop {
                    // SAFETY: filler is live; fd is the open pipe writer.
                    let count =
                        unsafe { libc::write(fd, filler.as_ptr().cast(), filler.len()) };
                    if count > 0 {
                        filled += count as usize;
                        continue;
                    }
                    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                    break;
                }
                assert!(filled > 0);
                // SAFETY: fd stays open; F_SETFL takes flags by value.
                assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, original) }, 0);
                // SAFETY: targets 1 and 2 are this isolated child's own
                // descriptors (its inherited stdio is null).
                assert_ne!(unsafe { libc::dup2(fd, 1) }, -1);
                assert_ne!(unsafe { libc::dup2(fd, 2) }, -1);
                crate::github_actions::set_enabled(true);
                crate::github_actions::reset_terminal_state();

                // Wedge a direct final write in its write phase.
                let cancelled = Arc::new(AtomicBool::new(false));
                let worker_cancelled = Arc::clone(&cancelled);
                let (result_sender, result_receiver) = mpsc::sync_channel(1);
                let caller = std::thread::spawn(move || {
                    let result = crate::github_actions::write_stdout_final_with_cancel(
                        &vec![b'y'; 1024 * 1024],
                        &worker_cancelled,
                        None,
                    );
                    let _ = result_sender.send(result);
                });
                std::thread::sleep(Duration::from_millis(500));
                cancelled.store(true, Ordering::Release);
                let error = result_receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("direct cancel did not return on a wedged write")
                    .unwrap_err();
                assert!(error.to_string().contains("cancelled"));
                caller.join().expect("cancelled direct caller panicked");

                // The drain expired while the direct worker is still
                // kernel-blocked: it keeps the merged single flight.
                assert!(crate::github_actions::try_claim_merged_terminal().is_none());

                // The serve-notice path waits behind that claim with its own
                // stop flag: cancelling it reports on a bound too.
                let notice_cancelled = Arc::new(AtomicBool::new(false));
                let notice_flag = Arc::clone(&notice_cancelled);
                let (notice_sender, notice_receiver) = mpsc::sync_channel(1);
                let notice_caller = std::thread::spawn(move || {
                    let result = crate::github_actions::write_direct_stderr_with_cancel(
                        b"serve-notice\n",
                        &notice_flag,
                        None,
                    );
                    let _ = notice_sender.send(result);
                });
                std::thread::sleep(Duration::from_millis(300));
                notice_cancelled.store(true, Ordering::Release);
                let notice_error = notice_receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("serve-notice cancel did not return behind a wedged direct write")
                    .unwrap_err();
                assert!(notice_error.to_string().contains("cancelled"));
                notice_caller.join().expect("notice caller panicked");
                assert!(crate::github_actions::try_claim_merged_terminal().is_none());

                // Drain the filler so the wedged chunk lands, the worker
                // sees `stop`, and the claim releases without leaking.
                let mut reader = std::fs::File::from(reader);
                let mut filler_drained = vec![0u8; filled];
                reader.read_exact(&mut filler_drained).unwrap();
                assert!(filler_drained.iter().all(|byte| *byte == b'f'));
                let release_deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if crate::github_actions::try_claim_merged_terminal().is_some() {
                        break;
                    }
                    assert!(
                        Instant::now() < release_deadline,
                        "merged single flight leaked after the wedged direct write landed"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }

                // Both direct paths write again once the worker finished,
                // and the bytes land in merged order behind the late chunk.
                crate::github_actions::write_direct_stderr_with_cancel(
                    b"serve-notice\n",
                    &AtomicBool::new(false),
                    Some(Instant::now() + Duration::from_secs(5)),
                )
                .expect("serve notice after the drain");
                crate::github_actions::write_stdout_final(b"final-after-drain")
                    .expect("final write after the drain");
                drop(writer);
                // SAFETY: fds 1 and 2 are the dup2 copies above; no
                // terminal writes follow.
                assert_eq!(unsafe { libc::close(1) }, 0);
                assert_eq!(unsafe { libc::close(2) }, 0);
                let mut late = Vec::new();
                reader.read_to_end(&mut late).unwrap();
                let y_run = late.iter().take_while(|byte| **byte == b'y').count();
                assert!(
                    (1..=8 * 1024).contains(&y_run),
                    "cancelled direct write leaked past one bounded chunk: {y_run} bytes"
                );
                let text = String::from_utf8_lossy(&late).into_owned();
                let notice_at = text
                    .find("serve-notice\n")
                    .expect("serve notice did not land after the drain");
                assert!(notice_at >= y_run, "notice landed before the late chunk");
                assert!(
                    text.ends_with("final-after-drain"),
                    "final did not land after the notice: {text:?}"
                );
                crate::github_actions::reset_terminal_state();
                crate::github_actions::set_enabled(false);
            },
        );
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
