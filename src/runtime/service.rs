//! Reuse a parsed body for interval ticks, calendar ticks, or streamed line events.
use super::checkpoint::{CheckpointOptions, Journal};
use super::resume;
use super::*;
use crate::schedule::CronSchedule;
use chrono::{Datelike, Local, TimeZone, Timelike};
use std::collections::{BTreeSet, VecDeque};
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::process::ChildStdout;
use std::time::Duration;

pub struct ServiceLimits {
    pub max_runs: Option<usize>,
    pub max_record_bytes: usize,
}

/// Runtime-owned inputs that bind an imported persistent program to its graph
/// execution without creating a second scheduler or registry scope.
pub(crate) struct ProgramServiceContext {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) deadline: Option<Instant>,
    pub(crate) module_session: Arc<crate::modules::ProgramModuleSession>,
    pub(crate) additional_sensitivity: crate::security::SensitivitySet,
    pub(crate) process_observer: ProcessObserver,
}

/// What to do when one or more scheduled ticks pass while a handler is busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedTickPolicy {
    /// Drop overdue ticks and retain the original interval phase.
    Skip,
    /// Collapse any overdue ticks into at most one immediate handler run.
    Coalesce,
    /// Run a bounded number of overdue ticks, then discard the remainder.
    CatchUp { max_runs: usize },
}

/// How repeated local calendar minutes are treated during a clock change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarDedupPolicy {
    /// Treat each Unix epoch minute as a separate tick, including DST folds.
    AbsoluteMinute,
    /// Treat an identical local wall-clock minute as one tick, even in a DST fold.
    LocalMinute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleOptions {
    pub missed_ticks: MissedTickPolicy,
    pub calendar_dedup: CalendarDedupPolicy,
}

impl Default for ScheduleOptions {
    fn default() -> Self {
        Self {
            missed_ticks: MissedTickPolicy::Skip,
            calendar_dedup: CalendarDedupPolicy::AbsoluteMinute,
        }
    }
}

impl ScheduleOptions {
    fn validate(self) -> Result<()> {
        if let MissedTickPolicy::CatchUp { max_runs } = self.missed_ticks {
            if !(1..=MAX_CATCH_UP_RUNS).contains(&max_runs) {
                bail!("catch-up max_runs must be between 1 and {MAX_CATCH_UP_RUNS}");
            }
        }
        Ok(())
    }
}

const MAX_CATCH_UP_RUNS: usize = 64;
const CRON_CATCH_UP_LOOKBACK_MINUTES: i64 = 7 * 24 * 60;
const LOCAL_MINUTE_DEDUP_CAPACITY: usize = 2 * 24 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CalendarMinuteKey {
    Epoch(i64),
    Local(i32, u32, u32, u32, u32),
}

#[derive(Default)]
struct CalendarMinuteDedup {
    seen: BTreeSet<CalendarMinuteKey>,
    insertion_order: VecDeque<CalendarMinuteKey>,
}

impl CalendarMinuteDedup {
    fn accepts(
        &mut self,
        epoch_minute: i64,
        policy: CalendarDedupPolicy,
    ) -> Result<Option<chrono::DateTime<Local>>> {
        let Some(timestamp) = epoch_minute.checked_mul(60) else {
            bail!("calendar minute is out of range");
        };
        let Some(local) = Local.timestamp_opt(timestamp, 0).single() else {
            bail!("calendar minute is not representable in local time");
        };
        let key = match policy {
            CalendarDedupPolicy::AbsoluteMinute => CalendarMinuteKey::Epoch(epoch_minute),
            CalendarDedupPolicy::LocalMinute => CalendarMinuteKey::Local(
                local.year(),
                local.month(),
                local.day(),
                local.hour(),
                local.minute(),
            ),
        };
        if !self.seen.insert(key) {
            return Ok(None);
        }
        self.insertion_order.push_back(key);
        while self.insertion_order.len() > LOCAL_MINUTE_DEDUP_CAPACITY {
            if let Some(expired) = self.insertion_order.pop_front() {
                self.seen.remove(&expired);
            }
        }
        Ok(Some(local))
    }
}
#[derive(Default)]
pub struct ServiceResult {
    pub runs: usize,
    pub cache_hits: usize,
    /// Successful child/helper process spawns during completed handler runs.
    /// Descendants spawned internally by those processes are not included.
    pub launched_processes: usize,
}

