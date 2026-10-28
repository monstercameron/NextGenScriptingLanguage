//! Bounded applet task groups used by finite `.program` workflows.

use super::checkpoint::elements::ElementOutcome;
use super::{AppletInvocationContext, AppletInvocationResult, RunOptions};
use crate::ast::{Invocation, TaskEach, TaskGroupMode, Value};
use crate::contracts::{Outcome, OutcomeKind};
use crate::manifest::{AppletBackend, Registry};
use crate::modules::ProgramModuleSession;
use crate::tasks::{CancellationToken, TaskHandle, TaskScope};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_TASKS: usize = 256;
const MAX_PARALLEL: usize = 64;
/// Element bound for `parallel for each`; matches the scale of a worker pool
/// over a file of inputs while keeping preflight and result records bounded.
const MAX_EACH_ITEMS: usize = 10_000;
const GROUP_WAIT_INTERVAL: Duration = Duration::from_millis(100);

fn min_deadline(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

fn any_cancelled(flags: &[Arc<AtomicBool>]) -> bool {
    flags
        .iter()
        .any(|cancelled| cancelled.load(Ordering::Acquire))
}

#[derive(Clone)]
struct PreparedTask {
    id: String,
    applet: String,
    parameters: std::collections::BTreeMap<String, Value>,
    uncertain_on_error: bool,
    native_command: bool,
    sensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskState {
    Pending,
    Running,
    Succeeded,
    NonSuccess,
    Failed,
    Cancelled,
    NotStarted,
}

impl TaskState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "success",
            Self::NonSuccess => "non_success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::NotStarted => "not_started",
        }
    }
}

struct TaskRecord {
    id: String,
    state: TaskState,
    invocation: Option<Box<AppletInvocationResult>>,
    launched_processes: usize,
    error: Option<String>,
    uncertain: bool,
    sensitive: bool,
}

impl TaskRecord {
    fn pending(task: &PreparedTask) -> Self {
        Self {
            id: task.id.clone(),
            state: TaskState::Pending,
            invocation: None,
            launched_processes: 0,
            error: None,
            uncertain: false,
            sensitive: task.sensitive,
        }
    }
}

/// Failure raised when a joined task group contains an execution error or a
/// non-success applet outcome. `partial_results` preserves every known result
/// plus cancellation and uncertainty state for the rest of the group. For
/// `parallel for each`, `partial_results_schema_version` identifies the
/// compact record encoding used in this list.
#[derive(Debug)]
pub struct TaskGroupFailure {
    pub mode: TaskGroupMode,
    pub failed_task: String,
    pub message: String,
    pub outcome: Option<Outcome>,
    pub uncertain: bool,
    pub partial_results: Value,
    /// Encoding version for compact `parallel for each` partial results.
    /// Ordinary `parallel` and `race` partial results retain their legacy
    /// `u8_array` representation and report `None`.
    pub partial_results_schema_version: Option<u8>,
    pub launched_processes: usize,
}

impl std::fmt::Display for TaskGroupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} task group failed at task {:?}: {} (uncertain effects: {})",
            self.mode, self.failed_task, self.message, self.uncertain
        )
    }
}

impl std::error::Error for TaskGroupFailure {}

/// The completed task-group value and its process accounting for the enclosing
/// finite workflow result.
#[derive(Debug)]
pub(crate) struct TaskGroupResult {
    pub value: Value,
    pub launched_processes: usize,
}

pub(super) struct TaskGroupConfig {
    pub mode: TaskGroupMode,
    pub limit: usize,
    pub outer_deadline: Option<Instant>,
}

/// Everything a task group needs from its step besides the tasks themselves,
/// shared by `run_group` and `run_each`.
pub(super) struct TaskGroupRequest<'a> {
    pub(super) config: TaskGroupConfig,
    pub(super) registry: &'a Registry,
    pub(super) bindings: &'a BTreeMap<String, Value>,
    pub(super) sensitive_items: bool,
    pub(super) sensitive_environment: bool,
    pub(super) options: &'a RunOptions,
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) draining: Arc<std::sync::atomic::AtomicU8>,
    pub(super) context: AppletInvocationContext,
    pub(super) environment: Option<crate::capabilities::CommandEnvOverlay>,
}

/// Receives the begin and the successful result of each parallel element, so the
/// checkpoint can record them. Called on the thread that runs the group.
pub(in crate::runtime) trait GroupSink {
    fn begin(&mut self, index: usize) -> Result<()>;
    fn commit(&mut self, index: usize, outcome: &ElementOutcome) -> Result<()>;
}

/// Elements restored from a checkpoint, and the sink for new element records.
pub(in crate::runtime) struct ElementPlan<'a> {
    pub skip: BTreeMap<usize, ElementOutcome>,
    pub sink: Option<&'a mut dyn GroupSink>,
}

impl ElementPlan<'_> {
    /// No checkpoint: every element runs and nothing is recorded.
    pub(in crate::runtime) fn none() -> Self {
        Self {
            skip: BTreeMap::new(),
            sink: None,
        }
    }
}

struct Completion {
    index: usize,
    result: Result<AppletInvocationResult>,
    cancelled: bool,
    launched_processes: usize,
}

/// Resolve and validate every task's applet, parameters, and capability grant
/// before starting any task. Parent bindings are resolved once into owned
/// values so a child cannot observe later sibling results.
pub(super) fn preflight_group(
    tasks: &[Invocation],
    registry: &Registry,
    bindings: &std::collections::BTreeMap<String, Value>,
    cwd: &Path,
    module_session: &ProgramModuleSession,
) -> Result<Vec<(String, Invocation)>> {
    if tasks.is_empty() || tasks.len() > MAX_TASKS {
        bail!("task group must contain between 1 and {MAX_TASKS} tasks")
    }
    let mut names = BTreeSet::new();
    let mut resolved_calls = Vec::with_capacity(tasks.len());
    let mut names_and_calls = Vec::with_capacity(tasks.len());
    for task in tasks {
        if task.direct
            || task.cache
            || task.status_binding.is_some()
            || task.result_binding.is_some()
        {
            bail!(
                "task groups require named, uncached applet calls without status or result capture"
            )
        }
        let id = task
            .binding
            .as_deref()
            .filter(|name| valid_task_identifier(name))
            .ok_or_else(|| {
                anyhow!("each task group applet call requires a valid `as <task>` identifier")
            })?;
        if !names.insert(id.to_owned()) {
            bail!("duplicate task identifier {id:?}")
        }
        let parameters = task
            .parameters
            .iter()
            .map(|(name, value)| Ok((name.clone(), super::resolve(value, bindings)?)))
            .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
        let mut resolved = Invocation {
            operation: task.operation.clone(),
            direct: false,
            discard_stderr: false,
            parameters,
            binding: None,
            cache: false,
            sensitive_binding: task.marks_sensitive_output(),
            status_binding: None,
            result_binding: None,
            retry: None,
        };
        let manifest = registry
            .resolve_call(&resolved)
            .with_context(|| format!("task {id:?}"))?;
        resolved.sensitive_binding |= crate::security::applet_has_declared_secret_environment(
            registry,
            manifest.name_for(&resolved),
        );
        manifest
            .validate(&resolved)
            .with_context(|| format!("task {id:?}"))?;
        if manifest.builtin.is_some() {
            bail!("task {id:?}: runtime directory builtins cannot run as independent tasks")
        }
        if manifest.execution_backend() == AppletBackend::JsonLinesWorker
            && !super::worker_backend::WorkerPool::supported()
        {
            bail!("task {id:?}: json_lines_worker backend is unsupported on this platform")
        }
        names_and_calls.push((id.to_owned(), resolved.clone()));
        resolved_calls.push(resolved);
    }
    crate::capabilities::preflight_invocations(resolved_calls.iter(), registry, cwd)
        .context("preflighting task group capabilities")?;
    for (scope, invocation) in module_session.scoped_body_invocations() {
        crate::capabilities::preflight_invocations(std::iter::once(invocation), scope, cwd)
            .with_context(|| format!("preflighting module body call {:?}", invocation.operation))?;
    }
    Ok(names_and_calls)
}

