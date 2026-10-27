//! Scope-owned native pipeline jobs for finite workflow executions.
//!
//! A resolved job remains in this table until its last possible source-level use
//! or its lexical owner exits. Workers use the ordinary native pipeline supervisor;
//! this module owns their lifetime and the explicit source-level handle operations.

use super::*;
use crate::ast::{JobStage, Value};
use crate::contracts::AppletBackend;
use crate::tasks::{CancellationWake, TaskHandle, TaskScope};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

pub(super) const MAX_ACTIVE_JOBS: usize = 16;
pub(super) const MAX_JOB_HANDLES: usize = 32;
const MAX_JOB_STAGES: usize = 16;

/// The workflow state a background job is launched against.
#[derive(Clone)]
pub(super) struct JobEnv<'a> {
    pub(super) registry: &'a Registry,
    pub(super) bindings: &'a BTreeMap<String, Value>,
    pub(super) cwd: &'a crate::working_dir::WorkingDirectory,
    pub(super) options: &'a RunOptions,
    pub(super) signals: &'a Signals,
    pub(super) environment: Option<crate::capabilities::CommandEnvOverlay>,
}

#[derive(Default)]
struct BudgetCounts {
    active_jobs: usize,
    retained_handles: usize,
}

/// Shared by every lexical finite scope in one runtime run. Handles remain in
/// their owning Supervisor; these counters keep nested module/task calls from
/// multiplying the run-wide resource bounds.
pub(super) struct RunBudget {
    counts: Mutex<BudgetCounts>,
    retained_stdout: Arc<AtomicUsize>,
    output_limit: usize,
}

impl RunBudget {
    pub(super) fn new(output_limit: usize) -> Self {
        Self {
            counts: Mutex::new(BudgetCounts::default()),
            retained_stdout: Arc::new(AtomicUsize::new(0)),
            output_limit,
        }
    }

    fn reserve(self: &Arc<Self>) -> Result<(ActiveJobPermit, HandlePermit)> {
        let mut counts = self
            .counts
            .lock()
            .map_err(|_| anyhow!("background job run budget is poisoned"))?;
        if counts.active_jobs >= MAX_ACTIVE_JOBS {
            return Err(not_started_failure(
                format!("background job concurrency limit ({MAX_ACTIVE_JOBS}) reached"),
                execution_result::ExecutionKind::CapacityError,
            ));
        }
        if counts.retained_handles >= MAX_JOB_HANDLES {
            return Err(not_started_failure(
                format!(
                    "finite run retained background job handle limit ({MAX_JOB_HANDLES}) reached"
                ),
                execution_result::ExecutionKind::CapacityError,
            ));
        }
        counts.active_jobs += 1;
        counts.retained_handles += 1;
        Ok((
            ActiveJobPermit(Arc::clone(self)),
            HandlePermit(Arc::clone(self)),
        ))
    }

    fn retained_stdout(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.retained_stdout)
    }

    pub(super) fn output_limit(&self) -> usize {
        self.output_limit
    }
}

struct ActiveJobPermit(Arc<RunBudget>);

impl Drop for ActiveJobPermit {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.0.counts.lock() {
            counts.active_jobs = counts.active_jobs.saturating_sub(1);
        }
    }
}

// A permit belongs to the retained record, including after the worker exits.
struct HandlePermit(Arc<RunBudget>);

impl Drop for HandlePermit {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.0.counts.lock() {
            counts.retained_handles = counts.retained_handles.saturating_sub(1);
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct StageLaunch {
    pub line: usize,
    pub applet: String,
    pub pid: u32,
}

#[derive(Debug)]
pub(super) struct LaunchFailure {
    pub message: String,
    pub result: execution_result::ExecutionResult,
    pub launched_processes: usize,
}

pub(super) enum LaunchAck {
    Started(Vec<StageLaunch>),
    Failed(LaunchFailure),
}

#[derive(Clone, Copy)]
struct TerminalSummary {
    kind: execution_result::ExecutionKind,
    status: Option<i32>,
    signal: Option<i32>,
    effect_certainty: execution_result::EffectCertainty,
    stdout_bytes: usize,
}

impl TerminalSummary {
    fn from_result(result: &execution_result::ExecutionResult) -> Self {
        Self {
            kind: result.kind,
            status: result.status,
            signal: result.signal,
            effect_certainty: result.effect_certainty,
            stdout_bytes: result.stdout.as_ref().map_or(0, Vec::len),
        }
    }
}

#[derive(Clone)]
pub(super) struct LaunchAcknowledger {
    sender: Sender<LaunchAck>,
    snapshot: Arc<Mutex<JobSnapshot>>,
}

impl LaunchAcknowledger {
    pub(super) fn new(sender: Sender<LaunchAck>, snapshot: Arc<Mutex<JobSnapshot>>) -> Self {
        Self { sender, snapshot }
    }

    pub(super) fn record_stage(&self, stage: StageLaunch) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.stages.push(stage);
        }
    }

    pub(super) fn send_started(self, stages: Vec<StageLaunch>) -> Result<()> {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.stages = stages.clone();
            snapshot.state = JobState::Running;
        }
        self.sender
            .send(LaunchAck::Started(stages))
            .map_err(|_| anyhow!("job start acknowledgement receiver was dropped"))
    }

    pub(super) fn send_failed(self, failure: LaunchFailure) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.record_terminal(&failure.result);
        }
        let _ = self.sender.send(LaunchAck::Failed(failure));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Starting,
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

pub(super) struct JobSnapshot {
    state: JobState,
    stages: Vec<StageLaunch>,
    stage_results: Arc<Mutex<Vec<Option<execution_result::StageResult>>>>,
    terminal: Option<TerminalSummary>,
    diagnostic_truncated: Vec<bool>,
    stdout_truncated: bool,
}