pub(super) fn split_program(program: &Program) -> (Program, Program) {
    let line = match program.trigger.as_ref().expect("triggered program") {
        Trigger::Every { line, .. } | Trigger::Cron { line, .. } | Trigger::Listen { line, .. } => {
            *line
        }
    };
    let setup_length = program
        .steps
        .iter()
        .take_while(|step| step.line < line)
        .count();
    (
        Program {
            definitions: program.definitions.clone(),
            inputs: program.inputs.clone(),
            steps: program.steps[..setup_length].to_vec(),
            trigger: None,
        },
        Program {
            definitions: program.definitions.clone(),
            inputs: program.inputs.clone(),
            steps: program.steps[setup_length..].to_vec(),
            trigger: None,
        },
    )
}

fn stopped(signals: &Signals) -> Result<()> {
    if signals.cancelled.load(Ordering::Relaxed) {
        if signals.graph_service_cancellation {
            return Err(execution_result::ExecutionFailure::new(
                execution_result::ExecutionResult::runtime_failure(
                    execution_result::ExecutionKind::Cancelled,
                    "graph-owned program service cancelled",
                    execution_result::EffectCertainty::Uncertain,
                ),
            )
            .into());
        }
        bail!("program cancelled by signal");
    }
    check_deadline(signals.deadline)?;
    Ok(())
}

fn authorize_service_checkpoint(
    registry: &Registry,
    sensitivities: &crate::security::SensitivitySet,
) -> Result<()> {
    crate::security::authorize_persistence_set(
        registry,
        "workflow.checkpoint",
        crate::security::PersistenceKind::Checkpoint,
        sensitivities,
    )?;
    Ok(())
}

fn limit_reached(limits: &ServiceLimits, result: &ServiceResult) -> bool {
    limits.max_runs.is_some_and(|limit| result.runs >= limit)
}

fn listener_source_exited_successfully(
    manifest: &Manifest,
    status: std::process::ExitStatus,
) -> bool {
    status
        .code()
        .is_some_and(|code| manifest.outcome_for(code).kind == OutcomeKind::Success)
}

fn trace_event(
    enabled: bool,
    signals: &Signals,
    value: impl FnOnce() -> serde_json::Value,
) -> Result<()> {
    if enabled {
        event(true, value(), signals)
    } else {
        Ok(())
    }
}

/// Find the next contiguous part of a listener record without scanning beyond
/// the configured record limit. The returned length includes a newline when
/// one is present.
fn next_record_segment(
    bytes: &[u8],
    record_len: usize,
    max_record_bytes: usize,
) -> Result<(usize, bool)> {
    if bytes.is_empty() {
        return Ok((0, false));
    }
    let remaining = max_record_bytes.saturating_sub(record_len);
    let probe_len = bytes.len().min(remaining.saturating_add(1));
    if let Some(newline) = bytes[..probe_len].iter().position(|byte| *byte == b'\n') {
        let length = newline + 1;
        if length > remaining {
            bail!("listener record exceeds {max_record_bytes} bytes");
        }
        return Ok((length, true));
    }
    if bytes.len() > remaining {
        bail!("listener record exceeds {max_record_bytes} bytes");
    }
    Ok((bytes.len(), false))
}

