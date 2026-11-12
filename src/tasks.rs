//! Structured ownership for concurrent applet and graph workers.
//!
//! A [`TaskScope`] owns every worker it starts. Dropping the scope requests
//! cancellation and joins all children, so a task cannot silently outlive its
//! group. Work functions receive a [`CancellationToken`] and must pass it into
//! the runtime operation they supervise. Applet-specific policy and contract
//! preflight belongs to the caller and must complete before spawning work.

use anyhow::{anyhow, bail, Result};
use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

/// A callback run on every explicit cancellation of a task group.
pub(crate) type CancelHook = Arc<dyn Fn() + Send + Sync>;

/// A one-shot wake descriptor shared by a task group and its native children.
/// Unix supervisors poll this beside their pipes and child-exit handles, so an
/// idle group sleeps until output, process exit, or cancellation is ready.
/// Blocking waiters that sleep on their own condition variable register a
/// [`CancelHook`] so an explicit cancellation wakes them as well.
pub(crate) struct CancellationWake {
    notified: AtomicBool,
    hooks: Mutex<Vec<CancelHook>>,
    #[cfg(unix)]
    reader: std::os::unix::net::UnixStream,
    #[cfg(unix)]
    writer: Mutex<std::os::unix::net::UnixStream>,
    #[cfg(unix)]
    writer_fd: RawFd,
}

impl std::fmt::Debug for CancellationWake {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CancellationWake")
            .field("notified", &self.notified.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl CancellationWake {
    pub(crate) fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
            reader.set_nonblocking(true)?;
            writer.set_nonblocking(true)?;
            let writer_fd = writer.as_raw_fd();
            Ok(Self {
                notified: AtomicBool::new(false),
                hooks: Mutex::new(Vec::new()),
                reader,
                writer: Mutex::new(writer),
                writer_fd,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                notified: AtomicBool::new(false),
                hooks: Mutex::new(Vec::new()),
            })
        }
    }

    pub(crate) fn notify(&self) {
        if !self.notified.swap(true, Ordering::AcqRel) {
            #[cfg(unix)]
            if let Ok(mut writer) = self.writer.lock() {
                let _ = writer.write(&[1]);
            }
        }
    }

    /// Register a hook that runs on every [`CancellationWake::cancel_all`].
    pub(crate) fn on_cancel(&self, hook: CancelHook) {
        let mut hooks = self.hooks.lock().unwrap_or_else(|e| e.into_inner());
        hooks.push(hook);
    }

    /// Wake every waiter for an explicit cancellation. The caller must store the
    /// cancellation flag before calling this, and must not hold any lock that a
    /// hook takes. A waiter that re-checks the flag under its own lock then either
    /// sees it, or is already blocked when its hook notifies.
    pub(crate) fn cancel_all(&self) {
        let hooks = self.hooks.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for hook in &hooks {
            hook();
        }
        self.notify();
    }

    /// Signal-handler variant: this uses only lock-free atomics and `write(2)`.
    /// Every signal writes its byte (bytes coalesce while the reader is never
    /// drained) so a repeat interrupt during cleanup wakes the native
    /// supervisor even after the first interrupt already notified. `notify`
    /// stays one-shot: deadline polling calls it in a loop and must not fill
    /// the pipe, while signals are rare.
    #[cfg(unix)]
    pub(crate) fn notify_signal(&self) {
        self.notified.store(true, Ordering::Release);
        let byte = [1u8];
        // SAFETY: `writer_fd` refers to the nonblocking socket owned by `self`,
        // and the one-byte write uses a live buffer. `write` is async-signal-safe.
        unsafe {
            libc::write(self.writer_fd, byte.as_ptr().cast(), byte.len());
        }
    }

    #[cfg(unix)]
    pub(crate) fn raw_fd(&self) -> std::os::fd::RawFd {
        self.reader.as_raw_fd()
    }
}

/// Cooperative cancellation state shared with every task in a scope.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
    wake: Arc<CancellationWake>,
}

impl CancellationToken {
    /// Whether a sibling, caller, or the task-group deadline requested stop.
    pub fn is_cancelled(&self) -> bool {
        if self.cancelled.load(Ordering::Acquire) {
            return true;
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.wake.notify();
            return true;
        }
        false
    }

