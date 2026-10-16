//! Bounded applet task groups used by finite `.program` workflows.

use super::{AppletInvocationContext, AppletInvocationResult, RunOptions};
use crate::ast::{Invocation, TaskGroupMode, Value};
use crate::contracts::{Outcome, OutcomeKind};
use crate::manifest::{AppletBackend, Registry};
use crate::modules::ProgramModuleSession;
use crate::tasks::{TaskHandle, TaskScope};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_TASKS: usize = 256;
const MAX_PARALLEL: usize = 64;

#[derive(Clone)]
struct PreparedTask {
    id: String,
    applet: String,
    parameters: std::collections::BTreeMap<String, Value>,
    uncertain_on_error: bool,
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
    invocation: Option<AppletInvocationResult>,
    launched_processes: usize,
    error: Option<String>,
    uncertain: bool,
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
        }
    }
}

/// Failure raised when a joined task group contains an execution error or a
/// non-success applet outcome. `partial_results` preserves every known result
/// plus cancellation and uncertainty state for the rest of the group.
#[derive(Debug)]
pub struct TaskGroupFailure {
    pub mode: TaskGroupMode,
    pub failed_task: String,
    pub message: String,
    pub outcome: Option<Outcome>,
    pub uncertain: bool,
    pub partial_results: Value,
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
        let resolved = Invocation {
            operation: task.operation.clone(),
            direct: false,
            discard_stderr: false,
            parameters,
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
        };
        let manifest = registry
            .resolve_call(&resolved)
            .with_context(|| format!("task {id:?}"))?;
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
    config: TaskGroupConfig,
    tasks: &[Invocation],
    registry: &Registry,
    bindings: &std::collections::BTreeMap<String, Value>,
    options: &RunOptions,
    cancelled: Arc<AtomicBool>,
    context: AppletInvocationContext,
) -> Result<TaskGroupResult> {
    let TaskGroupConfig {
        mode,
        limit,
        outer_deadline,
    } = config;
    if cancelled.load(Ordering::Acquire) {
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
        Ok(PreparedTask {
            id,
            applet: manifest.name.clone(),
            parameters: call.parameters,
            uncertain_on_error: manifest.uncertain_on_failure,
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
        })
        .collect::<Vec<_>>();
    ensure_encoded_limit(
        &group_value(mode, &not_started, None, &secret_redactor),
        max_output_bytes,
    )
    .context("task group result metadata exceeds the configured output limit")?;
    let deadline = outer_deadline;
    let group_cancelled = Arc::new(AtomicBool::new(false));
    let _bridge = CancellationBridge::new(Arc::clone(&cancelled), Arc::clone(&group_cancelled))?;
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
            if group_cancelled.load(Ordering::Acquire) {
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
                let result = catch_unwind(AssertUnwindSafe(|| {
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
                let cancelled = token.is_cancelled();
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
        let completion = receiver
            .recv()
            .context("task group worker ended without reporting a result")?;
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
                records[index].invocation = Some(result);
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
                records[index].error = Some(format!("{error:#}"));
                let uncertain = task_error_uncertain(&error, task.uncertain_on_error);
                records[index].uncertain = uncertain;
                if mode == TaskGroupMode::Join {
                    first_failure = Some((
                        index,
                        format!("applet invocation failed: {error:#}"),
                        None,
                        uncertain,
                    ));
                    break;
                }
            }
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
                completion.index,
                completion.result,
                completion.cancelled,
                completion.launched_processes,
                &mut records,
                &prepared,
                &mut total_output_bytes,
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
                task_group_failure(
                    mode,
                    &prepared[index].id,
                    format!("{error:#}"),
                    records[index]
                        .invocation
                        .as_ref()
                        .map(|result| result.outcome.clone()),
                    records[index].uncertain,
                    &records,
                    &secret_redactor,
                )
            })?;
            return Ok(TaskGroupResult {
                value,
                launched_processes: launched_processes(&records),
            });
        }
        if let Some((index, message, outcome, uncertain)) = first_failure {
            return Err(task_group_failure(
                mode,
                &prepared[index].id,
                message,
                outcome,
                uncertain,
                &records,
                &secret_redactor,
            )
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
            return Err(task_group_failure(
                mode,
                &prepared[failed_index].id,
                message,
                outcome,
                records[failed_index].uncertain,
                &records,
                &secret_redactor,
            )
            .into());
        }
    }

    if let Some(record) = records
        .iter()
        .find(|record| record.state != TaskState::Succeeded)
    {
        return Err(task_group_failure(
            mode,
            &record.id,
            "task did not complete successfully".into(),
            record
                .invocation
                .as_ref()
                .map(|result| result.outcome.clone()),
            record.uncertain,
            &records,
            &secret_redactor,
        )
        .into());
    }
    let value = group_value(mode, &records, None, &secret_redactor);
    ensure_encoded_limit(&value, max_output_bytes)?;
    Ok(TaskGroupResult {
        value,
        launched_processes: launched_processes(&records),
    })
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

fn update_record(
    index: usize,
    result: Result<AppletInvocationResult>,
    cancelled: bool,
    launched_processes: usize,
    records: &mut [TaskRecord],
    tasks: &[PreparedTask],
    total_output_bytes: &mut usize,
) {
    let Some(record) = records.get_mut(index) else {
        return;
    };
    match result {
        Ok(result) => {
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
            record.invocation = Some(result);
        }
        Err(error) => {
            record.state = if cancelled {
                TaskState::Cancelled
            } else {
                TaskState::Failed
            };
            record.error = Some(format!("{error:#}"));
            record.uncertain = task_error_uncertain(&error, tasks[index].uncertain_on_error);
            record.launched_processes = launched_processes;
        }
    }
}

fn group_value(
    mode: TaskGroupMode,
    records: &[TaskRecord],
    winner: Option<usize>,
    secret_redactor: &crate::security::SecretRedactor,
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
            Value::List(
                records
                    .iter()
                    .map(|record| task_value(record, secret_redactor))
                    .collect(),
            ),
        ),
    ]);
    if let Some(index) = winner {
        group.insert("winner".into(), Value::String(records[index].id.clone()));
    }
    Value::Record(group)
}