#[allow(clippy::too_many_arguments)]
fn fire(
    body: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    input: Option<(&str, Vec<u8>)>,
    emit: &mut impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
    result: &mut ServiceResult,
    mut journal: Option<&mut Journal>,
) -> Result<()> {
    stopped(signals)?;
    let started = Instant::now();
    let (input, binding) = match input {
        Some((name, bytes)) => {
            let text =
                String::from_utf8(bytes.clone()).context("listener records must be UTF-8 text")?;
            (
                Input::Bytes(bytes),
                Some((name.to_owned(), Value::String(text))),
            )
        }
        None => (Input::Empty, None),
    };
    let record_bytes = match &input {
        Input::Bytes(bytes) => bytes.len(),
        _ => 0,
    };
    let state_source = if let Some(journal) = journal.as_deref_mut() {
        if journal.has_pending() {
            stopped(signals)?;
            resume::prepare(body, registry, journal)?;
            stopped(signals)?;
            FiniteStateSource::Restore
        } else {
            let snapshot_output = match &input {
                Input::Bytes(bytes) => bytes.as_slice(),
                _ => &[],
            };
            let snapshot_bindings = binding.as_ref().map(|(name, value)| (name.as_str(), value));
            stopped(signals)?;
            authorize_service_checkpoint(registry, &signals.sensitivity_floor())?;
            stopped(signals)?;
            if binding.is_some() {
                journal.begin_listener_event_parts(
                    &options.working_directory,
                    snapshot_output,
                    snapshot_bindings,
                )?;
            } else {
                journal.begin_execution_parts(
                    &options.working_directory,
                    snapshot_output,
                    snapshot_bindings,
                )?;
            }
            stopped(signals)?;
            FiniteStateSource::Fresh
        }
    } else {
        FiniteStateSource::Fresh
    };
    if binding.is_some() {
        trace_event(
            options.trace,
            signals,
            || serde_json::json!({"event":"line_received","bytes":record_bytes}),
        )?;
    }
    trace_event(
        options.trace,
        signals,
        || serde_json::json!({"event":"handler_start","run":result.runs + 1}),
    )?;
    let finite = if let Some(journal) = journal.as_deref_mut() {
        run_finite_inner_with_state_source(
            body,
            registry,
            options,
            signals,
            input,
            binding,
            Some(journal),
            state_source,
        )?
    } else {
        run_finite(body, registry, options, signals, input, binding)?
    };
    if let Err(error) = emit(&finite.output, &signals.cancelled, signals.deadline) {
        if let Some(journal) = journal.as_deref_mut() {
            if authorize_service_checkpoint(registry, &signals.sensitivity_floor()).is_ok() {
                let _ = journal.fail(
                    &format!("{error:#}"),
                    signals.cancelled.load(Ordering::Relaxed),
                );
            }
        }
        return Err(error);
    }
    if let Some(journal) = journal {
        authorize_service_checkpoint(registry, &signals.sensitivity_floor())?;
        journal.finish()?;
        stopped(signals)?;
    }
    result.runs += 1;
    result.cache_hits += finite.cache_hits;
    result.launched_processes += finite.launched_processes;
    trace_event(
        options.trace,
        signals,
        || serde_json::json!({"event":"handler_end","run":result.runs,"elapsed_us":started.elapsed().as_micros(),"bytes":finite.output.len()}),
    )?;
    Ok(())
}
fn wait_until(deadline: Instant, signals: &Signals) -> Result<()> {
    loop {
        stopped(signals)?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}
pub(crate) fn next_interval(due: Instant, now: Instant, interval: Duration) -> Result<Instant> {
    if interval.is_zero() {
        bail!("interval must be greater than zero");
    }
    if due > now {
        return Ok(due);
    }
    // Keep the original phase but skip missed ticks rather than overlap/catch up.
    let remaining =
        interval.as_nanos() - now.saturating_duration_since(due).as_nanos() % interval.as_nanos();
    let wait = Duration::new(
        (remaining / 1_000_000_000).try_into()?,
        (remaining % 1_000_000_000) as u32,
    );
    now.checked_add(wait)
        .ok_or_else(|| anyhow!("interval deadline is out of range"))
}

fn overdue_intervals(
    next_due: Instant,
    now: Instant,
    interval: Duration,
) -> Result<(Instant, usize)> {
    if next_due > now {
        return Ok((next_due, 0));
    }
    if interval.is_zero() {
        bail!("interval must be greater than zero");
    }
    let missed = now
        .duration_since(next_due)
        .as_nanos()
        .checked_div(interval.as_nanos())
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| anyhow!("missed interval count is out of range"))?;
    let missed = usize::try_from(missed).unwrap_or(usize::MAX);
    Ok((next_interval(next_due, now, interval)?, missed))
}