    /// Check cancellation before starting another externally visible action.
    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            bail!("task group cancelled")
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.cancelled.store(true, Ordering::Release);
            self.wake.notify();
            bail!("task group deadline exceeded; external effects may be uncertain")
        }
        Ok(())
    }

    /// Return the deadline shared by this task group, if one was configured.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Clone the shared cancellation flag for runtime backends that consume an
    /// `Arc<AtomicBool>` directly.
    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    /// Set the shared cancellation flag and wake every registered waiter. Prefer
    /// this over storing into [`CancellationToken::cancellation_flag`] directly,
    /// which does not wake blocked waiters.
    #[cfg_attr(not(feature = "graph"), allow(dead_code))]
    pub(crate) fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.cancel_all();
    }

    /// Run `hook` on every [`CancellationToken::request_cancel`] and on the
    /// scope's cancellation. Blocking waiters use this to notify their condition
    /// variable under the lock they re-check the flag with.
    #[cfg_attr(not(feature = "graph"), allow(dead_code))]
    pub(crate) fn on_cancel(&self, hook: CancelHook) {
        self.wake.on_cancel(hook);
    }

    /// Shared event descriptor used by native supervisors to wake on group
    /// cancellation without periodic polling.
    pub(crate) fn cancellation_wake(&self) -> Arc<CancellationWake> {
        Arc::clone(&self.wake)
    }
}

/// An event count a waiter can block on, so one wait can cover several sources
/// (worker exits, queue pushes and closes, cancellation) without a timed poll.
///
/// Every state change that could satisfy a waiter's predicate calls `notify`.
/// A waiter reads `epoch` before it checks its predicate, then calls
/// `wait_past` with that value. A change after the read moves the epoch, so
/// the wait returns at once: no wake-up can be lost between check and wait.
#[derive(Default)]
pub(crate) struct ScopeSignal {
    epoch: Mutex<u64>,
    changed: Condvar,
    #[cfg(test)]
    wakeups: AtomicUsize,
}