fn task_value(record: &TaskRecord, secret_redactor: &crate::security::SecretRedactor) -> Value {
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
        fields.insert("output_encoding".into(), Value::String("u8_array".into()));
        fields.insert("output".into(), byte_array(&result.output));
        fields.insert("stderr_encoding".into(), Value::String("u8_array".into()));
        // Keep the raw bytes on AppletInvocationResult for callers that need
        // exact diagnostics; only the task-group presentation is redacted.
        fields.insert(
            "stderr".into(),
            byte_array(&secret_redactor.redact_bytes(&result.stderr)),
        );
        if let Some(value) = &result.value {
            fields.insert("value".into(), value.clone());
        }
    }
    Value::Record(fields)
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
    let encoded = serde_json::to_vec(value)?;
    if encoded.len() > limit {
        bail!("task group result exceeds maximum of {limit} bytes")
    }
    Ok(())
}

fn task_group_failure(
    mode: TaskGroupMode,
    failed_task: &str,
    message: String,
    outcome: Option<Outcome>,
    uncertain: bool,
    records: &[TaskRecord],
    secret_redactor: &crate::security::SecretRedactor,
) -> TaskGroupFailure {
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
                .map(|record| task_value(record, secret_redactor))
                .collect(),
        ),
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

struct CancellationBridge {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl CancellationBridge {
    fn new(parent: Arc<AtomicBool>, child: Arc<AtomicBool>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("flow-task-cancel-bridge".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    if parent.load(Ordering::Acquire) {
                        child.store(true, Ordering::Release);
                        break;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            })
            .context("starting task cancellation bridge")?;
        Ok(Self {
            stop,
            join: Some(join),
        })
    }
}

impl Drop for CancellationBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