/// Execute a task group after validating every task before any worker starts.
/// The returned `Value` is a typed record; task outputs are preserved as
/// explicit `u8_array` fields when the applet has no typed output schema.
pub(super) fn run_group(
    request: TaskGroupRequest<'_>,
    tasks: &[Invocation],
) -> Result<TaskGroupResult> {
    let TaskGroupRequest {
        config,
        registry,
        bindings,
        sensitive_items: _,
        sensitive_environment,
        options,
        cancelled,
        draining,
        context,
        environment,
    } = request;
    let mut context = context;
    context.outer_cancelled.push(Arc::clone(&cancelled));
    let outer_cancelled = context.outer_cancelled.clone();
    let TaskGroupConfig {
        mode,
        limit,
        outer_deadline,
    } = config;
    context.deadline = min_deadline(context.deadline, outer_deadline);
    if any_cancelled(&outer_cancelled) {
        bail!("task group was cancelled before preflight")
    }
    if !(1..=MAX_PARALLEL).contains(&limit) {
        bail!("task group limit must be between 1 and {MAX_PARALLEL}")
    }
    if tasks.is_empty() || tasks.len() > MAX_TASKS {
        bail!("task group must contain between 1 and {MAX_TASKS} tasks")
    }
    let prepared = preflight_group(
        tasks,
        registry,
        bindings,
        &options.working_directory,
        &context.module_session,
    )?
    .into_iter()
    .map(|(id, call)| {
        let manifest = registry.resolve_call(&call)?;
        let sensitive = call.marks_sensitive_output()
            || sensitive_environment
            || crate::security::applet_has_declared_secret_environment(
                registry,
                manifest.name_for(&call),
            );
        Ok(PreparedTask {
            id,
            applet: manifest.name.clone(),
            parameters: call.parameters,
            uncertain_on_error: manifest.uncertain_on_failure,
            native_command: manifest.execution_backend() == AppletBackend::Command,
            sensitive,
        })
    })
    .collect::<Result<Vec<_>>>()?;
    // Each child receives a statically bounded share of the group's output
    // budget. This caps aggregate captured stdout even when all children finish
    // before the parent has a chance to account for their results.
    let per_task_output_limit = options.max_output_bytes / prepared.len();
    if per_task_output_limit == 0 {
        bail!(
            "task group output limit of {} bytes is too small for {} tasks",
            options.max_output_bytes,
            prepared.len()
        );
    }
    let limit = limit.min(prepared.len());
    let max_output_bytes = options.max_output_bytes;
    let secret_redactor = crate::security::secret_redactor(registry);
    let mut records = prepared.iter().map(TaskRecord::pending).collect::<Vec<_>>();
    let not_started = records
        .iter()
        .map(|record| TaskRecord {
            id: record.id.clone(),
            state: TaskState::NotStarted,
            invocation: None,
            launched_processes: 0,
            error: None,
            uncertain: false,
            sensitive: record.sensitive,
        })
        .collect::<Vec<_>>();
    ensure_encoded_limit(
        &group_value(mode, &not_started, None, &secret_redactor),
        max_output_bytes,
    )
    .context("task group result metadata exceeds the configured output limit")?;
    let deadline = outer_deadline;
    let group_cancelled = Arc::new(AtomicBool::new(false));
    let mut scope =
        TaskScope::new_with_cancellation(limit, deadline, Arc::clone(&group_cancelled))?;
    let (sender, receiver) = mpsc::channel();
    let mut handles: Vec<Option<TaskHandle<()>>> = (0..prepared.len()).map(|_| None).collect();
    let mut total_output_bytes = 0usize;
    let mut next = 0usize;
    let mut active = 0usize;
    let mut winner = None;
    let mut first_failure: Option<(usize, String, Option<Outcome>, bool)> = None;

    while next < prepared.len() || active > 0 {
        while active < limit && next < prepared.len() && winner.is_none() {
            if draining.load(Ordering::SeqCst) != crate::platform::DRAIN_IDLE {
                first_failure.get_or_insert_with(|| {
                    (
                        next,
                        "task group was interrupted before this task started".into(),
                        None,
                        false,
                    )
                });
                break;
            }
            if group_cancelled.load(Ordering::Acquire) || any_cancelled(&outer_cancelled) {
                first_failure.get_or_insert_with(|| {
                    (
                        next,
                        "task group was cancelled before this task started".into(),
                        None,
                        false,
                    )
                });
                break;
            }
            let index = next;
            let task = prepared[index].clone();
            let registry = registry.clone();
            let context = context.clone();
            let inherited_cancelled = context.outer_cancelled.clone();
            let environment = environment.clone();
            let parent_cancelled = Arc::clone(&cancelled);
            let mut task_options = options.clone();
            task_options.max_output_bytes =
                task_options.max_output_bytes.min(per_task_output_limit);
            if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                task_options.total_timeout = Some(
                    task_options
                        .total_timeout
                        .map_or(remaining, |duration| duration.min(remaining)),
                );
            }
            let sender = sender.clone();
            let handle = match scope.spawn(move |token| {
                let _output_scope = super::OutputScopeGuard::enter(super::OutputScope::TaskGroup);
                let mut context = context;
                context.cancel_wakes.push(token.cancellation_wake());
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let _environment = environment
                        .map(|overlay| crate::capabilities::CommandEnvScope::enter(Some(overlay)));
                    token.check()?;
                    if parent_cancelled.load(Ordering::Acquire) {
                        bail!("task group was cancelled before task launch")
                    }
                    super::invoke_applet_with_context(
                        &registry,
                        &task.applet,
                        task.parameters,
                        Vec::new(),
                        &task_options,
                        token.cancellation_flag(),
                        context,
                    )
                }))
                .unwrap_or_else(|payload| Err(task_panic(payload)));
                let launched_processes = result
                    .as_ref()
                    .map(|result| result.launched_processes)
                    .unwrap_or_else(|error| {
                        error
                            .downcast_ref::<super::AppletInvocationError>()
                            .map_or(0, super::AppletInvocationError::launched_processes)
                    });
                let cancelled = completion_was_cancelled(&token, &inherited_cancelled);
                sender
                    .send(Completion {
                        index,
                        result,
                        cancelled,
                        launched_processes,
                    })
                    .map_err(|_| anyhow!("task group completion channel closed"))?;
                Ok(())
            }) {
                Ok(handle) => handle,
                Err(error) => {
                    first_failure.get_or_insert_with(|| {
                        (
                            index,
                            format!("could not start task: {error:#}"),
                            None,
                            task.uncertain_on_error,
                        )
                    });
                    break;
                }
            };
            records[index].state = TaskState::Running;
            handles[index] = Some(handle);
            active += 1;
            next += 1;
        }

        if first_failure.is_some() || winner.is_some() {
            break;
        }
        if active == 0 {
            break;
        }
        let completion = match receiver.recv_timeout(GROUP_WAIT_INTERVAL) {
            Ok(completion) => completion,
            Err(RecvTimeoutError::Timeout) if any_cancelled(&outer_cancelled) => {
                let index = handles
                    .iter()
                    .position(Option::is_some)
                    .unwrap_or_else(|| next.saturating_sub(1).min(prepared.len() - 1));
                first_failure = Some((
                    index,
                    "task group was cancelled while tasks were running".into(),
                    None,
                    prepared[index].uncertain_on_error,
                ));
                break;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                bail!("task group worker ended without reporting a result")
            }
        };
        let index = completion.index;
        let handle = handles[index]
            .take()
            .ok_or_else(|| anyhow!("task group received a duplicate completion"))?;
        let launched_processes = completion.launched_processes;
        let joined = scope.join(handle);
        active = active.saturating_sub(1);
        let task = &prepared[index];
        records[index].launched_processes = launched_processes;
        if let Err(error) = joined {
            records[index].state = TaskState::Failed;
            records[index].error = Some(format!("could not reap task worker: {error:#}"));
            records[index].uncertain = task.uncertain_on_error;
            first_failure = Some((
                index,
                format!("could not reap task worker: {error:#}"),
                None,
                task.uncertain_on_error,
            ));
            break;
        }
        match completion.result {
            Ok(result) => {
                if let Err(error) = register_task_result(&secret_redactor, task.sensitive, &result)
                {
                    let message =
                        format!("could not safely redact sensitive task output: {error:#}");
                    records[index].state = TaskState::Failed;
                    records[index].error = Some(message.clone());
                    first_failure = Some((index, message, None, true));
                    break;
                }
                total_output_bytes = total_output_bytes
                    .checked_add(result.output.len())
                    .and_then(|total| total.checked_add(result.stderr.len()))
                    .unwrap_or(usize::MAX);
                let succeeded = result.outcome.kind == OutcomeKind::Success;
                let outcome = result.outcome.clone();
                let status = result.status;
                let uncertain = result.uncertain;
                records[index].uncertain = uncertain;
                records[index].state = if succeeded {
                    TaskState::Succeeded
                } else {
                    TaskState::NonSuccess
                };
                records[index].invocation = Some(Box::new(result));
                if total_output_bytes > max_output_bytes {
                    first_failure = Some((
                        index,
                        format!("task group output exceeded {max_output_bytes} bytes"),
                        Some(outcome),
                        uncertain,
                    ));
                    break;
                }
                if succeeded && mode == TaskGroupMode::Race {
                    winner = Some(index);
                    break;
                }
                if !succeeded && mode == TaskGroupMode::Join {
                    first_failure = Some((
                        index,
                        format!(
                            "applet returned {:?} outcome with status {}",
                            outcome.name, status
                        ),
                        Some(outcome),
                        uncertain,
                    ));
                    break;
                }
            }
            Err(error) => {
                records[index].state = if completion.cancelled {
                    TaskState::Cancelled
                } else {
                    TaskState::Failed
                };
                let error_text = register_task_error(&secret_redactor, task.sensitive, &error)
                    .unwrap_or_else(|_| {
                        "sensitive task failure was withheld because redaction could not be secured"
                            .into()
                    });
                records[index].error = Some(error_text.clone());
                let uncertain = task_error_uncertain(&error, task.uncertain_on_error);
                records[index].uncertain = uncertain;
                if mode == TaskGroupMode::Join {
                    first_failure = Some((
                        index,
                        format!("applet invocation failed: {error_text}"),
                        None,
                        uncertain,
                    ));
                    break;
                }
            }
        }
        if cancelled.load(Ordering::Acquire) && first_failure.is_none() && winner.is_none() {
            first_failure = Some((
                index,
                "task group was cancelled while tasks were running".into(),
                None,
                records[index].uncertain,
            ));
        }
    }

    if first_failure.is_some() || winner.is_some() {
        scope.cancel();
        scope.cancel_and_join()?;
        for (index, handle) in handles.iter_mut().enumerate() {
            if let Some(handle) = handle.take() {
                if let Err(error) = scope.join(handle) {
                    records[index].state = TaskState::Failed;
                    records[index].error = Some(format!("could not reap task worker: {error:#}"));
                    records[index].uncertain = prepared[index].uncertain_on_error;
                }
                if records[index].state == TaskState::Running {
                    records[index].state = TaskState::Cancelled;
                    records[index].uncertain = prepared[index].uncertain_on_error;
                }
            }
        }
        for completion in receiver.try_iter() {
            update_record(
                completion,
                &mut records,
                &prepared,
                &mut total_output_bytes,
                &secret_redactor,
            );
        }
        for record in &mut records {
            if record.state == TaskState::Pending {
                record.state = TaskState::NotStarted;
            }
        }
        if let Some(index) = winner {
            let value = group_value(mode, &records, Some(index), &secret_redactor);
            ensure_encoded_limit(&value, max_output_bytes).map_err(|error| {
                task_group_failure(TaskGroupFailureRequest {
                    mode,
                    failed_task: &prepared[index].id,
                    message: format!("{error:#}"),
                    outcome: records[index]
                        .invocation
                        .as_ref()
                        .map(|result| result.outcome.clone()),
                    uncertain: records[index].uncertain,
                    records: &records,
                    secret_redactor: &secret_redactor,
                    compact: false,
                })
            })?;
            return Ok(TaskGroupResult {
                value,
                launched_processes: launched_processes(&records),
            });
        }
        if let Some((index, message, outcome, uncertain)) = first_failure {
            return Err(task_group_failure(TaskGroupFailureRequest {
                mode,
                failed_task: &prepared[index].id,
                message,
                outcome,
                uncertain,
                records: &records,
                secret_redactor: &secret_redactor,
                compact: false,
            })
            .into());
        }
        // A race with all candidates returning non-success outcomes has no
        // winner. Report the first observed failure without treating absence
        // or retryable outcomes as success.
        if mode == TaskGroupMode::Race {
            let failed_index = records
                .iter()
                .position(|record| {
                    matches!(record.state, TaskState::Failed | TaskState::NonSuccess)
                })
                .unwrap_or(0);
            let outcome = records[failed_index]
                .invocation
                .as_ref()
                .map(|result| result.outcome.clone());
            let message = records[failed_index]
                .error
                .clone()
                .unwrap_or_else(|| "no task produced a success outcome".into());
            return Err(task_group_failure(TaskGroupFailureRequest {
                mode,
                failed_task: &prepared[failed_index].id,
                message,
                outcome,
                uncertain: records[failed_index].uncertain,
                records: &records,
                secret_redactor: &secret_redactor,
                compact: false,
            })
            .into());
        }
    }

    if let Some(record) = records
        .iter()
        .find(|record| record.state != TaskState::Succeeded)
    {
        return Err(task_group_failure(TaskGroupFailureRequest {
            mode,
            failed_task: &record.id,
            message: "task did not complete successfully".into(),
            outcome: record
                .invocation
                .as_ref()
                .map(|result| result.outcome.clone()),
            uncertain: record.uncertain,
            records: &records,
            secret_redactor: &secret_redactor,
            compact: false,
        })
        .into());
    }
    let value = group_value(mode, &records, None, &secret_redactor);
    ensure_encoded_limit(&value, max_output_bytes)?;
    Ok(TaskGroupResult {
        value,
        launched_processes: launched_processes(&records),
    })
}