impl JobSnapshot {
    fn new(
        stage_count: usize,
        stage_results: Arc<Mutex<Vec<Option<execution_result::StageResult>>>>,
    ) -> Self {
        Self {
            state: JobState::Starting,
            stages: Vec::with_capacity(stage_count),
            stage_results,
            terminal: None,
            diagnostic_truncated: vec![false; stage_count],
            stdout_truncated: false,
        }
    }

    fn record_terminal(&mut self, result: &execution_result::ExecutionResult) {
        self.state = state_for_result(result);
        self.terminal = Some(TerminalSummary::from_result(result));
        self.stdout_truncated = result.kind == execution_result::ExecutionKind::OutputLimit;
        if let Ok(mut stages) = self.stage_results.lock() {
            for (target, stage) in stages.iter_mut().zip(&result.stages) {
                *target = Some(stage.clone());
            }
        }
    }
}

struct Job {
    snapshot: Arc<Mutex<JobSnapshot>>,
    handle: Option<TaskHandle<execution_result::ExecutionResult>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    cancel_wake: Arc<CancellationWake>,
    terminal: Option<execution_result::ExecutionResult>,
    resolved: bool,
    _permit: HandlePermit,
}

struct JobBatch {
    scopes: Vec<usize>,
    resolved: bool,
}

impl Default for JobBatch {
    fn default() -> Self {
        Self {
            scopes: Vec::new(),
            resolved: true,
        }
    }
}

/// The caller gets this error only when `spawn` could not reach its all-stages
/// start acknowledgement. The contained result distinguishes no process from
/// a partially launched pipeline whose started stages were reaped.
#[derive(Debug)]
pub(super) struct JobSpawnFailure {
    pub message: String,
    pub result: execution_result::ExecutionResult,
    pub launched_processes: usize,
}

impl fmt::Display for JobSpawnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for JobSpawnFailure {}

pub(super) struct Supervisor {
    tasks: TaskScope,
    jobs: BTreeMap<(usize, String), Job>,
    scope_parents: BTreeMap<usize, usize>,
    batches: BTreeMap<(usize, String), JobBatch>,
    next_scope: usize,
    budget: Arc<RunBudget>,
    output_budget: Arc<AtomicUsize>,
    output_limit: usize,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.cancel_scope(0);
    }
}

impl Supervisor {
    pub(super) fn new(signals: &Signals, _output_limit: usize) -> Result<Self> {
        Self::new_with_task_deadline(signals, signals.deadline)
    }

    /// A live Supervisor spans multiple independently timed nodes. Its task
    /// scope must not retain the first node's deadline; live waits apply the
    /// active node's deadline at the call site instead.
    pub(super) fn new_for_live(signals: &Signals, _output_limit: usize) -> Result<Self> {
        Self::new_with_task_deadline(signals, None)
    }

    fn new_with_task_deadline(signals: &Signals, task_deadline: Option<Instant>) -> Result<Self> {
        let budget = signals
            .current_job_run_budget()
            .context("background job runtime is missing its finite-run budget")?;
        let output_limit = budget.output_limit();
        Ok(Self {
            tasks: TaskScope::new(MAX_ACTIVE_JOBS, task_deadline)?,
            jobs: BTreeMap::new(),
            scope_parents: BTreeMap::new(),
            batches: BTreeMap::new(),
            next_scope: 1,
            output_budget: budget.retained_stdout(),
            budget,
            output_limit,
        })
    }

    pub(super) fn enter_scope(&mut self, parent: usize) -> Result<usize> {
        let scope = self.next_scope;
        self.next_scope = scope
            .checked_add(1)
            .ok_or_else(|| anyhow!("background job scope identifiers exhausted"))?;
        self.scope_parents.insert(scope, parent);
        Ok(scope)
    }

    pub(super) fn run_budget(&self) -> Arc<RunBudget> {
        Arc::clone(&self.budget)
    }

    pub(super) fn collect_loop_iteration(&mut self, parent: usize, iteration: usize) -> Result<()> {
        let direct = self
            .jobs
            .iter()
            .filter(|((scope, _), job)| *scope == iteration && !job.resolved)
            .map(|((_, name), _)| name.clone())
            .collect::<Vec<_>>();
        for name in direct {
            let batch = self.batches.entry((parent, name)).or_default();
            batch.scopes.push(iteration);
            batch.resolved = false;
        }

        let nested = self
            .batches
            .keys()
            .filter(|(scope, _)| *scope == iteration)
            .cloned()
            .collect::<Vec<_>>();
        for key in nested {
            let Some(batch) = self.batches.remove(&key) else {
                continue;
            };
            for scope in &batch.scopes {
                if self.scope_parents.get(scope) == Some(&iteration) {
                    self.scope_parents.insert(*scope, parent);
                }
            }
            let destination = self.batches.entry((parent, key.1.clone())).or_default();
            destination.scopes.extend(batch.scopes);
            destination.resolved &= batch.resolved;
        }

        // A collected iteration scope remains alive until every source-level
        // use of its collected handles has been retired.
        if !self.scope_is_referenced(iteration) {
            self.finish_scope(iteration)?;
        }
        Ok(())
    }

    pub(super) fn batch_scopes(&self, parent: usize, name: &str) -> Option<Vec<usize>> {
        self.batches
            .get(&(parent, name.to_owned()))
            .map(|batch| batch.scopes.clone())
    }

    fn scope_is_referenced(&self, scope: usize) -> bool {
        self.batches
            .values()
            .any(|batch| batch.scopes.contains(&scope))
            || self.batches.keys().any(|(parent, _)| *parent == scope)
    }

    pub(super) fn inspect_batch(&self, parent: usize, name: &str) -> Result<Value> {
        let scopes = self
            .batch_scopes(parent, name)
            .ok_or_else(|| anyhow!("unknown background job batch {name:?} in this scope"))?;
        scopes
            .iter()
            .map(|scope| self.inspect(*scope, name))
            .collect::<Result<Vec<_>>>()
            .map(Value::List)
    }

