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

/// A one-shot wake descriptor shared by a task group and its native children.
/// Unix supervisors poll this beside their pipes and child-exit handles, so an
/// idle group sleeps until output, process exit, or cancellation is ready.
#[derive(Debug)]
pub(crate) struct CancellationWake {
    notified: AtomicBool,
    #[cfg(unix)]
    reader: std::os::unix::net::UnixStream,
    #[cfg(unix)]
    writer: Mutex<std::os::unix::net::UnixStream>,
    #[cfg(unix)]
    writer_fd: RawFd,
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
                reader,
                writer: Mutex::new(writer),
                writer_fd,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                notified: AtomicBool::new(false),
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

    /// Signal-handler variant: this uses only lock-free atomics and `write(2)`.
    #[cfg(unix)]
    pub(crate) fn notify_signal(&self) {
        if self.notified.swap(true, Ordering::AcqRel) {
            return;
        }
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

    /// Shared event descriptor used by native supervisors to wake on group
    /// cancellation without periodic polling.
    pub(crate) fn cancellation_wake(&self) -> Arc<CancellationWake> {
        Arc::clone(&self.wake)
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
        })
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
        self.wake.notify();
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
    pub fn join<T>(&mut self, mut handle: TaskHandle<T>) -> Result<T> {
        let received = handle
            .result
            .take()
            .expect("task result receiver is present")
            .recv()
            .map_err(|_| anyhow!("task worker exited without a result"));
        let reaped = self.reap(handle.id);
        let result = received?;
        reaped?;
        result
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

fn task_panic(payload: Box<dyn Any + Send>) -> anyhow::Error {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    anyhow!("task worker panicked: {message}")
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