/// Run one template call per list element with bounded concurrency.
///
/// Every element yields a record: non-success outcomes and launch errors do
/// not stop sibling elements, which is the worker-pool contract. All element
/// calls are resolved, validated and capability-checked before the first one
/// starts. Cancellation, the workflow deadline or the aggregate output limit
/// stop new launches, cancel and reap running elements, and fail the group
/// with the partial records.
pub(super) fn run_each(
    request: TaskGroupRequest<'_>,
    each: &TaskEach,
    template: &Invocation,
    elements: ElementPlan<'_>,
) -> Result<TaskGroupResult> {
    let ElementPlan { skip, mut sink } = elements;
    let TaskGroupRequest {
        config,
        registry,
        bindings,
        sensitive_items,
        sensitive_environment,
        options,
        cancelled,
        draining,
        context,
        environment,
    } = request;
    let mut context = context;
    context.outer_cancelled.push(Arc::clone(&cancelled));
    let outer_cancelled = context.outer_cancelled.clone();
    let TaskGroupConfig {
        limit,
        outer_deadline,
        ..
    } = config;
    context.deadline = min_deadline(context.deadline, outer_deadline);
    if any_cancelled(&outer_cancelled) {
        bail!("parallel for each was cancelled before preflight")
    }
    if !(1..=MAX_PARALLEL).contains(&limit) {
        bail!("task group limit must be between 1 and {MAX_PARALLEL}")
    }
    if bindings.contains_key(&each.binding) {
        bail!("iteration binding {:?} is already defined", each.binding)
    }
    let Value::List(items) = super::resolve(&each.values, bindings)? else {
        bail!("parallel for each requires a JSON array or list of text")
    };
    if items.len() > MAX_EACH_ITEMS {
        bail!(
            "parallel for each accepts at most {MAX_EACH_ITEMS} elements, got {}",
            items.len()
        )
    }

    let mut scoped = bindings.clone();
    let mut prepared = Vec::with_capacity(items.len());
    let mut calls = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        scoped.insert(each.binding.clone(), item.clone());
        let parameters = template
            .parameters
            .iter()
            .map(|(name, value)| Ok((name.clone(), super::resolve(value, &scoped)?)))
            .collect::<Result<BTreeMap<_, _>>>()
            .with_context(|| format!("element {index}"))?;
        let mut call = Invocation {
            operation: template.operation.clone(),
            direct: template.direct,
            discard_stderr: false,
            parameters,
            binding: None,
            cache: false,
            sensitive_binding: template.marks_sensitive_output(),
            status_binding: None,
            result_binding: None,
            retry: None,
        };
        let manifest = registry
            .resolve_call(&call)
            .with_context(|| format!("element {index}"))?;
        call.sensitive_binding |= sensitive_items
            || sensitive_environment
            || crate::security::applet_has_declared_secret_environment(
                registry,
                manifest.name_for(&call),
            );
        manifest
            .validate(&call)
            .with_context(|| format!("element {index}"))?;
        if manifest.builtin.is_some() {
            bail!("runtime directory builtins cannot run as independent tasks")
        }
        if manifest.execution_backend() == AppletBackend::JsonLinesWorker
            && !super::worker_backend::WorkerPool::supported()
        {
            bail!("json_lines_worker backend is unsupported on this platform")
        }
        prepared.push(PreparedTask {
            id: index.to_string(),
            applet: String::new(),
            parameters: BTreeMap::new(),
            uncertain_on_error: manifest.uncertain_on_failure,
            native_command: manifest.execution_backend() == AppletBackend::Command,
            sensitive: call.marks_sensitive_output(),
        });
        calls.push(call);
    }
    crate::capabilities::preflight_invocations(calls.iter(), registry, &options.working_directory)
        .context("preflighting parallel for each capabilities")?;
    for (scope, invocation) in context.module_session.scoped_body_invocations() {
        crate::capabilities::preflight_invocations(
            std::iter::once(invocation),
            scope,
            &options.working_directory,
        )
        .with_context(|| format!("preflighting module body call {:?}", invocation.operation))?;
    }
    // The invocation list already owns every resolved argument map through
    // capability preflight. Move those maps into launch records afterwards so
    // 10,000 elements do not retain a second copy while preflight runs.
    for (task, call) in prepared.iter_mut().zip(calls.drain(..)) {
        task.applet = call.operation;
        task.parameters = call.parameters;
    }
    drop(calls);

    let secret_redactor = crate::security::secret_redactor(registry);
    let mut records = prepared.iter().map(TaskRecord::pending).collect::<Vec<_>>();
    if prepared.is_empty() {
        return Ok(TaskGroupResult {
            value: each_value(&mut records, items, &secret_redactor),
            launched_processes: 0,
        });
    }
    let max_output_bytes = options.max_output_bytes;
    // Reject a result whose static element metadata cannot fit before any
    // command starts. This does not reserve space for captured output: the
    // output budget is enforced separately as elements run.
    ensure_encoded_limit(
        &each_value(&mut records, items.iter().cloned(), &secret_redactor),
        max_output_bytes,
    )
    .context("parallel for each result metadata exceeds the configured output limit")?;
    let direct = template.direct;
    let limit = limit.min(prepared.len());
    // Divide the in-flight budget among active elements, then enforce the true
    // aggregate limit as each completion is collected. The full list length is
    // not relevant when elements run in bounded waves.
    let per_task_output_limit = (max_output_bytes / limit).max(1);
    let group_cancelled = Arc::new(AtomicBool::new(false));
    let mut scope =
        TaskScope::new_with_cancellation(limit, outer_deadline, Arc::clone(&group_cancelled))?;
    let (sender, receiver) = mpsc::channel();
    let mut handles: Vec<Option<TaskHandle<()>>> = (0..prepared.len()).map(|_| None).collect();
    let mut total_output_bytes = 0usize;
    // Restored elements already ran in the checkpointed run: they count against the
    // output budget and are never started again.
    for (&index, outcome) in &skip {
        total_output_bytes = total_output_bytes
            .saturating_add(outcome.output.len())
            .saturating_add(outcome.stderr.len());
        records[index] = TaskRecord {
            id: prepared[index].id.clone(),
            state: TaskState::Succeeded,
            invocation: Some(Box::new(outcome.clone().into_result())),
            launched_processes: outcome.launched_processes,
            error: None,
            uncertain: outcome.uncertain,
            sensitive: prepared[index].sensitive,
        };
    }
    // Restored outputs count against this run's limit before new effects start.
    // A smaller limit on resume must not admit a new element into an already
    // exhausted group.
    if total_output_bytes > max_output_bytes {
        for record in &mut records {
            if record.state == TaskState::Pending {
                record.state = TaskState::NotStarted;
            }
        }
        return Err(task_group_failure(TaskGroupFailureRequest {
            mode: TaskGroupMode::Join,
            failed_task: &prepared[0].id,
            message: format!("restored parallel for each output exceeded {max_output_bytes} bytes"),
            outcome: None,
            uncertain: false,
            records: &records,
            secret_redactor: &secret_redactor,
            compact: true,
        })
        .into());
    }
    // Native element output is charged against one shared budget as it arrives, so
    // an element that would cross the aggregate limit stops then, not at completion.
    let group_budget = Arc::new(std::sync::atomic::AtomicUsize::new(total_output_bytes));
    // `RunOptions` owns its run-input map, so cloning it per element copies
    // every input once per element. Element workers never read those inputs:
    // a nested invocation resolves its own parameters (moved above) and
    // program-module children derive their inputs from them, while run inputs
    // are consumed once by top-level run setup. The only per-element
    // adjustments are the output cap and the deadline-derived timeout, so
    // share one `Arc` per output variant instead of cloning the inputs per
    // element. A run with an outer deadline still builds a per-element copy
    // below so its timeout stays exact.
    //
    // Working directory is deliberately left non-canonical here. Each worker
    // re-resolves it inside `invoke_call_with_context`, so a symlink swap or
    // deletion after admission is observed per element instead of masked by a
    // stale admission-time string (a TOCTOU/security regression a one-time
    // canonicalization would introduce for this optimization).
    //
    // The capped variant is built only when a non-native element can use it;
    // an all-native group would otherwise pay one extra input clone for an
    // `Arc` that is never moved into a worker. This gate changes no value,
    // only whether an unused clone happens.
    let needs_capped = per_task_output_limit < options.max_output_bytes
        && prepared.iter().any(|task| !task.native_command);
    let shared_options: Arc<RunOptions> = Arc::new(options.clone());
    let shared_capped_options: Option<Arc<RunOptions>> = if needs_capped {
        let mut capped = options.clone();
        capped.max_output_bytes = per_task_output_limit;
        Some(Arc::new(capped))
    } else {
        None
    };
    let mut next = 0usize;
    let mut active = 0usize;
    let mut stopped: Option<(usize, String)> = None;

    while next < prepared.len() || active > 0 {
        while active < limit && next < prepared.len() {
            if stopped.is_some() {
                break;
            }
            if skip.contains_key(&next) {
                next += 1;
                continue;
            }
            if draining.load(Ordering::SeqCst) != crate::platform::DRAIN_IDLE {
                // A drain admits no new elements; running ones finish and are reported
                // as not started for the rest.
                next = prepared.len();
                break;
            }
            if scope.is_cancelled() || any_cancelled(&outer_cancelled) {
                stopped.get_or_insert_with(|| (next, "parallel for each was cancelled".into()));
                break;
            }
            let index = next;
            if let Some(sink) = sink.as_deref_mut() {
                sink.begin(index)?;
            }
            // A `parallel for each` element is admitted exactly once. Move its
            // resolved parameters into the worker instead of retaining a
            // second copy of every element's parameter map until the group
            // finishes.
            let applet = std::mem::take(&mut prepared[index].applet);
            let parameters = std::mem::take(&mut prepared[index].parameters);
            let native_command = prepared[index].native_command;
            // `Registry` is internally `Arc`-backed, so this is a refcount
            // bump; the context clone bumps a few small `Arc` vectors while
            // each worker still needs its own mutable context below.
            let registry = registry.clone();
            let mut context = context.clone();
            let environment = environment.clone();
            context.group_output_budget = Some((Arc::clone(&group_budget), max_output_bytes));
            let inherited_cancelled = context.outer_cancelled.clone();
            let parent_cancelled = Arc::clone(&cancelled);
            // Share the run inputs through `Arc`: without an outer deadline
            // the options are identical for every element of a backend kind,
            // so move a refcount bump of a prebuilt variant into the worker
            // instead of copying the input map per element. With an outer
            // deadline each admission snapshots its own remaining time into a
            // per-element copy, preserving the exact admission-time timeout
            // (`total_timeout = min(T, outer - admission_now)`); sharing the
            // unchanged `T` and relying on the worker-start re-stamp would
            // shift every element deadline later by its spawn delay. Native
            // stdout and retained stderr share the incremental group budget,
            // so a native element can use the available budget while other
            // backends retain their conservative per-worker cap.
            let element_options: Arc<RunOptions> = if let Some(deadline) = outer_deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let mut owned = (*shared_options).clone();
                if !native_command {
                    owned.max_output_bytes = owned.max_output_bytes.min(per_task_output_limit);
                }
                owned.total_timeout = Some(
                    owned
                        .total_timeout
                        .map_or(remaining, |duration| duration.min(remaining)),
                );
                Arc::new(owned)
            } else if !native_command {
                if let Some(capped) = shared_capped_options.as_ref() {
                    Arc::clone(capped)
                } else {
                    Arc::clone(&shared_options)
                }
            } else {
                Arc::clone(&shared_options)
            };
            let sender = sender.clone();
            let spawned = scope.spawn(move |token| {
                let _output_scope = super::OutputScopeGuard::enter(super::OutputScope::TaskGroup);
                context.cancel_wakes.push(token.cancellation_wake());
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let _environment = environment
                        .map(|overlay| crate::capabilities::CommandEnvScope::enter(Some(overlay)));
                    token.check()?;
                    if parent_cancelled.load(Ordering::Acquire) {
                        bail!("parallel for each was cancelled before element launch")
                    }
                    super::invoke_call_with_context(
                        super::AppletInvocation {
                            registry: &registry,
                            name: &applet,
                            direct,
                            parameters,
                            input: Vec::new(),
                            options: &element_options,
                        },
                        token.cancellation_flag(),
                        context,
                    )
                }))
                .unwrap_or_else(|payload| Err(task_panic(payload)));
                let launched_processes = result
                    .as_ref()
                    .map(|result| result.launched_processes)
                    .unwrap_or_else(|error| {
                        error
                            .downcast_ref::<super::AppletInvocationError>()
                            .map_or(0, super::AppletInvocationError::launched_processes)
                    });
                let cancelled = completion_was_cancelled(&token, &inherited_cancelled);
                sender
                    .send(Completion {
                        index,
                        result,
                        cancelled,
                        launched_processes,
                    })
                    .map_err(|_| anyhow!("parallel for each completion channel closed"))?;
                Ok(())
            });
            match spawned {
                Ok(handle) => {
                    records[index].state = TaskState::Running;
                    handles[index] = Some(handle);
                    active += 1;
                }
                Err(error) if scope.is_cancelled() || any_cancelled(&outer_cancelled) => {
                    stopped.get_or_insert_with(|| (index, format!("{error:#}")));
                    break;
                }
                Err(error) => {
                    records[index].state = TaskState::Failed;
                    records[index].error = Some(format!("could not start element: {error:#}"));
                }
            }
            next += 1;
        }
        if stopped.is_some() || active == 0 {
            break;
        }
        let completion = match receiver.recv_timeout(GROUP_WAIT_INTERVAL) {
            Ok(completion) => completion,
            Err(RecvTimeoutError::Timeout) if any_cancelled(&outer_cancelled) => {
                let index = handles
                    .iter()
                    .position(Option::is_some)
                    .unwrap_or_else(|| next.saturating_sub(1).min(prepared.len() - 1));
                stopped = Some((index, "parallel for each was cancelled".into()));
                break;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                bail!("parallel for each worker ended without reporting a result")
            }
        };
        let index = completion.index;
        let handle = handles[index]
            .take()
            .ok_or_else(|| anyhow!("parallel for each received a duplicate completion"))?;
        let element_cancelled = completion.cancelled;
        if let Err(error) = scope.join(handle) {
            records[index].state = TaskState::Failed;
            records[index].error = Some(format!("could not reap element worker: {error:#}"));
        }
        active = active.saturating_sub(1);
        update_record(
            completion,
            &mut records,
            &prepared,
            &mut total_output_bytes,
            &secret_redactor,
        );
        if records[index]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("exceeds the shared group limit of"))
        {
            stopped = Some((
                index,
                format!("parallel for each output exceeded {max_output_bytes} bytes"),
            ));
        } else if records[index]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("output exceeds the limit of"))
        {
            stopped = Some((
                index,
                format!(
                    "parallel for each element exceeded its output limit of {max_output_bytes} bytes"
                ),
            ));
        }
        if let Some(sink) = sink.as_deref_mut() {
            if matches!(records[index].state, TaskState::Succeeded) {
                if let Some(result) = &records[index].invocation {
                    let outcome =
                        ElementOutcome::from_result(result, records[index].launched_processes);
                    sink.commit(index, &outcome)?;
                }
            }
        }
        if stopped.is_none() && total_output_bytes > max_output_bytes {
            stopped = Some((
                index,
                format!("parallel for each output exceeded {max_output_bytes} bytes"),
            ));
            break;
        }
        if element_cancelled || scope.is_cancelled() || any_cancelled(&outer_cancelled) {
            stopped.get_or_insert_with(|| (index, "parallel for each was cancelled".into()));
            break;
        }
    }

    if let Some((index, message)) = stopped {
        scope.cancel_and_join()?;
        for (slot, handle) in handles.iter_mut().enumerate() {
            if let Some(handle) = handle.take() {
                if let Err(error) = scope.join(handle) {
                    records[slot].state = TaskState::Failed;
                    records[slot].error = Some(format!("could not reap element worker: {error:#}"));
                }
            }
        }
        for completion in receiver.try_iter() {
            update_record(
                completion,
                &mut records,
                &prepared,
                &mut total_output_bytes,
                &secret_redactor,
            );
        }
        for (slot, record) in records.iter_mut().enumerate() {
            match record.state {
                TaskState::Running => {
                    record.state = TaskState::Cancelled;
                    record.uncertain = prepared[slot].uncertain_on_error;
                }
                TaskState::Pending => record.state = TaskState::NotStarted,
                _ => {}
            }
        }
        return Err(task_group_failure(TaskGroupFailureRequest {
            mode: TaskGroupMode::Join,
            failed_task: &prepared[index.min(prepared.len() - 1)].id,
            message,
            outcome: None,
            uncertain: false,
            records: &records,
            secret_redactor: &secret_redactor,
            compact: true,
        })
        .into());
    }

    // Elements never admitted (a drain stopped admission) report as not started.
    for record in records.iter_mut() {
        if matches!(record.state, TaskState::Pending) {
            record.state = TaskState::NotStarted;
        }
    }
    // All workers are joined; release preflight calls and scheduling state
    // before materializing the potentially large result tree.
    drop(prepared);
    drop(handles);
    drop(scope);
    drop(receiver);
    drop(sender);
    drop(skip);
    let item_count = items.len();
    let value = each_value(&mut records, items, &secret_redactor);
    // `max_output_bytes` accounts for captured stdout and stderr. The result
    // record has a different JSON representation: redaction can expand each
    // captured stderr byte to the 10-byte `[REDACTED]` marker. JSON string
    // escaping can expand unredacted text, and invalid redacted UTF-8 is
    // base64 encoded. The 16x envelope covers those expansions plus per-item
    // metadata, without charging representation overhead to the raw capture
    // budget.
    ensure_encoded_limit(
        &value,
        each_result_encoded_limit(max_output_bytes, item_count),
    )?;
    Ok(TaskGroupResult {
        value,
        launched_processes: launched_processes(&records),
    })
}