fn apply_missed_interval_policy(
    next_due: Instant,
    now: Instant,
    interval: Duration,
    pending_runs: usize,
    policy: MissedTickPolicy,
) -> Result<(Instant, usize)> {
    match policy {
        MissedTickPolicy::Skip => Ok((next_interval(next_due, now, interval)?, 0)),
        MissedTickPolicy::Coalesce => {
            let (next_due, missed) = overdue_intervals(next_due, now, interval)?;
            Ok((next_due, usize::from(missed > 0)))
        }
        MissedTickPolicy::CatchUp { max_runs } => {
            if !(1..=MAX_CATCH_UP_RUNS).contains(&max_runs) {
                bail!("catch-up max_runs must be between 1 and {MAX_CATCH_UP_RUNS}");
            }
            let (next_due, missed) = overdue_intervals(next_due, now, interval)?;
            Ok((next_due, pending_runs.saturating_add(missed).min(max_runs)))
        }
    }
}

/// Run a foreground service. No OS cron, launchd or systemd configuration is changed.
pub fn run_service(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    mut emit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<ServiceResult> {
    run_service_with_cancellation(program, registry, options, limits, |bytes, _| emit(bytes))
}

/// Run a service with a cooperative output sink. Blocking sinks must poll the
/// cancellation flag themselves; cancellation cannot interrupt arbitrary code.
pub fn run_service_with_cancellation(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    mut emit: impl FnMut(&[u8], &AtomicBool) -> Result<()>,
) -> Result<ServiceResult> {
    run_service_internal(
        program,
        registry,
        options,
        limits,
        None,
        ScheduleOptions::default(),
        |bytes, cancelled, _deadline| emit(bytes, cancelled),
    )
}

/// Run a service with explicit missed-tick and calendar deduplication policy.
/// The output sink receives the service's absolute deadline so blocking output
/// can use the same workflow budget as execution and scheduling.
pub fn run_service_with_schedule_options(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    schedule: &ScheduleOptions,
    emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<ServiceResult> {
    run_service_internal(program, registry, options, limits, None, *schedule, emit)
}

/// Run a service with durable handler checkpoints in `checkpoint`.
pub fn run_service_checkpointed(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    checkpoint: &CheckpointOptions,
    mut emit: impl FnMut(&[u8], &AtomicBool) -> Result<()>,
) -> Result<ServiceResult> {
    run_service_internal(
        program,
        registry,
        options,
        limits,
        Some(checkpoint),
        ScheduleOptions::default(),
        |bytes, cancelled, _deadline| emit(bytes, cancelled),
    )
}

/// Run a checkpointed service with explicit scheduling policy.
pub fn run_service_checkpointed_with_schedule_options(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    checkpoint: &CheckpointOptions,
    schedule: &ScheduleOptions,
    emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<ServiceResult> {
    run_service_internal(
        program,
        registry,
        options,
        limits,
        Some(checkpoint),
        *schedule,
        emit,
    )
}

fn run_service_internal(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    checkpoint: Option<&CheckpointOptions>,
    schedule_options: ScheduleOptions,
    emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<ServiceResult> {
    schedule_options.validate()?;
    let service_started = Instant::now();
    let workflow_deadline = options
        .total_timeout
        .map(|timeout| {
            service_started
                .checked_add(timeout)
                .context("total workflow timeout is too large")
        })
        .transpose()?;
    let signals = Signals::register_with_deadline(workflow_deadline)?;
    run_service_with_signals(
        program,
        registry,
        options,
        limits,
        checkpoint,
        schedule_options,
        signals,
        None,
        emit,
    )
}

/// Run one imported interface-2 `.program` service inside its owning graph.
/// The graph supplies cancellation, deadline, preloaded module sources and
/// input provenance; handler execution and scheduling still use the ordinary
/// service runtime and native process supervisor. The source trigger selects
/// either schedule ticks or one sequential handler per native listener line.
pub(crate) fn run_program_module_service(
    loaded: &crate::modules::LoadedProgramSource,
    registry: &Registry,
    options: &RunOptions,
    context: ProgramServiceContext,
    emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<ServiceResult> {
    if !loaded.is_program_service() {
        bail!(
            "program module {:?} is a finite export, not a persistent service",
            loaded.reference.entry
        );
    }
    match loaded.program.trigger.as_ref() {
        Some(Trigger::Every { .. } | Trigger::Cron { .. }) => {}
        Some(Trigger::Listen { .. }) => {
            crate::modules::validate_program_module_listener_source(&loaded.program, registry)?;
        }
        None => bail!("program module service requires a schedule or applet-listener trigger"),
    }
    let deadline = context.deadline;
    let mut signals = Signals::with_cancellation_and_deadline(context.cancelled, false, deadline)?;
    signals.graph_service_cancellation = true;
    let registry_pool = registry.host_state::<worker_backend::WorkerPool>();
    signals.worker_pool = Some(Arc::new(registry_pool.isolated_like()));
    let service_cancelled = Arc::clone(&signals.cancelled);
    signals.set_program_module_session(context.module_session)?;
    signals.set_process_observer(context.process_observer)?;
    let limits = ServiceLimits {
        max_runs: None,
        max_record_bytes: options.max_output_bytes.max(1),
    };
    let result = run_service_with_signals(
        &loaded.program,
        registry,
        options,
        &limits,
        None,
        ScheduleOptions::default(),
        signals,
        Some(context.additional_sensitivity),
        emit,
    );
    let cancelled_shutdown = result.as_ref().is_err_and(|error| {
        service_cancelled.load(Ordering::Acquire)
            && deadline.is_none_or(|deadline| Instant::now() < deadline)
            && error
                .downcast_ref::<execution_result::ExecutionFailure>()
                .is_some_and(|failure| {
                    failure.result.kind == execution_result::ExecutionKind::Cancelled
                })
    });
    if cancelled_shutdown {
        return Ok(ServiceResult::default());
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn run_service_with_signals(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    checkpoint: Option<&CheckpointOptions>,
    schedule_options: ScheduleOptions,
    mut signals: Signals,
    additional_sensitivity: Option<crate::security::SensitivitySet>,
    mut emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<ServiceResult> {
    schedule_options.validate()?;
    stopped(&signals)?;
    validate(program, registry)?;
    stopped(&signals)?;
    let capability_cwd = fs::canonicalize(&options.working_directory)?;
    stopped(&signals)?;
    crate::capabilities::preflight_with_cache_directory(
        program,
        registry,
        &capability_cwd,
        options.cache_directory.as_deref(),
    )?;
    stopped(&signals)?;
    let module_session =
        super::preload_program_modules(program, registry, &signals, &capability_cwd)?;
    if module_session.has_background_job_operations() {
        bail!("native background jobs are unsupported in services and listeners, including imported program modules");
    }
    signals.background_jobs_allowed = false;
    let mut sensitivity_floor = super::workflow_sensitivity_floor(
        program,
        registry,
        &module_session,
        &capability_cwd,
        signals.worker_pool.as_deref(),
    )?;
    if let Some(additional_sensitivity) = additional_sensitivity {
        sensitivity_floor.extend(&additional_sensitivity);
    }
    signals.set_sensitivity_floor(sensitivity_floor);
    stopped(&signals)?;
    if checkpoint.is_some() {
        module_session.validate_checkpoint_identity()?;
        stopped(&signals)?;
        resume::validate_checkpoint_controls(program, registry)?;
        stopped(&signals)?;
    }
    let configured = configured_inputs(program, registry, options)?;
    stopped(&signals)?;
    let options = configured.as_ref();
    let trigger = program
        .trigger
        .as_ref()
        .ok_or_else(|| anyhow!("service requires a listener or schedule"))?;
    if limits.max_runs == Some(0) || limits.max_record_bytes == 0 {
        bail!("service limits must be positive");
    }
    let (setup, body) = split_program(program);
    if body.steps.is_empty() {
        bail!("service body must contain a statement");
    }
    let configured_cwd = fs::canonicalize(&options.working_directory)?;
    stopped(&signals)?;
    let mut cwd = configured_cwd.clone();
    for step in setup.steps {
        match step.kind {
            StepKind::UseFolder { path } => {
                let folder = path_value(&path, &options.inputs, &cwd)?;
                stopped(&signals)?;
                crate::capabilities::authorize_host_path(registry, &folder, false, &cwd)?;
                stopped(&signals)?;
                cwd = fs::canonicalize(folder)?;
                stopped(&signals)?;
                if !cwd.is_dir() {
                    bail!("line {}: use folder requires a directory", step.line);
                }
            }
            StepKind::Invoke { call, .. }
                if registry.resolve_call(&call)?.builtin == Some(Builtin::Cd) =>
            {
                let args =
                    arguments_for_call(&call, registry.resolve_call(&call)?, &options.inputs)?;
                let destination = cwd.join(&args[0]);
                stopped(&signals)?;
                crate::capabilities::authorize_host_path(registry, &destination, false, &cwd)?;
                stopped(&signals)?;
                cwd = fs::canonicalize(destination)
                    .with_context(|| format!("line {}: cd to {:?}", step.line, args[0]))?;
                stopped(&signals)?;
                if !cwd.is_dir() {
                    bail!("line {}: cd requires a directory", step.line);
                }
            }
            _ => bail!("service setup requires a folder directive or the cd builtin"),
        }
    }
    let options = RunOptions {
        inputs: options.inputs.clone(),
        working_directory: cwd,
        cache_directory: options
            .cache_directory
            .as_ref()
            .map(|path| configured_cwd.join(path)),
        trace: options.trace,
        max_output_bytes: options.max_output_bytes,
        command_timeout: options.command_timeout,
        total_timeout: options.total_timeout,
    };
    let mut journal = if let Some(checkpoint) = checkpoint {
        stopped(&signals)?;
        let fingerprint =
            resume::trigger_fingerprint(trigger, registry, &options.working_directory)?;
        stopped(&signals)?;
        let mut checkpoint = checkpoint.clone();
        if checkpoint.directory.is_relative() {
            checkpoint.directory = configured_cwd.join(&checkpoint.directory);
        }
        stopped(&signals)?;
        crate::capabilities::authorize_host_path(
            registry,
            &checkpoint.directory,
            true,
            &configured_cwd,
        )?;
        stopped(&signals)?;
        authorize_service_checkpoint(registry, &signals.sensitivity_floor())?;
        let journal = Journal::open_with_inputs(
            &checkpoint,
            &options.working_directory,
            &format!("service:{fingerprint}"),
            options.max_output_bytes,
            input_signature(program, &options.inputs)?,
        )?;
        stopped(&signals)?;
        Some(journal)
    } else {
        None
    };
    let mut result = ServiceResult::default();

    // A checkpointed handler is independent of the event that originally
    // triggered it. Finish it before starting a new listener or waiting for a
    // future schedule tick.
    if journal.as_ref().is_some_and(Journal::has_pending) {
        fire(
            &body,
            registry,
            &options,
            &signals,
            None,
            &mut emit,
            &mut result,
            journal.as_mut(),
        )?;
        if limit_reached(limits, &result) {
            return Ok(result);
        }
    }
    match trigger {
        Trigger::Every { interval_ms, .. } => {
            let interval = Duration::from_millis(*interval_ms);
            if interval.is_zero() {
                bail!("interval must be greater than zero");
            }
            let mut next_due = Instant::now()
                .checked_add(interval)
                .ok_or_else(|| anyhow!("interval deadline is out of range"))?;
            let mut pending_runs = 0usize;
            loop {
                let scheduled_due = if pending_runs == 0 {
                    wait_until(next_due, &signals)?;
                    let scheduled_due = next_due;
                    next_due = next_due
                        .checked_add(interval)
                        .ok_or_else(|| anyhow!("interval deadline is out of range"))?;
                    Some(scheduled_due)
                } else {
                    pending_runs -= 1;
                    stopped(&signals)?;
                    None
                };
                let lateness = scheduled_due.map_or(0, |due| {
                    Instant::now().saturating_duration_since(due).as_micros()
                });
                trace_event(
                    options.trace,
                    &signals,
                    || serde_json::json!({"event":"interval_tick","lateness_us":lateness,"pending":pending_runs}),
                )?;
                fire(
                    &body,
                    registry,
                    &options,
                    &signals,
                    None,
                    &mut emit,
                    &mut result,
                    journal.as_mut(),
                )?;
                if limit_reached(limits, &result) {
                    return Ok(result);
                }
                let now = Instant::now();
                (next_due, pending_runs) = apply_missed_interval_policy(
                    next_due,
                    now,
                    interval,
                    pending_runs,
                    schedule_options.missed_ticks,
                )?;
            }
        }
        Trigger::Cron { expression, .. } => {
            let schedule = CronSchedule::parse(expression)?;
            // Begin at the next calendar minute. Catch-up scans only a bounded
            // recent window so a long outage cannot produce unbounded work.
            let mut last_minute = Local::now().timestamp().div_euclid(60);
            let mut dedup = CalendarMinuteDedup::default();
            let mut pending_ticks = VecDeque::new();
            loop {
                stopped(&signals)?;
                let now = Local::now();
                let minute = now.timestamp().div_euclid(60);
                if minute > last_minute {
                    let first_minute = match schedule_options.missed_ticks {
                        MissedTickPolicy::Skip => minute,
                        MissedTickPolicy::Coalesce | MissedTickPolicy::CatchUp { .. } => {
                            (minute - CRON_CATCH_UP_LOOKBACK_MINUTES + 1).max(last_minute + 1)
                        }
                    };
                    let mut matched = Vec::new();
                    for candidate_minute in first_minute..=minute {
                        let candidate = match dedup
                            .accepts(candidate_minute, schedule_options.calendar_dedup)?
                        {
                            Some(candidate) if schedule.matches(&candidate) => candidate,
                            _ => continue,
                        };
                        matched.push(candidate);
                    }
                    last_minute = minute;
                    match schedule_options.missed_ticks {
                        MissedTickPolicy::Skip => {
                            if let Some(candidate) = matched.into_iter().last() {
                                pending_ticks.push_back(candidate);
                            }
                        }
                        MissedTickPolicy::Coalesce => {
                            if let Some(candidate) = matched.into_iter().last() {
                                pending_ticks.clear();
                                pending_ticks.push_back(candidate);
                            }
                        }
                        MissedTickPolicy::CatchUp { max_runs } => {
                            let available = max_runs.saturating_sub(pending_ticks.len());
                            for candidate in matched.into_iter().take(available) {
                                pending_ticks.push_back(candidate);
                            }
                        }
                    }
                }
                if let Some(candidate) = pending_ticks.pop_front() {
                    trace_event(
                        options.trace,
                        &signals,
                        || serde_json::json!({"event":"calendar_tick","local_time":candidate.to_rfc3339()}),
                    )?;
                    fire(
                        &body,
                        registry,
                        &options,
                        &signals,
                        None,
                        &mut emit,
                        &mut result,
                        journal.as_mut(),
                    )?;
                    if limit_reached(limits, &result) {
                        return Ok(result);
                    }
                    continue;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Trigger::Listen {
            source, binding, ..
        } => {
            listen(
                source,
                binding,
                &body,
                registry,
                &options,
                limits,
                &signals,
                &mut emit,
                &mut result,
                journal.as_mut(),
            )?;
            Ok(result)
        }
    }
}

#[cfg(unix)]
include!("unix_listener.rs");
#[cfg(windows)]
include!("windows_listener.rs");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_segments_include_newlines_and_allow_chunk_continuation() {
        assert_eq!(next_record_segment(b"abc\ndef\n", 0, 8).unwrap(), (4, true));
        assert_eq!(next_record_segment(b"def\n", 0, 8).unwrap(), (4, true));
        assert_eq!(next_record_segment(b"ab", 0, 4).unwrap(), (2, false));
        assert_eq!(next_record_segment(b"c\n", 2, 4).unwrap(), (2, true));
        assert_eq!(next_record_segment(b"", 0, 4).unwrap(), (0, false));
    }

    #[test]
    fn record_limit_counts_newline_and_carries_partial_record_length() {
        assert_eq!(next_record_segment(b"abc\n", 0, 4).unwrap(), (4, true));
        assert!(next_record_segment(b"abcd\n", 0, 4).is_err());
        assert!(next_record_segment(b"bc", 3, 4).is_err());
    }

    #[test]
    fn disabled_trace_does_not_construct_event_payload() {
        let signals =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(false)), false, None)
                .unwrap();
        let constructed = std::cell::Cell::new(false);
        trace_event(false, &signals, || {
            constructed.set(true);
            serde_json::json!({"event":"unused"})
        })
        .unwrap();
        assert!(!constructed.get());
    }

    #[test]
    fn skips_missed_intervals_without_drifting_phase() {
        let due = Instant::now();
        let interval = Duration::from_millis(10);
        let now = due + Duration::from_millis(35);
        assert_eq!(
            next_interval(due, now, interval).unwrap(),
            due + Duration::from_millis(40)
        );
        assert_eq!(
            next_interval(due, now + Duration::from_micros(125), interval).unwrap(),
            due + Duration::from_millis(40)
        );
    }

    #[test]
    fn interval_policies_skip_coalesce_and_cap_catch_up() {
        let origin = Instant::now();
        let interval = Duration::from_millis(10);
        let first_unconsumed = origin + interval;
        let now = origin + Duration::from_millis(35);

        let (next, pending) = apply_missed_interval_policy(
            first_unconsumed,
            now,
            interval,
            0,
            MissedTickPolicy::Skip,
        )
        .unwrap();
        assert_eq!(next, origin + Duration::from_millis(40));
        assert_eq!(pending, 0);

        let (next, pending) = apply_missed_interval_policy(
            first_unconsumed,
            now,
            interval,
            0,
            MissedTickPolicy::Coalesce,
        )
        .unwrap();
        assert_eq!(next, origin + Duration::from_millis(40));
        assert_eq!(pending, 1);

        let (next, pending) = apply_missed_interval_policy(
            first_unconsumed,
            now,
            interval,
            2,
            MissedTickPolicy::CatchUp { max_runs: 4 },
        )
        .unwrap();
        assert_eq!(next, origin + Duration::from_millis(40));
        assert_eq!(pending, 4);

        let (_, pending) = apply_missed_interval_policy(
            first_unconsumed,
            now,
            interval,
            0,
            MissedTickPolicy::CatchUp { max_runs: 2 },
        )
        .unwrap();
        assert_eq!(pending, 2);
    }

    #[test]
    fn scheduler_options_are_bounded_and_default_to_skip() {
        assert_eq!(
            ScheduleOptions::default(),
            ScheduleOptions {
                missed_ticks: MissedTickPolicy::Skip,
                calendar_dedup: CalendarDedupPolicy::AbsoluteMinute,
            }
        );
        assert!(ScheduleOptions {
            missed_ticks: MissedTickPolicy::CatchUp { max_runs: 0 },
            calendar_dedup: CalendarDedupPolicy::AbsoluteMinute,
        }
        .validate()
        .is_err());
        assert!(ScheduleOptions {
            missed_ticks: MissedTickPolicy::CatchUp {
                max_runs: MAX_CATCH_UP_RUNS + 1,
            },
            calendar_dedup: CalendarDedupPolicy::AbsoluteMinute,
        }
        .validate()
        .is_err());
        assert!(ScheduleOptions {
            missed_ticks: MissedTickPolicy::CatchUp { max_runs: 1 },
            calendar_dedup: CalendarDedupPolicy::AbsoluteMinute,
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn total_timeout_bounds_a_scheduled_wait() {
        let program =
            crate::parser::parse("every 60 seconds\n  then encode \"ready\" as json\n").unwrap();
        let options = RunOptions {
            inputs: BTreeMap::new(),
            working_directory: std::env::current_dir().unwrap(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024,
            command_timeout: None,
            total_timeout: Some(Duration::from_millis(25)),
        };
        let started = Instant::now();
        let result = run_service_with_schedule_options(
            &program,
            &Registry::default(),
            &options,
            &ServiceLimits {
                max_runs: None,
                max_record_bytes: 1024,
            },
            &ScheduleOptions::default(),
            |_, _, _| Ok(()),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn calendar_dedup_tracks_epoch_or_wall_clock_keys_with_a_cap() {
        let mut dedup = CalendarMinuteDedup::default();
        let minute = Local::now().timestamp().div_euclid(60);
        assert!(dedup
            .accepts(minute, CalendarDedupPolicy::AbsoluteMinute)
            .unwrap()
            .is_some());
        assert!(dedup
            .accepts(minute, CalendarDedupPolicy::AbsoluteMinute)
            .unwrap()
            .is_none());

        let local_minute = minute + 1;
        assert!(dedup
            .accepts(local_minute, CalendarDedupPolicy::LocalMinute)
            .unwrap()
            .is_some());
        assert!(dedup
            .accepts(local_minute, CalendarDedupPolicy::LocalMinute)
            .unwrap()
            .is_none());
        assert!(LOCAL_MINUTE_DEDUP_CAPACITY <= CRON_CATCH_UP_LOOKBACK_MINUTES as usize);
    }
}