    pub(super) fn await_batch_until(
        &mut self,
        parent: usize,
        name: &str,
        deadline: Option<Instant>,
    ) -> Result<Vec<(usize, execution_result::ExecutionResult)>> {
        let scopes = self
            .batch_scopes(parent, name)
            .ok_or_else(|| anyhow!("unknown background job batch {name:?} in this scope"))?;
        let mut results = Vec::with_capacity(scopes.len());
        for scope in scopes {
            results.push((scope, self.await_job_until(scope, name, deadline)?));
        }
        if let Some(batch) = self.batches.get_mut(&(parent, name.to_owned())) {
            batch.resolved = true;
        }
        Ok(results)
    }

    pub(super) fn cancel_batch(&mut self, parent: usize, name: &str) -> Result<()> {
        let scopes = self
            .batch_scopes(parent, name)
            .ok_or_else(|| anyhow!("unknown background job batch {name:?} in this scope"))?;
        let keys = scopes
            .into_iter()
            .map(|scope| (scope, name.to_owned()))
            .collect::<Vec<_>>();
        for key in &keys {
            if !self.jobs.contains_key(key) {
                bail!("unknown background job {name:?} in this scope");
            }
        }
        // Signal every batch member before joining any one worker. A slow
        // shutdown for an earlier iteration must not leave later iterations
        // running throughout that wait.
        for key in &keys {
            if let Some(job) = self.jobs.get(key).filter(|job| !job.resolved) {
                job.cancelled
                    .store(true, std::sync::atomic::Ordering::Release);
                job.cancel_wake.notify();
            }
        }
        for key in &keys {
            if self.jobs.get(key).is_some_and(|job| !job.resolved) {
                self.join_job_result(key)?;
                self.jobs
                    .get_mut(key)
                    .expect("validated batch job exists")
                    .resolved = true;
            }
        }
        if let Some(batch) = self.batches.get_mut(&(parent, name.to_owned())) {
            batch.resolved = true;
        }
        Ok(())
    }

    pub(super) fn retire_batch(&mut self, parent: usize, name: &str) {
        let Some(batch) = self.batches.remove(&(parent, name.to_owned())) else {
            return;
        };
        let scopes = batch.scopes;
        for scope in scopes {
            self.retire_resolved(scope, name);
            if !self.scope_is_referenced(scope) {
                let _ = self.finish_scope(scope);
            }
        }
    }