/// `{mode, items, succeeded, failed, cancelled, not_started}`; each element
/// record extends the task record with its `index` and the bound `item`.
fn each_value(
    records: &mut [TaskRecord],
    items: impl IntoIterator<Item = Value>,
    secret_redactor: &crate::security::SecretRedactor,
) -> Value {
    let count = |states: &[TaskState]| {
        Value::Integer(
            records
                .iter()
                .filter(|record| states.contains(&record.state))
                .count() as i64,
        )
    };
    let succeeded = count(&[TaskState::Succeeded]);
    let failed = count(&[TaskState::NonSuccess, TaskState::Failed]);
    let cancelled = count(&[TaskState::Cancelled]);
    let not_started = count(&[TaskState::NotStarted]);
    let elements = records
        .iter_mut()
        .zip(items)
        .enumerate()
        .map(|(index, (record, item))| {
            // Move the captured stdout allocation into its UTF-8 record value
            // when possible. This avoids holding both a full raw capture and
            // its copied text representation while the fan-out result is
            // materialized.
            let mut value = each_task_value(record, secret_redactor);
            if let Value::Record(fields) = &mut value {
                fields.insert("index".into(), Value::Integer(index as i64));
                fields.insert("item".into(), item);
            }
            value
        })
        .collect();
    Value::Record(BTreeMap::from([
        ("mode".into(), Value::String("for_each".into())),
        ("schema_version".into(), Value::Integer(3)),
        ("items".into(), Value::List(elements)),
        ("succeeded".into(), succeeded),
        ("failed".into(), failed),
        ("cancelled".into(), cancelled),
        ("not_started".into(), not_started),
    ]))
}

