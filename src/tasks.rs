//! Structured ownership for concurrent applet and graph workers.
//!
//! A [`TaskScope`] owns every worker it starts. Dropping the scope requests
//! cancellation and joins all children, so a task cannot silently outlive its
//! group. Work functions receive a [`CancellationToken`] and must pass it into
//! the runtime operation they supervise. Applet-specific policy and contract
//! preflight belongs to the caller and must complete before spawning work.

use anyhow::{anyhow, bail, Result};
use std::any::Any;
#[cfg(debug_assertions)]
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

/// Cooperative cancellation state shared with every task in a scope.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

impl CancellationToken {
    /// Whether a sibling, caller, or the task-group deadline requested stop.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
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
}

struct Worker {
    join: Option<JoinHandle<()>>,
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
    workers: Vec<Worker>,
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
        Ok(Self {
            max_parallel,
            deadline,
            cancelled,
            active: Arc::new(AtomicUsize::new(0)),
            next_id: 0,
            workers: Vec::new(),
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
        }
    }

    /// Whether the scope cancellation flag has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token().is_cancelled()
    }

    /// Request cooperative cancellation for every child.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
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
        let deadline = self.deadline;
        let worker_active = Arc::clone(&self.active);
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let thread = thread::Builder::new()
            .name(format!("flow-task-{id}"))
            .spawn(move || {
                let token = CancellationToken {
                    cancelled,
                    deadline,
                };
                #[cfg(debug_assertions)]
                let result = catch_unwind(AssertUnwindSafe(|| work(token)))
                    .unwrap_or_else(|payload| Err(task_panic(payload)));
                #[cfg(not(debug_assertions))]
                let result = work(token);
                let _ = sender.send(result);
                worker_finished.store(true, Ordering::Release);
                worker_active.fetch_sub(1, Ordering::AcqRel);
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                self.active.fetch_sub(1, Ordering::AcqRel);
                return Err(error.into());
            }
        };
        self.workers.push(Worker { join: Some(thread) });
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
        let Some(worker) = self.workers.get_mut(id) else {
            return Ok(());
        };
        if let Some(thread) = worker.join.take() {
            thread
                .join()
                .map_err(task_panic)
                .map_err(|error| anyhow!("task worker panicked while reaping: {error}"))?;
        }
        Ok(())
    }

    fn join_workers(&mut self) -> Result<()> {
        let mut first_error = None;
        for worker in &mut self.workers {
            if let Some(thread) = worker.join.take() {
                if let Err(payload) = thread.join() {
                    first_error.get_or_insert_with(|| {
                        anyhow!(
                            "task worker panicked while reaping: {}",
                            task_panic(payload)
                        )
                    });
                }
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.join_workers();
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