impl ScopeSignal {
    /// The current event count. Read it before checking the predicate.
    #[cfg_attr(not(feature = "graph"), allow(dead_code))]
    pub(crate) fn epoch(&self) -> u64 {
        *self.epoch.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a state change and wake every waiter.
    pub(crate) fn notify(&self) {
        let mut epoch = self.epoch.lock().unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.wrapping_add(1);
        self.changed.notify_all();
    }

    /// Block until the epoch differs from `observed`, or `until` passes. Returns
    /// immediately if a change already happened after `observed` was read. The
    /// caller re-checks its predicate after every return.
    #[cfg_attr(not(feature = "graph"), allow(dead_code))]
    pub(crate) fn wait_past(&self, observed: u64, until: Instant) {
        let mut epoch = self.epoch.lock().unwrap_or_else(|e| e.into_inner());
        while *epoch == observed {
            let now = Instant::now();
            if now >= until {
                return;
            }
            let (next, _) = self
                .changed
                .wait_timeout(epoch, until - now)
                .unwrap_or_else(|e| e.into_inner());
            epoch = next;
            #[cfg(test)]
            self.wakeups.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Number of condition-variable returns from `wait_past` (notified, timed
    /// out or spurious), counted for structural tests.
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> usize {
        self.wakeups.load(Ordering::Relaxed)
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct QueueState {
    jobs: VecDeque<Job>,
    shutdown: bool,
}

struct WorkerQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

struct Worker {
    done: Receiver<()>,
}

/// A typed result receiver for one child owned by a [`TaskScope`].
///
/// Handles are intentionally not detached: the scope retains and joins the
/// underlying worker even if the caller drops this value.
pub struct TaskHandle<T> {
    id: usize,
    result: Option<Receiver<Result<T>>>,
    finished: Arc<AtomicBool>,
}

impl<T> TaskHandle<T> {
    /// Stable, scope-local task identifier.
    pub fn id(&self) -> usize {
        self.id
    }

    /// Whether this task has sent its final result.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

// TaskHandle's finished bit allows polling without consuming its result.
impl<T> TaskHandle<T> {
    fn with_finished(id: usize, result: Receiver<Result<T>>, finished: Arc<AtomicBool>) -> Self {
        Self {
            id,
            result: Some(result),
            finished,
        }
    }
}

/// A concurrent task scope with a hard cap on active workers.
///
/// A scope never detaches work. `cancel_and_join` and `Drop` signal all tasks
/// and reap every thread. Each worker is responsible for honoring its token;
/// runtime applet calls use that token to stop and reap subprocesses.
pub struct TaskScope {
    max_parallel: usize,
    deadline: Option<Instant>,
    cancelled: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    next_id: usize,
    workers: BTreeMap<usize, Worker>,
    queue: Arc<WorkerQueue>,
    pool: Vec<JoinHandle<()>>,
    wake: Arc<CancellationWake>,
    /// Notified when a worker finishes and on every cancellation.
    exits: Arc<ScopeSignal>,
}

impl TaskScope {
    /// Create an owned worker scope. `max_parallel` must be nonzero.
    pub fn new(max_parallel: usize, deadline: Option<Instant>) -> Result<Self> {
        Self::new_with_cancellation(max_parallel, deadline, Arc::new(AtomicBool::new(false)))
    }

    /// Create a scope using a cancellation flag supplied by its owner.
    pub fn new_with_cancellation(
        max_parallel: usize,
        deadline: Option<Instant>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self> {
        if max_parallel == 0 {
            bail!("task group max_parallel must be greater than zero")
        }
        let wake = Arc::new(CancellationWake::new()?);
        let exits = Arc::new(ScopeSignal::default());
        let hook_exits = Arc::clone(&exits);
        wake.on_cancel(Arc::new(move || hook_exits.notify()));
        Ok(Self {
            max_parallel,
            deadline,
            cancelled,
            active: Arc::new(AtomicUsize::new(0)),
            next_id: 0,
            workers: BTreeMap::new(),
            queue: Arc::new(WorkerQueue {
                state: Mutex::new(QueueState {
                    jobs: VecDeque::new(),
                    shutdown: false,
                }),
                ready: Condvar::new(),
            }),
            pool: Vec::new(),
            wake,
            exits,
        })
    }

    /// Signal notified whenever a worker of this scope exits or the scope is
    /// cancelled. A waiter reads its epoch, checks its predicate, then waits on
    /// it with [`ScopeSignal::wait_past`].
    #[cfg_attr(not(feature = "graph"), allow(dead_code))]
    pub(crate) fn exit_signal(&self) -> Arc<ScopeSignal> {
        Arc::clone(&self.exits)
    }

    /// The maximum number of tasks allowed to execute concurrently.
    pub fn max_parallel(&self) -> usize {
        self.max_parallel
    }

    /// Current count of executing tasks.
    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Cancellation token shared with this scope's children.
    pub fn cancellation_token(&self) -> CancellationToken {
        CancellationToken {
            cancelled: Arc::clone(&self.cancelled),
            deadline: self.deadline,
            wake: Arc::clone(&self.wake),
        }
    }

    /// Whether the scope cancellation flag has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token().is_cancelled()
    }

    /// Request cooperative cancellation for every child.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.cancel_all();
    }

    /// Start a child if the configured concurrency bound has capacity.
    ///
    /// The closure must own its inputs and observe the token while it runs.
    /// A spawn attempted at capacity fails immediately, allowing schedulers to
    /// decide which ready task should wait. Panics become ordinary task errors.
    pub fn spawn<T, F>(&mut self, work: F) -> Result<TaskHandle<T>>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Result<T> + Send + 'static,
    {
        self.cancellation_token().check()?;
        let id = self.next_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task group exhausted task identifiers"))?;
        let mut active = self.active.load(Ordering::Acquire);
        loop {
            if active >= self.max_parallel {
                bail!(
                    "task group concurrency limit ({}) reached",
                    self.max_parallel
                )
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => active = observed,
            }
        }

        self.next_id = next_id;
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::clone(&self.cancelled);
        let wake = Arc::clone(&self.wake);
        let deadline = self.deadline;
        let worker_active = Arc::clone(&self.active);
        let worker_exits = Arc::clone(&self.exits);
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        // Grow the fixed pool only as concurrency is requested. In particular,
        // sequential tasks reuse one small-stack worker instead of creating a
        // fresh OS thread for every element.
        if self.pool.len() < active + 1 {
            let queue = Arc::clone(&self.queue);
            let worker_id = self.pool.len();
            let thread = thread::Builder::new()
                .name(format!("flow-task-{worker_id}"))
                .stack_size(512 * 1024)
                .spawn(move || worker_loop(queue));
            match thread {
                Ok(thread) => self.pool.push(thread),
                Err(error) => {
                    self.active.fetch_sub(1, Ordering::AcqRel);
                    return Err(error.into());
                }
            }
        }
        let (done_sender, done_receiver) = mpsc::channel();
        let job: Job = Box::new(move || {
            let token = CancellationToken {
                cancelled,
                deadline,
                wake,
            };
            let result = catch_unwind(AssertUnwindSafe(|| work(token)))
                .unwrap_or_else(|payload| Err(task_panic(payload)));
            let _ = sender.send(result);
            worker_finished.store(true, Ordering::Release);
            worker_active.fetch_sub(1, Ordering::AcqRel);
            let _ = done_sender.send(());
            worker_exits.notify();
        });
        {
            let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
            state.jobs.push_back(job);
        }
        self.queue.ready.notify_one();
        self.workers.insert(
            id,
            Worker {
                done: done_receiver,
            },
        );
        Ok(TaskHandle::with_finished(id, receiver, finished))
    }

    /// Join a task, preserving its typed value or task error.
    pub fn join<T>(&mut self, handle: TaskHandle<T>) -> Result<T> {
        self.join_until(handle, None)
    }

    /// Join a task while applying the caller's current deadline. This lets a
    /// reusable scope serve successive live nodes without retaining the first
    /// node's deadline for later waits.
    pub fn join_until<T>(
        &mut self,
        mut handle: TaskHandle<T>,
        deadline: Option<Instant>,
    ) -> Result<T> {
        let receiver = handle
            .result
            .take()
            .expect("task result receiver is present");
        let received = if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(remaining) {
                Ok(result) => Ok(result),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.cancel();
                    let _ = receiver.recv();
                    let _ = self.reap(handle.id);
                    return Err(TaskDeadlineExceeded.into());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(anyhow!("task worker exited without a result"))
                }
            }
        } else {
            receiver
                .recv()
                .map_err(|_| anyhow!("task worker exited without a result"))
        };
        let received = received?;
        let reaped = self.reap(handle.id);
        let result = received?;
        reaped?;
        Ok(result)
    }

    /// Poll a task without blocking. Its result is consumed only when ready.
    pub fn try_join<T>(&mut self, handle: &mut TaskHandle<T>) -> Result<Option<T>> {
        let Some(result) = handle.result.as_ref() else {
            bail!("task result was already consumed")
        };
        match result.try_recv() {
            Ok(result) => {
                handle.result.take();
                self.reap(handle.id)?;
                result.map(Some)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                handle.result.take();
                self.reap(handle.id)?;
                bail!("task worker exited without a result")
            }
        }
    }

    /// Cancel and join every child owned by this scope.
    pub fn cancel_and_join(&mut self) -> Result<()> {
        self.cancel();
        self.join_workers()
    }

    /// Join completed and running tasks, leaving the scope reusable.
    pub fn join_all(&mut self) -> Result<()> {
        self.join_workers()
    }

    fn reap(&mut self, id: usize) -> Result<()> {
        if let Some(worker) = self.workers.remove(&id) {
            worker
                .done
                .recv()
                .map_err(|_| anyhow!("task worker exited without completion"))?;
        }
        Ok(())
    }

    fn join_workers(&mut self) -> Result<()> {
        let mut first_error = None;
        for (_, worker) in std::mem::take(&mut self.workers) {
            if worker.done.recv().is_err() {
                first_error.get_or_insert_with(|| anyhow!("task worker exited without completion"));
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

fn worker_loop(queue: Arc<WorkerQueue>) {
    loop {
        let job = {
            let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.jobs.is_empty() && !state.shutdown {
                state = queue.ready.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.shutdown && state.jobs.is_empty() {
                return;
            }
            state.jobs.pop_front()
        };
        if let Some(job) = job {
            let _ = catch_unwind(AssertUnwindSafe(job));
        }
    }
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        self.cancel();
        let _ = self.join_workers();
        {
            let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
            state.shutdown = true;
        }
        self.queue.ready.notify_all();
        for worker in self.pool.drain(..) {
            let _ = worker.join();
        }
    }
}

/// The caller's total deadline expired while it awaited a task. The task was
/// cancelled and reaped, and it may already have started external effects.
#[derive(Debug)]
pub(crate) struct TaskDeadlineExceeded;

impl std::fmt::Display for TaskDeadlineExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("task group deadline exceeded; external effects may be uncertain")
    }
}

impl std::error::Error for TaskDeadlineExceeded {}

fn task_panic(payload: Box<dyn Any + Send>) -> anyhow::Error {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    anyhow!("task worker panicked: {message}")
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn expired_await_deadline_on_blocked_task_is_typed() -> Result<()> {
        let mut scope = TaskScope::new(1, None)?;
        let handle = scope.spawn(|token| {
            while !token.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(())
        })?;
        let error = scope
            .join_until(handle, Some(Instant::now()))
            .expect_err("a blocked task must not finish before the deadline");
        assert!(error.is::<TaskDeadlineExceeded>());
        Ok(())
    }

    #[test]
    fn unexpected_task_failure_is_not_a_deadline() -> Result<()> {
        let mut scope = TaskScope::new(1, None)?;
        let handle = scope.spawn(|_| -> Result<()> { bail!("worker broke") })?;
        let error = scope
            .join_until(
                handle,
                Some(Instant::now() + std::time::Duration::from_secs(10)),
            )
            .expect_err("worker failure must propagate");
        assert!(!error.is::<TaskDeadlineExceeded>());
        Ok(())
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use std::sync::Barrier;

    #[cfg(unix)]
    #[test]
    fn cancellation_notifies_native_supervisor_wake_descriptor() -> Result<()> {
        let scope = TaskScope::new(1, None)?;
        let wake = scope.cancellation_token().cancellation_wake();
        let (ready, _) = crate::platform::unix_ffi::poll_one(wake.raw_fd(), libc::POLLIN, 0)?;
        assert_eq!(ready, 0);

        scope.cancel();

        let (ready, events) = crate::platform::unix_ffi::poll_one(wake.raw_fd(), libc::POLLIN, 0)?;
        assert_eq!(ready, 1);
        assert_ne!(events & libc::POLLIN, 0);
        Ok(())
    }

    #[test]
    fn joined_workers_do_not_accumulate() -> Result<()> {
        let mut scope = TaskScope::new(2, None)?;
        for _ in 0..1_000 {
            let handle = scope.spawn(|_| Ok(()))?;
            scope.join(handle)?;
            assert!(scope.workers.is_empty());
        }
        assert_eq!(scope.pool.len(), 1);
        Ok(())
    }

    #[test]
    fn many_tasks_reuse_a_bounded_pool() -> Result<()> {
        let mut scope = TaskScope::new(64, None)?;
        let started = Arc::new(Barrier::new(65));
        let mut handles = Vec::with_capacity(10_000);
        for _ in 0..64 {
            let worker_started = Arc::clone(&started);
            handles.push(scope.spawn(move |_| {
                worker_started.wait();
                Ok(())
            })?);
        }
        started.wait();
        assert_eq!(scope.pool.len(), 64);
        for handle in handles {
            scope.join(handle)?;
        }
        for _ in 0..9_936 {
            let handle = scope.spawn(|_| Ok(()))?;
            scope.join(handle)?;
        }
        assert_eq!(scope.pool.len(), 64);
        Ok(())
    }
}

#[cfg(test)]
mod issue_556_wait_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn idle_scope_signal_wait_does_not_poll() {
        let signal = ScopeSignal::default();
        let observed = signal.epoch();
        let started = Instant::now();
        signal.wait_past(observed, started + Duration::from_millis(300));
        assert!(
            started.elapsed() >= Duration::from_millis(280),
            "wait returned before its bound"
        );
        // A timed wait can return a little early and be re-armed, more often on a loaded
        // machine; a 2 ms poll would return about 150 times in this window.
        assert!(
            signal.wakeups() <= 20,
            "idle wait returned {} times in 300 ms",
            signal.wakeups()
        );
    }

    #[test]
    fn change_after_epoch_read_does_not_block() {
        let signal = ScopeSignal::default();
        let observed = signal.epoch();
        signal.notify();
        let started = Instant::now();
        signal.wait_past(observed, started + Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(signal.wakeups(), 0);
    }

    #[test]
    fn worker_exit_wakes_exit_waiter_without_polling() -> Result<()> {
        let mut scope = TaskScope::new(1, None)?;
        let exits = scope.exit_signal();
        let handle = scope.spawn(|_| {
            thread::sleep(Duration::from_millis(100));
            Ok(())
        })?;
        let started = Instant::now();
        loop {
            let observed = exits.epoch();
            if handle.is_finished() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "worker exit was never signalled"
            );
            exits.wait_past(observed, started + Duration::from_secs(5));
        }
        let wakeups = exits.wakeups();
        // One notification is expected; early timed returns under load are tolerated, a
        // polling loop (dozens of returns in 100 ms) is not.
        assert!(wakeups <= 20, "worker exit woke the waiter {wakeups} times");
        scope.join(handle)?;
        Ok(())
    }

    #[test]
    fn cancel_wakes_exit_waiter_promptly() -> Result<()> {
        let scope = TaskScope::new(1, None)?;
        let exits = scope.exit_signal();
        let token = scope.cancellation_token();
        let waiter = thread::spawn(move || {
            let started = Instant::now();
            loop {
                let observed = exits.epoch();
                if token.is_cancelled() {
                    return started.elapsed();
                }
                exits.wait_past(observed, started + Duration::from_secs(10));
            }
        });
        thread::sleep(Duration::from_millis(100));
        scope.cancel();
        let elapsed = waiter
            .join()
            .map_err(|_| anyhow!("waiter thread panicked"))?;
        assert!(
            elapsed < Duration::from_secs(1),
            "cancel woke the waiter after {elapsed:?}"
        );
        Ok(())
    }
}