fn launched_processes(records: &[TaskRecord]) -> usize {
    records.iter().map(|record| record.launched_processes).sum()
}

fn task_error_uncertain(error: &anyhow::Error, manifest_uncertain: bool) -> bool {
    error
        .downcast_ref::<super::AppletInvocationError>()
        .map(super::AppletInvocationError::uncertain)
        .unwrap_or(manifest_uncertain)
}

fn completion_was_cancelled(token: &CancellationToken, inherited: &[Arc<AtomicBool>]) -> bool {
    token.is_cancelled() || any_cancelled(inherited)
}

fn update_record(
    completion: Completion,
    records: &mut [TaskRecord],
    tasks: &[PreparedTask],
    total_output_bytes: &mut usize,
    secret_redactor: &crate::security::SecretRedactor,
) {
    let Completion {
        index,
        result,
        cancelled,
        launched_processes,
    } = completion;
    let Some(record) = records.get_mut(index) else {
        return;
    };
    match result {
        Ok(result) => {
            if register_task_result(secret_redactor, record.sensitive, &result).is_err() {
                record.state = TaskState::Failed;
                record.error = Some(
                    "sensitive task output was withheld because redaction could not be secured"
                        .into(),
                );
                record.uncertain = true;
                record.launched_processes = launched_processes;
                return;
            }
            *total_output_bytes = total_output_bytes
                .checked_add(result.output.len())
                .and_then(|total| total.checked_add(result.stderr.len()))
                .unwrap_or(usize::MAX);
            record.state = if result.outcome.kind == OutcomeKind::Success {
                TaskState::Succeeded
            } else {
                TaskState::NonSuccess
            };
            record.uncertain = result.uncertain;
            record.launched_processes = launched_processes;
            record.invocation = Some(Box::new(result));
        }
        Err(error) => {
            record.state = if cancelled {
                TaskState::Cancelled
            } else {
                TaskState::Failed
            };
            record.error = Some(
                register_task_error(secret_redactor, record.sensitive, &error).unwrap_or_else(
                    |_| {
                        "sensitive task failure was withheld because redaction could not be secured"
                            .into()
                    },
                ),
            );
            record.uncertain = task_error_uncertain(&error, tasks[index].uncertain_on_error);
            record.launched_processes = launched_processes;
        }
    }
}