    /// Validate a setup-owned job without starting it. Services preflight every
    /// setup job before launching the first persistent worker.
    pub(super) fn preflight_spawn(
        &self,
        scope: usize,
        name: &str,
        stages: &[JobStage],
        env: JobEnv<'_>,
    ) -> Result<()> {
        let JobEnv {
            registry,
            bindings,
            cwd,
            ..
        } = env;
        if stages.is_empty() || stages.len() > MAX_JOB_STAGES {
            bail!("background pipeline must contain between 1 and {MAX_JOB_STAGES} stages");
        }
        if self.jobs.contains_key(&(scope, name.to_owned())) {
            bail!("background job {name:?} is already defined in this scope");
        }
        let cwd = cwd.duplicate()?;
        let mut calls = Vec::with_capacity(stages.len());
        for (index, stage) in stages.iter().enumerate() {
            let mut call = stage.call.clone();
            let manifest = registry.resolve_call(&call)?.clone();
            if manifest.builtin.is_some() || manifest.execution_backend() != AppletBackend::Command
            {
                bail!(
                    "line {}: background jobs support native command applets only",
                    stage.line
                );
            }
            if call.cache || call.binding.is_some() || call.result_binding.is_some() {
                bail!(
                    "line {}: background stages cannot cache or bind values",
                    stage.line
                );
            }
            if call.status_binding.is_some() {
                bail!(
                    "line {}: background stages cannot bind native status",
                    stage.line
                );
            }
            if index > 0 && !manifest.accepts_stdin {
                bail!(
                    "line {}: background pipeline receiver {:?} does not accept stdin",
                    stage.line,
                    manifest.name_for(&call)
                );
            }
            if let Some(stdio) = manifest.stdio() {
                if stdio
                    .stderr
                    .is_some_and(|mode| mode != invocation_io::StdioDisposition::Capture)
                    || (index + 1 == stages.len()
                        && stdio
                            .stdout
                            .is_some_and(|mode| mode != invocation_io::StdioDisposition::Capture))
                {
                    bail!(
                        "line {}: background pipelines require captured stdout and stderr",
                        stage.line
                    );
                }
            }
            let parameters = call
                .parameters
                .iter()
                .map(|(parameter, value)| Ok((parameter.clone(), resolve(value, bindings)?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            let argv = manifest.build_arguments(&parameters).with_context(|| {
                format!("line {}: validating resolved job arguments", stage.line)
            })?;
            call.parameters = parameters;
            call.status_binding = Some("__execute_job_stage_status".into());
            let binary = binary_path(manifest.binary_for(&call), &cwd)
                .with_context(|| format!("line {}: resolving job executable", stage.line))?;
            crate::capabilities::command_for(
                &binary,
                argv.iter().map(|argument| OsStr::new(argument.as_str())),
                registry,
                &manifest.name,
                &cwd,
            )
            .with_context(|| format!("line {}: preflighting job command", stage.line))?;
            calls.push((call, manifest, stage.line));
        }
        crate::capabilities::preflight_invocations(
            calls.iter().map(|(call, _, _)| call),
            registry,
            &cwd,
        )
        .with_context(|| format!("preflighting background job {name:?}"))?;
        Ok(())
    }

    pub(super) fn spawn(
        &mut self,
        scope: usize,
        name: &str,
        stages: &[JobStage],
        env: JobEnv<'_>,
    ) -> Result<usize> {
        let JobEnv {
            registry,
            bindings,
            cwd,
            options,
            signals: outer_signals,
            environment,
        } = env;
        if stages.is_empty() || stages.len() > MAX_JOB_STAGES {
            return Err(not_started_failure(
                format!("background pipeline must contain between 1 and {MAX_JOB_STAGES} stages"),
                execution_result::ExecutionKind::UnknownError,
            ));
        }
        let key = (scope, name.to_owned());
        if self.jobs.contains_key(&key) {
            return Err(not_started_failure(
                format!("background job {name:?} is already defined in this scope"),
                execution_result::ExecutionKind::UnknownError,
            ));
        }
        let cwd = cwd
            .duplicate()
            .with_context(|| format!("snapshotting working directory for job {name:?}"))?;
        let mut calls = Vec::with_capacity(stages.len());
        let mut prepared = Vec::with_capacity(stages.len());
        for (index, stage) in stages.iter().enumerate() {
            let mut call = stage.call.clone();
            let manifest = registry.resolve_call(&call)?.clone();
            if manifest.builtin.is_some() || manifest.execution_backend() != AppletBackend::Command
            {
                bail!(
                    "line {}: background jobs support native command applets only",
                    stage.line
                );
            }
            if call.cache || call.binding.is_some() || call.result_binding.is_some() {
                bail!(
                    "line {}: background stages cannot cache or bind values",
                    stage.line
                );
            }
            if call.status_binding.is_some() {
                bail!(
                    "line {}: background stages cannot bind native status",
                    stage.line
                );
            }
            if index > 0 && !manifest.accepts_stdin {
                bail!(
                    "line {}: background pipeline receiver {:?} does not accept stdin",
                    stage.line,
                    manifest.name_for(&call)
                );
            }
            if let Some(stdio) = manifest.stdio() {
                if stdio
                    .stderr
                    .is_some_and(|mode| mode != invocation_io::StdioDisposition::Capture)
                    || (index + 1 == stages.len()
                        && stdio
                            .stdout
                            .is_some_and(|mode| mode != invocation_io::StdioDisposition::Capture))
                {
                    bail!(
                        "line {}: background pipelines require captured stdout and stderr",
                        stage.line
                    );
                }
            }

            let parameters = call
                .parameters
                .iter()
                .map(|(parameter, value)| Ok((parameter.clone(), resolve(value, bindings)?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            let argv = manifest.build_arguments(&parameters).with_context(|| {
                format!("line {}: validating resolved job arguments", stage.line)
            })?;
            call.parameters = parameters;
            // The worker must collect all stage outcomes before the later
            // `await` statement decides whether its terminal status is kept.
            call.status_binding = Some("__execute_job_stage_status".into());
            let binary = binary_path(manifest.binary_for(&call), &cwd)
                .with_context(|| format!("line {}: resolving job executable", stage.line))?;
            crate::capabilities::command_for(
                &binary,
                argv.iter().map(|argument| OsStr::new(argument.as_str())),
                registry,
                &manifest.name,
                &cwd,
            )
            .with_context(|| format!("line {}: preflighting job command", stage.line))?;
            prepared.push((binary, argv));
            calls.push((call, manifest, stage.line));
        }
        crate::capabilities::preflight_invocations(
            calls.iter().map(|(call, _, _)| call),
            registry,
            &cwd,
        )
        .with_context(|| format!("preflighting background job {name:?}"))?;

        // Reserve run-wide active and retained-handle capacity only after the
        // complete pipeline has passed preflight and before its first effect.
        let (admission, handle_permit) = self.budget.reserve()?;

        let stage_results = Arc::new(Mutex::new(vec![None; stages.len()]));
        let snapshot = Arc::new(Mutex::new(JobSnapshot::new(
            stages.len(),
            Arc::clone(&stage_results),
        )));
        let (sender, receiver) = mpsc::channel();
        let ack = LaunchAcknowledger::new(sender, Arc::clone(&snapshot));
        let worker_snapshot = Arc::clone(&snapshot);
        let worker_environment = environment;
        let registry = registry.clone();
        let options = options.clone();
        let outer_cancelled = Arc::clone(&outer_signals.cancelled);
        let job_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_job_cancelled = Arc::clone(&job_cancelled);
        let job_cancel_wake = Arc::new(CancellationWake::new()?);
        let worker_job_cancel_wake = Arc::clone(&job_cancel_wake);
        let invocation_context = AppletInvocationContext::from_signals(outer_signals)?;
        let prepared = Arc::new(prepared);
        let truncation = Arc::new(Mutex::new(vec![false; stages.len()]));
        let worker_truncation = Arc::clone(&truncation);
        let output_budget = Arc::clone(&self.output_budget);
        let output_limit = self.output_limit;
        #[cfg(test)]
        let panic_before_ack = outer_signals.job_panic_before_ack.clone();
        let worker_ack = ack.clone();
        // The receiver must observe disconnection if the task panics before
        // it can publish either a start or a failure acknowledgment.
        drop(ack);
        let worker = self.tasks.spawn(move |token| {
            let worker_signals = Signals::with_cancellation_and_deadline(
                token.cancellation_flag(),
                true,
                token.deadline(),
            );

            let result = match worker_signals {
                Ok(mut signals) => {
                    let _environment = worker_environment
                        .map(|overlay| crate::capabilities::CommandEnvScope::enter(Some(overlay)));
                    signals.outer_cancelled.push(outer_cancelled);
                    signals.job_cancelled = Some(worker_job_cancelled);
                    signals.inherit_applet_context(&invocation_context)?;
                    signals.cancel_wakes.push(token.cancellation_wake());
                    signals.cancel_wakes.push(worker_job_cancel_wake);
                    signals.job_launch_ack = Some(Arc::new(Mutex::new(Some(worker_ack))));
                    signals.job_output_budget = Some(output_budget);
                    signals.job_output_limit = Some(output_limit);
                    signals.prepared_job_commands = Some(prepared);
                    signals.job_diagnostic_truncation = Some(worker_truncation);
                    signals.job_stage_results = Some(stage_results);
                    #[cfg(test)]
                    {
                        signals.job_panic_before_ack = panic_before_ack;
                    }
                    let _admission = admission;
                    let refs = calls
                        .iter()
                        .map(|(call, manifest, line)| (call, manifest, *line))
                        .collect::<Vec<_>>();
                    let mut launched = 0;
                    let _output_scope =
                        super::OutputScopeGuard::enter(super::OutputScope::BackgroundJob);
                    match execute_group(GroupRequest {
                        registry: &registry,
                        calls: &refs,
                        input: Input::Empty,
                        prepared_first_call: None,
                        bindings: &BTreeMap::new(),
                        cwd: &cwd,
                        options: &options,
                        launched: &mut launched,
                        signals: &signals,
                    }) {
                        Ok(group) => {
                            let terminal = refs.last().expect("nonempty job");
                            let status = group.status.unwrap_or_default();
                            let outcome = terminal.1.outcome_for(status);
                            execution_result::ExecutionResult::from_outcome(
                                outcome,
                                Some(group.bytes),
                                Some(group.diagnostic),
                                None,
                                group.stages,
                            )
                        }
                        Err(error) => {
                            let mut result = execution_result::result_for_error(&error);
                            if launched == 0
                                && result.effect_certainty
                                    == execution_result::EffectCertainty::Uncertain
                            {
                                result.effect_certainty =
                                    execution_result::EffectCertainty::NotStarted;
                            }
                            signals.fail_job_launch(LaunchFailure {
                                message: format!("{error:#}"),
                                result: result.clone(),
                                launched_processes: launched,
                            });
                            result
                        }
                    }
                }
                Err(error) => {
                    let result = execution_result::ExecutionResult::runtime_failure(
                        execution_result::ExecutionKind::IoError,
                        format!("{error:#}"),
                        execution_result::EffectCertainty::NotStarted,
                    );
                    if let Ok(mut snapshot) = worker_snapshot.lock() {
                        snapshot.record_terminal(&result);
                    }
                    worker_ack.send_failed(LaunchFailure {
                        message: format!("{error:#}"),
                        result: result.clone(),
                        launched_processes: 0,
                    });
                    return Ok(result);
                }
            };

            if let Ok(mut snapshot) = worker_snapshot.lock() {
                snapshot.record_terminal(&result);
                if let Ok(truncated) = truncation.lock() {
                    snapshot.diagnostic_truncated.clone_from(&truncated);
                }
            }
            Ok(result)
        });
        let handle = match worker {
            Ok(handle) => handle,
            Err(error) => {
                return Err(error).context("starting background job worker");
            }
        };
        self.jobs.insert(
            key.clone(),
            Job {
                snapshot: Arc::clone(&snapshot),
                handle: Some(handle),
                cancelled: job_cancelled,
                cancel_wake: job_cancel_wake,
                terminal: None,
                resolved: false,
                _permit: handle_permit,
            },
        );

        match receiver.recv() {
            Ok(LaunchAck::Started(stages)) => Ok(stages.len()),
            Ok(LaunchAck::Failed(failure)) => {
                let result = self.join_job_result(&key)?;
                let result = if result.stages.is_empty() {
                    failure.result
                } else {
                    result
                };
                Err(anyhow::Error::new(JobSpawnFailure {
                    message: failure.message,
                    result,
                    launched_processes: failure.launched_processes,
                }))
            }
            Err(error) => {
                let launched = snapshot
                    .lock()
                    .map(|snapshot| snapshot.stages.len())
                    .unwrap_or(0);
                let result = self.join_job_result(&key)?;
                Err(anyhow::Error::new(JobSpawnFailure {
                    message: format!(
                        "background job worker exited before launch acknowledgement: {error}"
                    ),
                    result,
                    launched_processes: launched,
                }))
            }
        }
    }

    pub(super) fn inspect(&self, scope: usize, name: &str) -> Result<Value> {
        let job = self.get(scope, name)?;
        let snapshot = job
            .snapshot
            .lock()
            .map_err(|_| anyhow!("background job snapshot lock is poisoned"))?;
        Ok(snapshot_value(name, &snapshot))
    }

    pub(super) fn result_value(
        &self,
        scope: usize,
        name: &str,
        line: usize,
        result: &execution_result::ExecutionResult,
    ) -> Result<Value> {
        let job = self.get(scope, name)?;
        let snapshot = job
            .snapshot
            .lock()
            .map_err(|_| anyhow!("background job snapshot lock is poisoned"))?;
        let mut record = match result.to_source_value(line, format!("job {name}")) {
            Value::Record(record) => record,
            _ => unreachable!("execution source record is a JSON object"),
        };
        record.insert(
            "stdout_truncated".into(),
            Value::Bool(snapshot.stdout_truncated),
        );
        let truncated = snapshot.diagnostic_truncated.clone();
        record.insert(
            "stderr_truncated".into(),
            Value::List(truncated.iter().copied().map(Value::Bool).collect()),
        );
        record.insert(
            "stderr_by_stage".into(),
            Value::List(
                result
                    .stages
                    .iter()
                    .map(|stage| {
                        stage
                            .stderr
                            .as_deref()
                            .map(|bytes| {
                                Value::List(
                                    bytes
                                        .iter()
                                        .map(|byte| Value::Integer(i64::from(*byte)))
                                        .collect(),
                                )
                            })
                            .unwrap_or_else(|| Value::List(Vec::new()))
                    })
                    .collect(),
            ),
        );
        if let Some(Value::List(stages)) = record.get_mut("stages") {
            for (stage, truncated) in stages.iter_mut().zip(truncated) {
                if let Value::Record(fields) = stage {
                    fields.insert("stderr_truncated".into(), Value::Bool(truncated));
                }
            }
        }
        record.insert(
            "stderr_limit_per_stage".into(),
            Value::Integer(MAX_DIAGNOSTIC_BYTES_PER_STAGE as i64),
        );
        Ok(Value::Record(record))
    }

    pub(super) fn await_job_until(
        &mut self,
        scope: usize,
        name: &str,
        deadline: Option<Instant>,
    ) -> Result<execution_result::ExecutionResult> {
        let key = (scope, name.to_owned());
        let result = self.join_job_result_until(&key, deadline)?;
        self.jobs
            .get_mut(&key)
            .context("background job disappeared while awaiting")?
            .resolved = true;
        Ok(result)
    }

    pub(super) fn cancel(&mut self, scope: usize, name: &str) -> Result<()> {
        let key = (scope, name.to_owned());
        let Some(job) = self.jobs.get(&key) else {
            bail!("unknown background job {name:?} in this scope");
        };
        if !job.resolved {
            job.cancelled
                .store(true, std::sync::atomic::Ordering::Release);
            job.cancel_wake.notify();
            self.join_job_result(&key)?;
            self.jobs.get_mut(&key).expect("job exists").resolved = true;
        }
        Ok(())
    }

    /// Discard a resolved record after its final possible source-level use.
    pub(super) fn retire_resolved(&mut self, scope: usize, name: &str) {
        let key = (scope, name.to_owned());
        if self.jobs.get(&key).is_some_and(|job| job.resolved) {
            self.jobs.remove(&key);
        }
    }

    /// Resolve a statically visible handle name through its lexical scopes.
    pub(super) fn scope_for_job(&self, name: &str, scopes: &[usize]) -> Option<usize> {
        scopes
            .iter()
            .copied()
            .find(|scope| self.jobs.contains_key(&(*scope, name.to_owned())))
    }

    /// A normal lexical exit requires explicit observation through await or
    /// cancel, regardless of whether the worker completed on its own.
    pub(super) fn finish_scope(&mut self, scope: usize) -> Result<()> {
        let unresolved = self
            .jobs
            .iter()
            .filter(|((job_scope, _), job)| *job_scope == scope && !job.resolved)
            .map(|((_, name), _)| name.clone())
            .collect::<Vec<_>>();
        let unresolved_batches = self
            .batches
            .iter()
            .filter(|((parent, _), batch)| *parent == scope && !batch.resolved)
            .map(|((_, name), _)| name.clone())
            .collect::<Vec<_>>();
        if !unresolved.is_empty() || !unresolved_batches.is_empty() {
            let unresolved = unresolved
                .into_iter()
                .chain(unresolved_batches)
                .collect::<Vec<_>>();
            bail!(
                "background job handles must be explicitly awaited or cancelled before normal scope exit: {}",
                unresolved.join(", ")
            );
        }
        self.jobs.retain(|(job_scope, _), _| *job_scope != scope);
        self.batches.retain(|(parent, _), _| *parent != scope);
        self.scope_parents.remove(&scope);
        Ok(())
    }

    /// Early return and error unwind cancel and reap every still-owned worker
    /// in the scope before its handles disappear.
    pub(super) fn cancel_scope(&mut self, scope: usize) -> Result<()> {
        let mut scopes = vec![scope];
        let mut cursor = 0;
        while cursor < scopes.len() {
            let parent = scopes[cursor];
            cursor += 1;
            scopes.extend(
                self.scope_parents
                    .iter()
                    .filter_map(|(child, owner)| (*owner == parent).then_some(*child)),
            );
        }
        let scope_set = scopes
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let keys = self
            .jobs
            .iter()
            .filter(|((job_scope, _), job)| scope_set.contains(job_scope) && !job.resolved)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in &keys {
            if let Some(job) = self.jobs.get(key) {
                job.cancelled
                    .store(true, std::sync::atomic::Ordering::Release);
                job.cancel_wake.notify();
            }
        }
        let mut first_error = None;
        for key in keys {
            if let Err(error) = self.join_job_result(&key) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            if let Some(job) = self.jobs.get_mut(&key) {
                job.resolved = true;
            }
        }
        self.jobs
            .retain(|(job_scope, _), _| !scope_set.contains(job_scope));
        self.batches.retain(|(parent, _), batch| {
            !scope_set.contains(parent)
                && !batch.scopes.iter().any(|child| scope_set.contains(child))
        });
        self.scope_parents
            .retain(|child, parent| !scope_set.contains(child) && !scope_set.contains(parent));
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Cancel a try body's owned jobs before entering its catch while keeping
    /// the active try scope available for jobs created by the catch body.
    pub(super) fn cancel_scope_preserving(&mut self, scope: usize) -> Result<()> {
        let parent = self.scope_parents.get(&scope).copied();
        self.cancel_scope(scope)?;
        if let Some(parent) = parent {
            self.scope_parents.insert(scope, parent);
        }
        Ok(())
    }

    fn get(&self, scope: usize, name: &str) -> Result<&Job> {
        self.jobs
            .get(&(scope, name.to_owned()))
            .ok_or_else(|| anyhow!("unknown background job {name:?} in this scope"))
    }

    fn join_job_result(
        &mut self,
        key: &(usize, String),
    ) -> Result<execution_result::ExecutionResult> {
        self.join_job_result_until(key, None)
    }

    fn join_job_result_until(
        &mut self,
        key: &(usize, String),
        deadline: Option<Instant>,
    ) -> Result<execution_result::ExecutionResult> {
        if let Some(result) = self.jobs.get(key).and_then(|job| job.terminal.clone()) {
            if self.jobs.get(key).is_some_and(|job| job.handle.is_none()) {
                return Ok(result);
            }
        }
        let handle = self
            .jobs
            .get_mut(key)
            .and_then(|job| job.handle.take())
            .context("background job worker handle is missing")?;
        let result = match self.tasks.join_until(handle, deadline) {
            Ok(result) => result,
            Err(error) => execution_result::ExecutionResult::runtime_failure(
                execution_result::ExecutionKind::UnknownError,
                format!("background job worker failed: {error:#}"),
                execution_result::EffectCertainty::Uncertain,
            ),
        };
        if let Some(job) = self.jobs.get_mut(key) {
            job.terminal = Some(result.clone());
            if let Ok(mut snapshot) = job.snapshot.lock() {
                snapshot.record_terminal(&result);
            }
        }
        Ok(result)
    }
}

fn not_started_failure(message: String, kind: execution_result::ExecutionKind) -> anyhow::Error {
    let result = execution_result::ExecutionResult::runtime_failure(
        kind,
        message.clone(),
        execution_result::EffectCertainty::NotStarted,
    );
    anyhow::Error::new(JobSpawnFailure {
        message,
        result,
        launched_processes: 0,
    })
}

fn state_for_result(result: &execution_result::ExecutionResult) -> JobState {
    match result.kind {
        execution_result::ExecutionKind::Success => JobState::Completed,
        execution_result::ExecutionKind::Cancelled => JobState::Cancelled,
        _ => JobState::Failed,
    }
}

fn snapshot_value(name: &str, snapshot: &JobSnapshot) -> Value {
    let mut fields = BTreeMap::new();
    fields.insert("name".into(), Value::String(name.to_owned()));
    fields.insert(
        "state".into(),
        Value::String(snapshot.state.as_str().into()),
    );
    let observed_stages = snapshot
        .stage_results
        .lock()
        .map(|stages| stages.clone())
        .unwrap_or_default();
    let result = snapshot.terminal.as_ref();
    let stage_count = snapshot.stages.len().max(observed_stages.len());
    let stages = (0..stage_count)
        .map(|index| {
            let launched = snapshot.stages.get(index);
            let stage_result = observed_stages.get(index).and_then(Option::as_ref);
            let mut stage = BTreeMap::new();
            stage.insert(
                "line".into(),
                Value::Integer(
                    stage_result
                        .map(|stage| stage.line)
                        .or_else(|| launched.map(|stage| stage.line))
                        .unwrap_or_default() as i64,
                ),
            );
            stage.insert(
                "applet".into(),
                Value::String(
                    stage_result
                        .map(|stage| stage.applet.clone())
                        .or_else(|| launched.map(|stage| stage.applet.clone()))
                        .unwrap_or_default(),
                ),
            );
            stage.insert(
                "pid".into(),
                launched
                    .map(|stage| Value::Integer(i64::from(stage.pid)))
                    .unwrap_or(Value::Null),
            );
            stage.insert(
                "state".into(),
                Value::String(stage_result.map_or_else(
                    || {
                        if launched.is_some() {
                            "running"
                        } else {
                            "not_started"
                        }
                        .into()
                    },
                    |stage| stage_state(stage.state).into(),
                )),
            );
            stage.insert(
                "status".into(),
                stage_result
                    .and_then(|stage| stage.status)
                    .map(|status| Value::Integer(i64::from(status)))
                    .unwrap_or(Value::Null),
            );
            stage.insert(
                "signal".into(),
                stage_result
                    .and_then(|stage| stage.signal)
                    .map(|signal| Value::Integer(i64::from(signal)))
                    .unwrap_or(Value::Null),
            );
            stage.insert(
                "effect_certainty".into(),
                Value::String(
                    stage_result
                        .map(|stage| stage.effect_certainty.as_str())
                        .unwrap_or(if launched.is_some() {
                            "uncertain"
                        } else {
                            "not_started"
                        })
                        .into(),
                ),
            );
            stage.insert(
                "stderr_bytes".into(),
                Value::Integer(
                    stage_result
                        .and_then(|stage| stage.stderr.as_ref())
                        .map_or(0, Vec::len) as i64,
                ),
            );
            stage.insert(
                "stderr_truncated".into(),
                Value::Bool(
                    snapshot
                        .diagnostic_truncated
                        .get(index)
                        .copied()
                        .unwrap_or(false),
                ),
            );
            Value::Record(stage)
        })
        .collect();
    fields.insert("stages".into(), Value::List(stages));
    fields.insert(
        "status".into(),
        result
            .and_then(|result| result.status)
            .map(|status| Value::Integer(i64::from(status)))
            .unwrap_or(Value::Null),
    );
    fields.insert(
        "signal".into(),
        result
            .and_then(|result| result.signal)
            .map(|signal| Value::Integer(i64::from(signal)))
            .unwrap_or(Value::Null),
    );
    fields.insert(
        "kind".into(),
        result
            .map(|result| Value::String(result.kind.as_str().into()))
            .unwrap_or(Value::Null),
    );
    fields.insert(
        "effect_certainty".into(),
        result
            .map(|result| Value::String(result.effect_certainty.as_str().into()))
            .unwrap_or_else(|| {
                Value::String(
                    if snapshot.stages.is_empty() {
                        "not_started"
                    } else {
                        "uncertain"
                    }
                    .into(),
                )
            }),
    );
    fields.insert(
        "stdout_bytes".into(),
        Value::Integer(result.map_or(0, |result| result.stdout_bytes) as i64),
    );
    fields.insert(
        "stdout_truncated".into(),
        Value::Bool(snapshot.stdout_truncated),
    );
    fields.insert(
        "stderr_limit_per_stage".into(),
        Value::Integer(MAX_DIAGNOSTIC_BYTES_PER_STAGE as i64),
    );
    Value::Record(fields)
}

fn stage_state(state: execution_result::StageState) -> &'static str {
    use execution_result::StageState;
    match state {
        StageState::Exited => "exited",
        StageState::Signaled => "signaled",
        StageState::SpawnFailed => "spawn_failed",
        StageState::NotStarted => "not_started",
        StageState::Unknown => "unknown",
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ast::StepKind;
    use std::time::{Duration, Instant};

    #[test]
    fn released_job_permits_do_not_accumulate() -> Result<()> {
        let budget = Arc::new(RunBudget::new(1024));
        for _ in 0..10_000 {
            let (active, handle) = budget.reserve()?;
            drop(active);
            drop(handle);
            let counts = budget.counts.lock().unwrap();
            assert_eq!(counts.active_jobs, 0);
            assert_eq!(counts.retained_handles, 0);
        }
        Ok(())
    }

    #[test]
    fn a_finished_process_still_requires_explicit_handle_resolution() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let program = crate::parser::parse("spawn as short\n  run \"true\"\n")?;
        let StepKind::SpawnJob { stages, .. } = &program.steps[0].kind else {
            bail!("parsed program did not contain a job");
        };
        let registry = Registry::default();
        let options = RunOptions {
            max_output_bytes: 1024,
            command_timeout: Some(Duration::from_secs(2)),
            ..RunOptions::new(directory.path().to_owned())
        };
        let signals = Signals::with_cancellation_and_deadline(
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            true,
            None,
        )?;
        preload_program_modules(&program, &registry, &signals, directory.path())?;
        let _run = signals.begin_job_run(options.max_output_bytes)?;
        let mut supervisor = Supervisor::new(&signals, options.max_output_bytes)?;
        supervisor.spawn(
            0,
            "short",
            stages,
            JobEnv {
                registry: &registry,
                bindings: &BTreeMap::new(),
                cwd: &crate::working_dir::WorkingDirectory::plain(directory.path()),
                options: &options,
                signals: &signals,
                environment: None,
            },
        )?;

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = supervisor.inspect(0, "short")?;
            let Value::Record(fields) = snapshot else {
                bail!("job snapshot was not a record");
            };
            if fields.get("state") == Some(&Value::String("completed".into())) {
                break;
            }
            if Instant::now() >= deadline {
                bail!("short job did not complete before the test deadline");
            }
            std::thread::yield_now();
        }

        let error = supervisor.finish_scope(0).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("explicitly awaited or cancelled")
        );
        supervisor.cancel_scope(0)?;
        supervisor.finish_scope(0)?;
        Ok(())
    }

    #[test]
    fn panic_before_launch_ack_is_bounded_and_reaps_created_children() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let program = crate::parser::parse(
            "spawn as doomed\n  run \"sh\" with arguments [\"-c\", \"exec sleep 30\"]\n",
        )?;
        let StepKind::SpawnJob { stages, .. } = &program.steps[0].kind else {
            bail!("parsed program did not contain a job");
        };
        let stages = stages.clone();
        let registry = Registry::default();
        let cwd = directory.path().to_owned();
        let options = RunOptions {
            max_output_bytes: 1024,
            command_timeout: Some(Duration::from_secs(3)),
            total_timeout: Some(Duration::from_secs(5)),
            ..RunOptions::new(directory.path().to_owned())
        };
        let mut signals = Signals::with_cancellation_and_deadline(
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            true,
            None,
        )?;
        preload_program_modules(&program, &registry, &signals, directory.path())?;
        let _run = signals.begin_job_run(options.max_output_bytes)?;
        let cancelled = Arc::clone(&signals.cancelled);
        let probe = Arc::new(JobAckPanicProbe {
            panic_next: AtomicBool::new(true),
            launched_pids: Mutex::new(Vec::new()),
        });
        signals.job_panic_before_ack = Some(Arc::clone(&probe));
        let (sender, receiver) = mpsc::channel();
        let runner = std::thread::spawn(move || {
            let mut supervisor = Supervisor::new(&signals, options.max_output_bytes)?;
            let result = supervisor.spawn(
                0,
                "doomed",
                &stages,
                JobEnv {
                    registry: &registry,
                    bindings: &BTreeMap::new(),
                    cwd: &crate::working_dir::WorkingDirectory::plain(cwd.clone()),
                    options: &options,
                    signals: &signals,
                    environment: None,
                },
            );
            sender
                .send((supervisor, result))
                .map_err(|_| anyhow!("panic regression receiver was dropped"))
        });

        let (mut supervisor, result) = match receiver.recv_timeout(Duration::from_secs(4)) {
            Ok(value) => value,
            Err(error) => {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                let _ = receiver.recv_timeout(Duration::from_secs(1));
                bail!("background spawn did not resolve its start acknowledgment: {error}");
            }
        };
        runner
            .join()
            .map_err(|_| anyhow!("panic regression runner panicked"))??;
        let failure = result.expect_err("injected pre-ack panic must fail spawn");
        let failure = failure
            .downcast_ref::<JobSpawnFailure>()
            .context("pre-ack spawn failure did not preserve certainty details")?;
        assert_eq!(failure.launched_processes, 1);
        assert_eq!(
            failure.result.effect_certainty,
            execution_result::EffectCertainty::Uncertain
        );

        let pids = probe
            .launched_pids
            .lock()
            .map_err(|_| anyhow!("panic probe lock is poisoned"))?
            .clone();
        assert_eq!(pids.len(), 1, "the panic must happen after a real spawn");
        let mut gone = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            // SAFETY: signal 0 sends nothing and only tests whether the pid exists. pids[0] is in
            // range because the assert_eq! above requires exactly one launched pid, and that pid
            // was recorded from the real spawn this test drives.
            let status = unsafe { libc::kill(pids[0] as libc::pid_t, 0) };
            if status == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                gone = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            gone,
            "panic unwind must terminate and reap the acknowledged PID"
        );
        supervisor.cancel_scope(0)?;
        supervisor.finish_scope(0)?;
        Ok(())
    }
}