fn register_secret_value(redactor: &crate::security::SecretRedactor, value: &Value) -> Result<()> {
    match value {
        Value::String(value) => {
            redactor
                .register(value.as_bytes())
                .map_err(|error| anyhow!("registering sensitive value: {error}"))?;
        }
        Value::Integer(value) => {
            redactor
                .register(value.to_string().as_bytes())
                .map_err(|error| anyhow!("registering sensitive value: {error}"))?;
        }
        Value::Number(value) => {
            redactor
                .register(value.to_string().as_bytes())
                .map_err(|error| anyhow!("registering sensitive value: {error}"))?;
        }
        Value::List(values) => {
            for value in values {
                register_secret_value(redactor, value)?;
            }
        }
        Value::Record(values) => {
            for (key, value) in values {
                redactor
                    .register(key.as_bytes())
                    .map_err(|error| anyhow!("registering sensitive value key: {error}"))?;
                register_secret_value(redactor, value)?;
            }
        }
        Value::Bool(_) | Value::Null | Value::Reference(_) => {}
    }
    Ok(())
}

fn register_task_result(
    redactor: &crate::security::SecretRedactor,
    sensitive: bool,
    result: &AppletInvocationResult,
) -> Result<()> {
    if !sensitive {
        return Ok(());
    }
    redactor
        .register(&result.output)
        .map_err(|error| anyhow!("registering sensitive task output: {error}"))?;
    redactor
        .register(&result.stderr)
        .map_err(|error| anyhow!("registering sensitive task stderr: {error}"))?;
    if let Some(value) = &result.value {
        register_secret_value(redactor, value)?;
    }
    Ok(())
}

fn register_task_error(
    redactor: &crate::security::SecretRedactor,
    sensitive: bool,
    error: &anyhow::Error,
) -> Result<String> {
    let text = format!("{error:#}");
    if !sensitive {
        return Ok(text);
    }
    let result = super::execution_result::result_for_error(error);
    for output in [result.stdout.as_deref(), result.stderr.as_deref()]
        .into_iter()
        .flatten()
        .chain(result.message.as_deref().map(str::as_bytes))
        .chain(
            result
                .stages
                .iter()
                .filter_map(|stage| stage.stderr.as_deref()),
        )
    {
        redactor
            .register(output)
            .map_err(|error| anyhow!("registering sensitive task failure bytes: {error}"))?;
    }
    if let Some(value) = &result.value {
        register_secret_value(redactor, value)?;
    }
    redactor
        .register(text.as_bytes())
        .map_err(|error| anyhow!("registering sensitive task failure: {error}"))?;
    Ok(text)
}

fn group_value(
    mode: TaskGroupMode,
    records: &[TaskRecord],
    winner: Option<usize>,
    _secret_redactor: &crate::security::SecretRedactor,
) -> Value {
    let mut group = std::collections::BTreeMap::from([
        (
            "mode".into(),
            Value::String(match mode {
                TaskGroupMode::Join => "join".into(),
                TaskGroupMode::Race => "race".into(),
            }),
        ),
        (
            "tasks".into(),
            Value::List(records.iter().map(task_value_raw).collect()),
        ),
    ]);
    if let Some(index) = winner {
        group.insert("winner".into(), Value::String(records[index].id.clone()));
    }
    Value::Record(group)
}

fn task_value(
    record: &TaskRecord,
    secret_redactor: &crate::security::SecretRedactor,
    compact: bool,
) -> Value {
    let mut fields = std::collections::BTreeMap::from([
        ("id".into(), Value::String(record.id.clone())),
        ("state".into(), Value::String(record.state.as_str().into())),
        ("uncertain".into(), Value::Bool(record.uncertain)),
        (
            "launched_processes".into(),
            Value::Integer(i64::try_from(record.launched_processes).unwrap_or(i64::MAX)),
        ),
    ]);
    if let Some(error) = &record.error {
        fields.insert(
            "error".into(),
            Value::String(secret_redactor.redact_text(error)),
        );
    }
    if let Some(result) = &record.invocation {
        fields.insert("status".into(), Value::Integer(i64::from(result.status)));
        fields.insert("outcome".into(), Value::String(result.outcome.name.clone()));
        fields.insert(
            "outcome_kind".into(),
            Value::String(result.outcome.kind.as_str().into()),
        );
        let encode = |bytes: &[u8]| {
            if compact {
                match std::str::from_utf8(bytes) {
                    Ok(text) => ("utf8", Value::String(text.to_owned())),
                    Err(_) => ("base64", Value::String(BASE64.encode(bytes))),
                }
            } else {
                ("u8_array", byte_array(bytes))
            }
        };
        let redacted_output = secret_redactor.redact_bytes(&result.output);
        let (output_encoding, output) = encode(&redacted_output);
        fields.insert(
            "output_encoding".into(),
            Value::String(output_encoding.into()),
        );
        fields.insert("output".into(), output);
        let redacted_stderr = secret_redactor.redact_bytes(&result.stderr);
        let (stderr_encoding, stderr) = encode(&redacted_stderr);
        fields.insert(
            "stderr_encoding".into(),
            Value::String(stderr_encoding.into()),
        );
        // Keep the raw bytes on AppletInvocationResult for callers that need
        // exact diagnostics; only the task-group presentation is redacted.
        fields.insert("stderr".into(), stderr);
        if let Some(value) = &result.value {
            fields.insert("value".into(), redact_value(secret_redactor, value));
        }
    }
    Value::Record(fields)
}

/// Preserve true values in the language binding. Observer and error paths use
/// `task_value`, which redacts serialized presentations; the enclosing group
/// binding is itself withheld when any member is sensitive.
fn task_value_raw(record: &TaskRecord) -> Value {
    let mut fields = std::collections::BTreeMap::from([
        ("id".into(), Value::String(record.id.clone())),
        ("state".into(), Value::String(record.state.as_str().into())),
        ("uncertain".into(), Value::Bool(record.uncertain)),
        (
            "launched_processes".into(),
            Value::Integer(i64::try_from(record.launched_processes).unwrap_or(i64::MAX)),
        ),
    ]);
    if let Some(error) = &record.error {
        fields.insert("error".into(), Value::String(error.clone()));
    }
    if let Some(result) = &record.invocation {
        fields.insert("status".into(), Value::Integer(i64::from(result.status)));
        fields.insert("outcome".into(), Value::String(result.outcome.name.clone()));
        fields.insert(
            "outcome_kind".into(),
            Value::String(result.outcome.kind.as_str().into()),
        );
        fields.insert("output_encoding".into(), Value::String("u8_array".into()));
        fields.insert("output".into(), byte_array(&result.output));
        fields.insert("stderr_encoding".into(), Value::String("u8_array".into()));
        fields.insert("stderr".into(), byte_array(&result.stderr));
        if let Some(value) = &result.value {
            fields.insert("value".into(), value.clone());
        }
    }
    Value::Record(fields)
}

fn each_task_value(
    record: &mut TaskRecord,
    secret_redactor: &crate::security::SecretRedactor,
) -> Value {
    let mut fields = BTreeMap::from([
        ("state".into(), Value::String(record.state.as_str().into())),
        ("uncertain".into(), Value::Bool(record.uncertain)),
        (
            "launched_processes".into(),
            Value::Integer(i64::try_from(record.launched_processes).unwrap_or(i64::MAX)),
        ),
    ]);
    if let Some(error) = &record.error {
        fields.insert(
            "error".into(),
            Value::String(secret_redactor.redact_text(error)),
        );
    }
    if let Some(result) = record.invocation.take() {
        let mut result = *result;
        fields.insert("status".into(), Value::Integer(i64::from(result.status)));
        fields.insert("outcome".into(), Value::String(result.outcome.name));
        fields.insert(
            "outcome_kind".into(),
            Value::String(result.outcome.kind.as_str().into()),
        );
        let (output_encoding, output) = match String::from_utf8(result.output) {
            Ok(text) => ("utf8", Value::String(text)),
            Err(error) => ("base64", Value::String(BASE64.encode(error.as_bytes()))),
        };
        fields.insert(
            "output_encoding".into(),
            Value::String(output_encoding.into()),
        );
        fields.insert("output".into(), output);
        let stderr = secret_redactor.redact_bytes(&result.stderr);
        let (stderr_encoding, stderr) = match String::from_utf8(stderr) {
            Ok(text) => ("utf8", Value::String(text)),
            Err(error) => ("base64", Value::String(BASE64.encode(error.as_bytes()))),
        };
        fields.insert(
            "stderr_encoding".into(),
            Value::String(stderr_encoding.into()),
        );
        fields.insert("stderr".into(), stderr);
        if let Some(value) = result.value.take() {
            fields.insert("value".into(), value);
        }
    }
    Value::Record(fields)
}

fn redact_value(redactor: &crate::security::SecretRedactor, value: &Value) -> Value {
    match value {
        Value::String(value) => Value::String(redactor.redact_text(value)),
        Value::Integer(value) => {
            let text = value.to_string();
            if redactor.redact_text(&text) == text {
                Value::Integer(*value)
            } else {
                withheld_value()
            }
        }
        Value::Number(value) => {
            let text = value.to_string();
            if redactor.redact_text(&text) == text {
                Value::Number(value.clone())
            } else {
                withheld_value()
            }
        }
        Value::List(values) => Value::List(
            values
                .iter()
                .map(|value| redact_value(redactor, value))
                .collect(),
        ),
        Value::Record(values) => Value::Record(
            values
                .iter()
                .map(|(key, value)| (redactor.redact_text(key), redact_value(redactor, value)))
                .collect(),
        ),
        Value::Bool(_) | Value::Null | Value::Reference(_) => value.clone(),
    }
}

fn withheld_value() -> Value {
    Value::Record(BTreeMap::from([(
        "withheld".into(),
        Value::String("secret".into()),
    )]))
}

fn byte_array(bytes: &[u8]) -> Value {
    Value::List(
        bytes
            .iter()
            .map(|byte| Value::Integer(i64::from(*byte)))
            .collect(),
    )
}

fn ensure_encoded_limit(value: &Value, limit: usize) -> Result<()> {
    // Count as the serializer writes so checking a large record never creates
    // a second full-sized JSON buffer just to discard it.
    struct Counter {
        remaining: usize,
        exceeded: bool,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                self.exceeded = true;
                return Err(std::io::Error::other("task group result limit exceeded"));
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        remaining: limit,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut counter, value);
    if counter.exceeded {
        bail!("task group result exceeds maximum of {limit} bytes")
    }
    result?;
    Ok(())
}

pub(super) fn each_result_encoded_limit(raw_limit: usize, item_count: usize) -> usize {
    raw_limit
        .saturating_mul(16)
        .saturating_add(item_count.saturating_mul(512))
        .saturating_add(1024)
}

struct TaskGroupFailureRequest<'a> {
    mode: TaskGroupMode,
    failed_task: &'a str,
    message: String,
    outcome: Option<Outcome>,
    uncertain: bool,
    records: &'a [TaskRecord],
    secret_redactor: &'a crate::security::SecretRedactor,
    compact: bool,
}

fn task_group_failure(request: TaskGroupFailureRequest<'_>) -> TaskGroupFailure {
    let TaskGroupFailureRequest {
        mode,
        failed_task,
        message,
        outcome,
        uncertain,
        records,
        secret_redactor,
        compact,
    } = request;
    TaskGroupFailure {
        mode,
        failed_task: failed_task.to_owned(),
        message: secret_redactor.redact_text(&message),
        outcome,
        // A fail-fast group may have canceled siblings after they started
        // externally visible work. The aggregate failure must preserve that
        // uncertainty even when the first observed task had a known outcome.
        uncertain: uncertain || records.iter().any(|record| record.uncertain),
        partial_results: Value::List(
            records
                .iter()
                .map(|record| task_value(record, secret_redactor, compact))
                .collect(),
        ),
        partial_results_schema_version: compact.then_some(3),
        launched_processes: launched_processes(records),
    }
}

fn valid_task_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'_' | b'A'..=b'Z' | b'a'..=b'z'))
        && bytes.all(|byte| matches!(byte, b'_' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'))
        && !matches!(name, "true" | "false")
}

fn task_panic(payload: Box<dyn std::any::Any + Send>) -> anyhow::Error {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    anyhow!("task worker panicked: {message}")
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn parent_cancellation_marks_task_completion_cancelled() {
        let scope = TaskScope::new(1, None).unwrap();
        let token = scope.cancellation_token();
        let parent_cancelled = Arc::new(AtomicBool::new(true));
        let inherited = [Arc::clone(&parent_cancelled)];
        let cancelled = completion_was_cancelled(&token, &inherited);
        let tasks = [PreparedTask {
            id: "child".into(),
            applet: "fixture.wait".into(),
            parameters: BTreeMap::new(),
            uncertain_on_error: false,
            native_command: false,
            sensitive: false,
        }];
        let mut records = vec![TaskRecord::pending(&tasks[0])];
        let mut total_output_bytes = 0;

        update_record(
            Completion {
                index: 0,
                result: Err(anyhow!("in-process applet observed parent cancellation")),
                cancelled,
                launched_processes: 0,
            },
            &mut records,
            &tasks,
            &mut total_output_bytes,
            &crate::security::SecretRedactor::default(),
        );

        assert_eq!(records[0].state, TaskState::Cancelled);
    }
}
