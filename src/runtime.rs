//! Unix program executor with finite pipelines and persistent event services.
use crate::ast::{Connection, Invocation, ListenSource, Program, StepKind, Trigger, Value};
use crate::contracts::{AppletBackend, Lifecycle, OutcomeKind};
use crate::manifest::{AppletOutput, Builtin, Manifest, Registry};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::fd::{FromRawFd, OwnedFd};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(all(test, unix))]
use std::process::Command;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

mod cache;
mod calls;
mod checkpoint;
mod control;
mod control_flow;
pub mod execution_result;
pub mod invocation_io;
pub(crate) mod live;
mod loops;
mod program_backend;
mod resume;
mod service;
mod streaming;
pub mod tasks;
pub mod worker_backend;
pub use checkpoint::CheckpointOptions;
pub use resume::run_checkpointed;
pub use streaming::{
    run_stream, run_stream_with_cancellation, run_stream_with_context, StreamExecutionContext,
    StreamResult,
};
#[cfg_attr(windows, path = "runtime/windows_stdio.rs")]
mod stdio;
pub(crate) use service::next_interval;
pub use service::{
    run_service, run_service_checkpointed, run_service_checkpointed_with_schedule_options,
    run_service_with_cancellation, run_service_with_schedule_options, CalendarDedupPolicy,
    MissedTickPolicy, ScheduleOptions, ServiceLimits, ServiceResult,
};
pub use stdio::{write_cancellable, write_diagnostic, write_with_deadline};

#[derive(Clone)]
pub struct RunOptions {
    pub inputs: BTreeMap<String, Value>,
    pub working_directory: PathBuf,
    pub cache_directory: Option<PathBuf>,
    pub trace: bool,
    pub max_output_bytes: usize,
    /// Budget for each native pipeline group, finite stdin capture and
    /// cooperative checkpoints in an in-process applet. It cannot interrupt a
    /// synchronous OS or library call.
    pub command_timeout: Option<Duration>,
    /// Maximum lifetime of the full runtime operation, including scheduler waits.
    pub total_timeout: Option<Duration>,
}
#[derive(Debug)]
struct CommandTimedOut;
impl std::fmt::Display for CommandTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("command timed out")
    }
}
impl std::error::Error for CommandTimedOut {}
#[derive(Debug)]
pub struct AppletInvocationError {
    source: anyhow::Error,
    launched_processes: usize,
}
impl AppletInvocationError {
    pub fn launched_processes(&self) -> usize {
        self.launched_processes
    }

    /// Whether a failed invocation may have completed external effects.
    pub fn uncertain(&self) -> bool {
        execution_result::effect_certainty(&self.source)
            == execution_result::EffectCertainty::Uncertain
    }
}
impl std::fmt::Display for AppletInvocationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.source)
    }
}
impl std::error::Error for AppletInvocationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}
fn check_deadline(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(
            anyhow!("execution deadline exceeded; external effects may be incomplete")
                .context(CommandTimedOut),
        );
    }
    Ok(())
}
fn check_execution_budget(cancelled: &AtomicBool, deadline: Option<Instant>) -> Result<()> {
    check_deadline(deadline)?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(execution_result::ExecutionFailure::new(
            execution_result::ExecutionResult::runtime_failure(
                execution_result::ExecutionKind::Cancelled,
                "workflow cancelled by signal",
                execution_result::EffectCertainty::Uncertain,
            ),
        )
        .into());
    }
    Ok(())
}

fn is_timeout_error(error: &anyhow::Error) -> bool {
    error.is::<CommandTimedOut>()
        || error
            .downcast_ref::<execution_result::ExecutionFailure>()
            .is_some_and(|failure| failure.result.kind == execution_result::ExecutionKind::Timeout)
}
fn deadline_after(timeout: Option<Duration>, label: &str) -> Result<Option<Instant>> {
    timeout
        .map(|duration| {
            Instant::now()
                .checked_add(duration)
                .with_context(|| format!("{label} is too large"))
        })
        .transpose()
}
#[cfg(unix)]
fn deadline_poll_timeout(deadline: Option<Instant>, maximum_ms: libc::c_int) -> libc::c_int {
    deadline.map_or(maximum_ms, |deadline| {
        let remaining_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            / 1_000_000;
        remaining_ms.min(maximum_ms as u128) as libc::c_int
    })
}
pub struct RunResult {
    pub output: Vec<u8>,
    pub cache_hits: usize,
    pub launched_processes: usize,
    pub executions: Vec<execution_result::ExecutionRecord>,
    pub dropped_executions: usize,
    pub dropped_stages: usize,
}

/// Result of one explicit applet invocation through its declared backend.
/// `output` always preserves the exact stdout bytes; `value` is available only
/// when a typed contract successfully decodes the result (or creates an
/// explicit result envelope).
#[derive(Debug, Clone)]
pub struct AppletInvocationResult {
    pub output: Vec<u8>,
    pub status: i32,
    pub outcome: crate::contracts::Outcome,
    pub value: Option<Value>,
    pub stderr: Vec<u8>,
    pub uncertain: bool,
    /// Actual helper/native processes started by this single invocation.
    pub launched_processes: usize,
}

#[cfg(unix)]
const SMALL_STDIN_PREFILL_LIMIT: usize = 64 * 1024;

fn configured_inputs<'a>(
    program: &Program,
    registry: &Registry,
    options: &'a RunOptions,
) -> Result<Cow<'a, RunOptions>> {
    if program.inputs.is_empty() && options.inputs.is_empty() {
        return Ok(Cow::Borrowed(options));
    }
    // Resolve supplied values once; cloning options first copied the complete
    // input tree only to discard that copy when replacing the inputs field.
    let configured = RunOptions {
        inputs: crate::inputs::resolve(&program.inputs, &options.inputs, false)?,
        working_directory: options.working_directory.clone(),
        cache_directory: options.cache_directory.clone(),
        trace: options.trace,
        max_output_bytes: options.max_output_bytes,
        command_timeout: options.command_timeout,
        total_timeout: options.total_timeout,
    };
    preflight_inputs(program, registry, &configured.inputs)?;
    Ok(Cow::Owned(configured))
}

fn input_signature(program: &Program, values: &BTreeMap<String, Value>) -> Result<Option<String>> {
    if program.inputs.is_empty() {
        return Ok(None);
    }
    let signature = program
        .inputs
        .iter()
        .map(|d| (&d.name, (d.kind, values.get(&d.name))))
        .collect::<BTreeMap<_, _>>();
    let digest = Sha256::digest(serde_json::to_vec(&signature)?);
    Ok(Some(format!("inputs-v1:{digest:x}")))
}
include!("runtime/shared.rs");

fn capture(mut reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(output);
        }
        if count > limit.saturating_sub(output.len()) {
            bail!("output exceeds maximum of {limit} bytes");
        }
        output.extend_from_slice(&chunk[..count]);
    }
}
enum Input {
    Empty,
    Bytes(Vec<u8>),
    File(File),
    Stdin,
}
impl Input {
    fn materialize(
        self,
        limit: usize,
        signals: &Signals,
        timeout: Option<Duration>,
    ) -> Result<Vec<u8>> {
        match self {
            Self::Empty => Ok(Vec::new()),
            Self::Bytes(bytes) => Ok(bytes),
            Self::File(file) => capture(file, limit),
            Self::Stdin => stdio::capture_stdin(limit, signals, timeout),
        }
    }
}

#[cfg(unix)]
include!("runtime/unix_process.rs");
#[cfg(windows)]
include!("runtime/windows_process.rs");

pub(crate) struct Signals {
    cancelled: Arc<AtomicBool>,
    registrations: Vec<signal_hook::SigId>,
    capture_diagnostics: bool,
    deadline: Option<Instant>,
    program_modules: Arc<Mutex<Option<Arc<crate::modules::ProgramModuleSession>>>>,
    sensitivity_floor: Arc<OnceLock<crate::security::SensitivitySet>>,
}
impl Signals {
    pub(crate) fn register_with_deadline(deadline: Option<Instant>) -> Result<Self> {
        let mut guard = Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            registrations: Vec::with_capacity(3),
            capture_diagnostics: false,
            deadline,
            program_modules: Arc::new(Mutex::new(None)),
            sensitivity_floor: Arc::new(OnceLock::new()),
        };
        guard.registrations = crate::platform::signals(guard.cancelled.clone())?;
        Ok(guard)
    }

    pub(crate) fn register_with_cancellation_and_deadline(
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        let mut guard = Self::with_cancellation_and_deadline(cancelled, false, deadline)?;
        guard.registrations = crate::platform::signals(guard.cancelled.clone())?;
        Ok(guard)
    }

    #[cfg(test)]
    fn with_cancellation(cancelled: Arc<AtomicBool>, capture_diagnostics: bool) -> Result<Self> {
        Self::with_cancellation_and_deadline(cancelled, capture_diagnostics, None)
    }

    pub(crate) fn with_cancellation_and_deadline(
        cancelled: Arc<AtomicBool>,
        capture_diagnostics: bool,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        Ok(Self {
            cancelled,
            registrations: Vec::new(),
            capture_diagnostics,
            deadline,
            program_modules: Arc::new(Mutex::new(None)),
            sensitivity_floor: Arc::new(OnceLock::new()),
        })
    }

    fn set_sensitivity_floor(&self, sensitivity: crate::security::SensitivitySet) {
        let _ = self.sensitivity_floor.set(sensitivity);
    }

    fn sensitivity_floor(&self) -> crate::security::SensitivitySet {
        self.sensitivity_floor.get().cloned().unwrap_or_default()
    }
}

/// Load and validate every statically reachable program module before a
/// producer, journal, or workflow step can have effects. The immutable
/// snapshot is retained on Signals for all nested execution calls.
pub(crate) fn preload_program_modules(
    program: &Program,
    registry: &Registry,
    signals: &Signals,
    cwd: &Path,
) -> Result<Arc<crate::modules::ProgramModuleSession>> {
    let mut slot = signals
        .program_modules
        .lock()
        .map_err(|_| anyhow!("program module session lock is poisoned"))?;
    if let Some(session) = slot.as_ref() {
        return Ok(Arc::clone(session));
    }
    let mut session = crate::modules::ProgramModuleSession::preload_program(program, registry)?;
    session.fingerprint_body_calls(cwd, program_module_body_call_fingerprint)?;
    let session = Arc::new(session);
    preflight_module_session(&session, registry, cwd)?;
    *slot = Some(Arc::clone(&session));
    Ok(session)
}

fn preflight_module_session(
    session: &crate::modules::ProgramModuleSession,
    registry: &Registry,
    cwd: &Path,
) -> Result<()> {
    for (scope, invocation) in session.scoped_body_invocations() {
        crate::capabilities::preflight_invocations(std::iter::once(invocation), scope, cwd)
            .with_context(|| {
                format!(
                    "preflighting program module body call {:?}",
                    invocation.operation
                )
            })?;
    }
    // Keep the argument in this helper to make accidental cross-registry
    // preflight visible while imported scopes are being assembled.
    let _ = registry;
    Ok(())
}

fn current_module_session(signals: &Signals) -> Result<Arc<crate::modules::ProgramModuleSession>> {
    signals
        .program_modules
        .lock()
        .map_err(|_| anyhow!("program module session lock is poisoned"))?
        .clone()
        .ok_or_else(|| anyhow!("program module source was not preloaded before execution"))
}
impl Drop for Signals {
    fn drop(&mut self) {
        for registration in &self.registrations {
            signal_hook::low_level::unregister(*registration);
        }
    }
}
pub(crate) fn binary_path(binary: &str, cwd: &Path) -> Result<PathBuf> {
    if binary == "@execute-model" {
        return bundled_model_applet();
    }
    if binary == "@execute-applet" {
        return bundled_applet_helper();
    }
    // Keep the selected path spelling: multicall tools and symlink aliases
    // dispatch on argv[0], so canonicalizing to the target changes behavior.
    if binary.contains('/')
        || (cfg!(windows) && binary.contains('\\'))
        || Path::new(binary).is_absolute()
    {
        let candidate = crate::platform::executable_path(cwd.join(binary))?;
        let metadata = fs::metadata(&candidate)?;
        if !metadata.is_file() || !crate::platform::executable(&metadata) {
            bail!("{} is not an executable file", candidate.display());
        }
        Ok(candidate)
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        binary_path_from_path(binary, cwd, &path)
    }
}

/// Locate the version-matched generic applet helper beside this executable.
pub fn bundled_applet_helper() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locating the running interpreter")?;
    let helper = executable.with_file_name(if cfg!(windows) {
        "execute-applet.exe"
    } else {
        "execute-applet"
    });
    let metadata = fs::metadata(&helper).with_context(|| {
        "bundled execute-applet helper missing; build the interpreter and helper binaries with cargo build --release"
    })?;
    if !metadata.is_file() || !crate::platform::executable(&metadata) {
        bail!("bundled execute-applet helper is not an executable file");
    }
    Ok(helper)
}

/// Locate the version-matched native model applet beside this executable.
pub fn bundled_model_applet() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locating the running interpreter")?;
    let helper = executable.with_file_name(if cfg!(windows) {
        "execute-model.exe"
    } else {
        "execute-model"
    });
    let metadata = fs::metadata(&helper).with_context(|| {
        "bundled execute-model helper missing; build both binaries with cargo build --release"
    })?;
    if !metadata.is_file() || !crate::platform::executable(&metadata) {
        bail!("bundled execute-model helper is not an executable file");
    }
    Ok(helper)
}

fn binary_path_from_path(binary: &str, cwd: &Path, path: &std::ffi::OsStr) -> Result<PathBuf> {
    let mut candidate = PathBuf::new();
    for entry in std::env::split_paths(path) {
        candidate.clear();
        if entry.is_absolute() {
            // Path::join replaces its left-hand side for an absolute entry.
            // Pushing it directly avoids copying cwd and keeps the exact spelling.
            candidate.push(entry);
        } else {
            candidate.push(cwd);
            candidate.push(entry);
        }
        candidate.push(binary);
        candidate = crate::platform::executable_path(candidate)?;

        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file() && crate::platform::executable(&metadata) {
            return Ok(candidate);
        }
    }
    Err(anyhow!("executable {binary:?} not found in PATH"))
}
fn event(enabled: bool, value: serde_json::Value, signals: &Signals) -> Result<()> {
    if enabled {
        stdio::write_with_deadline(
            2,
            format!("{value}\n").as_bytes(),
            &signals.cancelled,
            signals.deadline,
        )?;
    }
    Ok(())
}
fn lazy_event(
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

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn scan_child_statuses(
    children: &mut Children,
    calls: &[(&Invocation, &Manifest, usize)],
    observed_stages: &mut [Option<execution_result::StageResult>],
    trace: bool,
    signals: &Signals,
    observer: &mut Option<ChildObserver>,
    observer_ready: &mut bool,
    started: Instant,
) -> Result<()> {
    let drain_failed = *observer_ready
        && observer
            .as_ref()
            .is_some_and(|current| current.drain().is_err());
    *observer_ready = false;
    if drain_failed {
        // The readiness source is optional. On failure, use the periodic poll
        // fallback and preserve try_wait as the only child-status authority.
        *observer = None;
    }
    for (index, managed) in children.0.iter_mut().enumerate() {
        drain_diagnostic(managed)?;
        if !managed.reaped {
            if let Some(status) = managed.child.try_wait()? {
                // An applet must stay alive until its work is complete.
                // Reclaim descendants that outlive their owning applet.
                kill_group(&managed.child);
                managed.reaped = true;
                let code = status.code();
                let stderr = captured_stage_stderr(calls[index].1, signals)
                    .then(|| managed.diagnostic.clone());
                observed_stages[index] = Some(if let Some(code) = code {
                    execution_result::StageResult::exited(
                        calls[index].2,
                        calls[index].1.name_for(calls[index].0),
                        code,
                        calls[index].1.outcome_for(code),
                        stderr,
                    )
                } else {
                    execution_result::StageResult::signaled(
                        calls[index].2,
                        calls[index].1.name_for(calls[index].0),
                        execution_result::exit_signal(status),
                        stderr,
                    )
                });
                lazy_event(
                    trace,
                    signals,
                    || serde_json::json!({"event":"exit","line":calls[index].2,"applet":calls[index].1.name_for(calls[index].0),"status":status.code(),"group_elapsed_us":started.elapsed().as_micros()}),
                )?;
                let implicit_success = code.is_some_and(|code| {
                    calls[index].1.outcome_for(code).kind == crate::contracts::OutcomeKind::Success
                });
                if !implicit_success && !(calls[index].0.status_binding.is_some() && code.is_some())
                {
                    drain_diagnostic(managed)?;
                    return Err(CommandFailed {
                        line: calls[index].2,
                        operation: calls[index].1.name_for(calls[index].0).to_owned(),
                        status,
                        outcome: code.map(|code| calls[index].1.outcome_for(code)),
                        diagnostic: String::from_utf8_lossy(&managed.diagnostic)
                            .trim()
                            .to_owned(),
                        stderr: captured_stage_stderr(calls[index].1, signals)
                            .then(|| managed.diagnostic.clone()),
                        stdout: None,
                        stages: collect_native_stages(calls, observed_stages, children, signals),
                    }
                    .into());
                }
            }
        }
    }
    if children.0.iter().all(|child| child.reaped) {
        observer.take();
    }
    Ok(())
}

/// A native command exited unsuccessfully. Timeouts, spawn/I/O errors and
/// cancellation use separate errors and are never converted into exit codes.
#[derive(Debug)]
pub struct CommandFailed {
    pub line: usize,
    pub operation: String,
    pub status: std::process::ExitStatus,
    pub outcome: Option<crate::contracts::Outcome>,
    pub diagnostic: String,
    /// Raw captured stderr bytes from the failed stage, when stderr capture
    /// was enabled. The text diagnostic remains for legacy CLI formatting.
    pub stderr: Option<Vec<u8>>,
    /// Captured terminal stdout, when available at the point the failure was
    /// observed. Native fail-fast behavior can make this unavailable.
    pub stdout: Option<Vec<u8>>,
    pub stages: Vec<execution_result::StageResult>,
}
impl std::fmt::Display for CommandFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "line {}: applet {:?} failed ({}): {}",
            self.line, self.operation, self.status, self.diagnostic
        )
    }
}
impl std::error::Error for CommandFailed {}

struct GroupOutput {
    bytes: Vec<u8>,
    status: Option<i32>,
    diagnostic: Vec<u8>,
    stdout_captured: bool,
    stages: Vec<execution_result::StageResult>,
}

fn captured_stage_stderr(manifest: &Manifest, signals: &Signals) -> bool {
    invocation_io::ResolvedStdio::resolve(
        manifest.stdio(),
        invocation_io::StdioDisposition::Capture,
        if signals.capture_diagnostics {
            invocation_io::StdioDisposition::Capture
        } else {
            invocation_io::StdioDisposition::Inherit
        },
    )
    .captures_stderr()
}

fn collect_native_stages(
    calls: &[(&Invocation, &Manifest, usize)],
    observed: &[Option<execution_result::StageResult>],
    children: &Children,
    signals: &Signals,
) -> Vec<execution_result::StageResult> {
    calls
        .iter()
        .enumerate()
        .map(|(index, (call, manifest, line))| {
            let mut stage = observed
                .get(index)
                .and_then(Clone::clone)
                .unwrap_or_else(|| {
                    if index < children.0.len() {
                        execution_result::StageResult::unknown(*line, manifest.name_for(call))
                    } else {
                        execution_result::StageResult::not_started(*line, manifest.name_for(call))
                    }
                });
            if captured_stage_stderr(manifest, signals) {
                stage.stderr = children.0.get(index).map(|child| child.diagnostic.clone());
            }
            stage
        })
        .collect()
}

fn native_execution_failure(
    kind: execution_result::ExecutionKind,
    message: impl Into<String>,
    calls: &[(&Invocation, &Manifest, usize)],
    children: &Children,
    observed: &[Option<execution_result::StageResult>],
    signals: &Signals,
) -> execution_result::ExecutionFailure {
    let stages = collect_native_stages(calls, observed, children, signals);
    let mut result = execution_result::ExecutionResult::runtime_failure(
        kind,
        message,
        if children.0.is_empty() {
            execution_result::EffectCertainty::NotStarted
        } else {
            execution_result::EffectCertainty::Uncertain
        },
    );
    result.stderr = stages.iter().rev().find_map(|stage| stage.stderr.clone());
    result.stages = stages;
    execution_result::ExecutionFailure::new(result)
}

fn native_output_limit_failure(
    message: impl Into<String>,
    output: Vec<u8>,
    calls: &[(&Invocation, &Manifest, usize)],
    children: &Children,
    observed: &[Option<execution_result::StageResult>],
    signals: &Signals,
) -> execution_result::ExecutionFailure {
    let mut result = native_execution_failure(
        execution_result::ExecutionKind::OutputLimit,
        message,
        calls,
        children,
        observed,
        signals,
    )
    .result;
    result.stdout = Some(output);
    execution_result::ExecutionFailure::new(result)
}

fn check_native_abort(
    calls: &[(&Invocation, &Manifest, usize)],
    children: &Children,
    observed: &[Option<execution_result::StageResult>],
    signals: &Signals,
) -> Result<()> {
    if signals.cancelled.load(Ordering::Relaxed) {
        return Err(native_execution_failure(
            execution_result::ExecutionKind::Cancelled,
            "workflow cancelled by signal",
            calls,
            children,
            observed,
            signals,
        )
        .into());
    }
    if signals
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Err(native_execution_failure(
            execution_result::ExecutionKind::Timeout,
            "command pipeline timed out; external effects may be incomplete",
            calls,
            children,
            observed,
            signals,
        )
        .into());
    }
    Ok(())
}

fn native_spawn_failure(
    message: String,
    index: usize,
    calls: &[(&Invocation, &Manifest, usize)],
    children: &Children,
    observed: &mut [Option<execution_result::StageResult>],
    signals: &Signals,
) -> anyhow::Error {
    let (call, manifest, line) = calls[index];
    observed[index] = Some(execution_result::StageResult::spawn_failed(
        line,
        manifest.name_for(call),
    ));
    native_execution_failure(
        execution_result::ExecutionKind::SpawnError,
        message,
        calls,
        children,
        observed,
        signals,
    )
    .into()
}

#[allow(clippy::too_many_arguments)]
fn execute_group(
    registry: &Registry,
    calls: &[(&Invocation, &Manifest, usize)],
    input: Input,
    prepared_first_call: Option<(PathBuf, Vec<String>)>,
    bindings: &BTreeMap<String, Value>,
    cwd: &Path,
    options: &RunOptions,
    launched: &mut usize,
    signals: &Signals,
) -> Result<GroupOutput> {
    redact_command_failure(
        registry,
        execute_group_with_sink(
            registry,
            calls,
            input,
            prepared_first_call,
            bindings,
            cwd,
            options,
            launched,
            signals,
            None,
        ),
    )
}

fn redact_command_failure<T>(registry: &Registry, mut result: Result<T>) -> Result<T> {
    if let Err(error) = &mut result {
        if let Some(failure) = error.downcast_mut::<CommandFailed>() {
            failure.diagnostic =
                crate::security::secret_redactor(registry).redact_text(&failure.diagnostic);
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
#[cfg(unix)]
fn execute_group_with_sink(
    registry: &Registry,
    calls: &[(&Invocation, &Manifest, usize)],
    input: Input,
    prepared_first_call: Option<(PathBuf, Vec<String>)>,
    bindings: &BTreeMap<String, Value>,
    cwd: &Path,
    options: &RunOptions,
    launched: &mut usize,
    signals: &Signals,
    mut sink: Option<&mut streaming::LineSink<'_>>,
) -> Result<GroupOutput> {
    let mut writer = InputWriter::default();
    let mut children = Children(Vec::with_capacity(calls.len()));
    let mut observed_stages = vec![None; calls.len()];
    let mut input = Some(input);
    let mut prepared_first_call = prepared_first_call;
    let mut previous = None;
    let started = Instant::now();
    let mut child_observer = ChildObserver::new();
    let mut observer_ready = false;
    let bounded_signals = Signals {
        cancelled: signals.cancelled.clone(),
        registrations: Vec::new(),
        capture_diagnostics: signals.capture_diagnostics,
        program_modules: Arc::clone(&signals.program_modules),
        sensitivity_floor: Arc::clone(&signals.sensitivity_floor),
        deadline: {
            let command_deadline = options
                .command_timeout
                .map(|duration| {
                    started
                        .checked_add(duration)
                        .context("command timeout is too large")
                })
                .transpose()?;
            match (signals.deadline, command_deadline) {
                (Some(total), Some(command)) => Some(total.min(command)),
                (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
                (None, None) => None,
            }
        },
    };
    let signals = &bounded_signals;
    if calls.is_empty() {
        bail!("cannot execute an empty native command group");
    }
    let sensitivities = signals.sensitivity_floor();
    for (_, manifest, _) in calls {
        crate::security::authorize_declared_filesystem_write(registry, manifest, &sensitivities)?;
    }
    for (index, (_, manifest, line)) in calls.iter().enumerate() {
        if index + 1 != calls.len()
            && manifest.stdio().is_some_and(|stdio| {
                stdio
                    .stdout
                    .is_some_and(|mode| mode != invocation_io::StdioDisposition::Capture)
            })
        {
            bail!("line {line}: only the final pipeline applet can discard or inherit stdout");
        }
    }
    let terminal_io = invocation_io::ResolvedStdio::resolve(
        calls.last().expect("nonempty group").1.stdio(),
        invocation_io::StdioDisposition::Capture,
        if signals.capture_diagnostics {
            invocation_io::StdioDisposition::Capture
        } else {
            invocation_io::StdioDisposition::Inherit
        },
    );
    if sink.is_some() && !terminal_io.captures_terminal_stdout() {
        bail!("streaming requires the final applet's stdout to be captured");
    }
    let (terminal_call, terminal_manifest, terminal_line) = calls.last().expect("nonempty group");
    if !terminal_io.captures_terminal_stdout()
        && (terminal_call.binding.is_some()
            || calls.iter().any(|(call, _, _)| call.cache)
            || (terminal_manifest.output_kind().is_some()
                && terminal_call.result_binding.is_none()))
    {
        bail!(
            "line {terminal_line}: discarded or inherited stdout cannot be bound, cached, or decoded as a typed output"
        );
    }
    for (index, (call, manifest, line)) in calls.iter().enumerate() {
        check_native_abort(calls, &children, &observed_stages, signals)?;
        let (binary, argv) = match prepared_first_call.take() {
            Some((binary, argv)) => (binary, GroupArguments::Owned(argv)),
            None => {
                let argv = arguments_for_group_call(call, manifest, bindings)?;
                let binary = binary_path(manifest.binary_for(call), cwd).map_err(|error| {
                    native_spawn_failure(
                        format!(
                            "line {line}: resolving executable for {}: {error:#}",
                            manifest.name_for(call)
                        ),
                        index,
                        calls,
                        &children,
                        &mut observed_stages,
                        signals,
                    )
                })?;
                (binary, argv)
            }
        };
        let mut command = match &argv {
            GroupArguments::Owned(argv) => crate::capabilities::command_for(
                &binary,
                argv.iter().map(|argument| OsStr::new(argument.as_str())),
                registry,
                &manifest.name,
                cwd,
            )?,
            GroupArguments::Borrowed(argv) => crate::capabilities::command_for(
                &binary,
                argv.iter().map(|argument| OsStr::new(argument.as_ref())),
                registry,
                &manifest.name,
                cwd,
            )?,
        };
        let resolved_io = invocation_io::ResolvedStdio::resolve(
            manifest.stdio(),
            invocation_io::StdioDisposition::Capture,
            if signals.capture_diagnostics {
                invocation_io::StdioDisposition::Capture
            } else {
                invocation_io::StdioDisposition::Inherit
            },
        );
        let stdout_disposition = resolved_io.stdout_for_stage(index + 1 == calls.len());
        command
            .current_dir(cwd)
            .stdout(invocation_io::process_stdio(stdout_disposition))
            .stderr(invocation_io::process_stdio(resolved_io.stderr))
            .process_group(0);
        let mut bytes = None;
        let mut prefilled_input = None;
        let stdin = if index != 0 {
            Stdio::from(previous.take().expect("prior child stdout"))
        } else {
            match input.take().expect("first input") {
                Input::File(file) => Stdio::from(file),
                Input::Stdin => Stdio::inherit(),
                Input::Bytes(data) if !data.is_empty() && data.len() <= libc::PIPE_BUF => {
                    let (reader, mut pipe_writer) =
                        std::io::pipe().context("creating applet stdin pipe")?;
                    pipe_writer
                        .write_all(&data)
                        .context("prefilling applet stdin pipe")?;
                    // Close before spawn so the child observes EOF after the
                    // buffered input without a background writer thread.
                    drop(pipe_writer);
                    Stdio::from(reader)
                }
                Input::Bytes(data)
                    if !data.is_empty() && data.len() <= SMALL_STDIN_PREFILL_LIMIT =>
                {
                    let (reader, remainder) = prefill_stdin_pipe(data)?;
                    prefilled_input = remainder;
                    Stdio::from(reader)
                }
                Input::Bytes(data) if !data.is_empty() => {
                    bytes = Some(data);
                    Stdio::piped()
                }
                _ => Stdio::null(),
            }
        };
        command.stdin(stdin);
        check_native_abort(calls, &children, &observed_stages, signals)?;
        let mut child = command.spawn().map_err(|error| {
            native_spawn_failure(
                format!("line {line}: spawning {}: {error}", binary.display()),
                index,
                calls,
                &children,
                &mut observed_stages,
                signals,
            )
        })?;
        let observer_failed = child_observer
            .as_ref()
            .and_then(|observer| observer.observe(child.id()).err())
            .is_some_and(|error| error.raw_os_error() != Some(libc::ESRCH));
        if observer_failed {
            child_observer = None;
        }
        *launched += 1;
        let child_stdin = bytes
            .as_ref()
            .map(|_| child.stdin.take().expect("piped child stdin"));
        previous = child.stdout.take();
        let stderr = child.stderr.take();
        children.0.push(ManagedChild {
            child,
            reaped: false,
            stderr,
            diagnostic: Vec::new(),
        });
        if let Some((pipe_writer, data, offset)) = prefilled_input {
            writer.spawn(
                pipe_writer,
                data,
                offset,
                signals.cancelled.clone(),
                signals.deadline,
            );
        } else if let Some(data) = bytes {
            writer.spawn(
                child_stdin.expect("piped child stdin"),
                data,
                0,
                signals.cancelled.clone(),
                signals.deadline,
            );
        }
        if let Some(stderr) = &children.0.last().unwrap().stderr {
            let fd = stderr.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(std::io::Error::last_os_error())
                    .context("setting diagnostics nonblocking");
            }
        }
        // Parameter values are deliberately omitted from traces (may be secrets).
        lazy_event(
            options.trace,
            signals,
            || serde_json::json!({"event":"spawn", "line":line,"applet":manifest.name_for(call),"binary":binary,"pid":children.0.last().unwrap().child.id()}),
        )?;
    }
    let mut stdout = previous;
    // Only a captured terminal pipe is monitored. Intermediate bytes travel
    // directly between child descriptors; the interpreter never copies them.
    let (fd, mut eof) = if let Some(stdout) = stdout.as_ref() {
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(std::io::Error::last_os_error())
                .context("setting terminal pipe nonblocking");
        }
        (fd, false)
    } else {
        (-1, true)
    };
    let capture_diagnostics = children.0.iter().any(|child| child.stderr.is_some());
    let mut diagnostic_poll_descriptors = Vec::with_capacity(if capture_diagnostics {
        children.0.len() + 2
    } else {
        0
    });
    if capture_diagnostics {
        for _ in 0..children.0.len() + 2 {
            diagnostic_poll_descriptors.push(libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            });
        }
    }
    let mut output = Vec::new();
    let mut chunk = [0u8; 32768];
    loop {
        check_native_abort(calls, &children, &observed_stages, signals)?;
        scan_child_statuses(
            &mut children,
            calls,
            &mut observed_stages,
            options.trace,
            signals,
            &mut child_observer,
            &mut observer_ready,
            started,
        )?;
        let mut reached_eof = false;
        if !eof {
            loop {
                match stdout
                    .as_mut()
                    .expect("captured stdout pipe")
                    .read(&mut chunk)
                {
                    Ok(0) => {
                        eof = true;
                        reached_eof = true;
                        if let Some(sink) = sink.as_deref_mut() {
                            sink.finish(signals)?;
                        }
                        break;
                    }
                    Ok(count) => {
                        if let Some(sink) = sink.as_deref_mut() {
                            if sink.push(&chunk[..count], signals)? {
                                // Account for failures that became observable while
                                // reading or emitting the final requested record.
                                scan_child_statuses(
                                    &mut children,
                                    calls,
                                    &mut observed_stages,
                                    options.trace,
                                    signals,
                                    &mut child_observer,
                                    &mut observer_ready,
                                    started,
                                )?;
                                // The script's output limit is deliberate successful
                                // termination. Children owns kill/reap on this return;
                                // killed upstream stages do not become native failures.
                                return Ok(GroupOutput {
                                    bytes: output,
                                    status: None,
                                    diagnostic: children
                                        .0
                                        .last()
                                        .map_or_else(Vec::new, |child| child.diagnostic.clone()),
                                    stdout_captured: false,
                                    stages: collect_native_stages(
                                        calls,
                                        &observed_stages,
                                        &children,
                                        signals,
                                    ),
                                });
                            }
                        } else {
                            let remaining = options.max_output_bytes.saturating_sub(output.len());
                            if count > remaining {
                                output.extend_from_slice(&chunk[..remaining]);
                                return Err(native_output_limit_failure(
                                    format!(
                                        "output exceeds maximum of {} bytes",
                                        options.max_output_bytes
                                    ),
                                    output,
                                    calls,
                                    &children,
                                    &observed_stages,
                                    signals,
                                )
                                .into());
                            }
                            output.extend_from_slice(&chunk[..count]);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error).context("reading terminal pipe"),
                }
                check_native_abort(calls, &children, &observed_stages, signals)?;
            }
        }
        if reached_eof && children.0.iter().any(|child| !child.reaped) {
            // stdout EOF can precede child exit. Recheck once before falling
            // back to the short no-descriptor poll for cancellation/deadline.
            check_native_abort(calls, &children, &observed_stages, signals)?;
            scan_child_statuses(
                &mut children,
                calls,
                &mut observed_stages,
                options.trace,
                signals,
                &mut child_observer,
                &mut observer_ready,
                started,
            )?;
        }
        if eof && children.0.iter().all(|child| child.reaped) {
            break;
        }
        // Keep a short timeout for external cancellation and observer fallback.
        // Active kqueue observation makes a 20 ms EOF fallback sufficient;
        // unavailable observers retain the previous 1 ms EOF rescan.
        let maximum_wait_ms = if eof && child_observer.is_none() {
            1
        } else {
            20
        };
        let timeout_ms = deadline_poll_timeout(signals.deadline, maximum_wait_ms);
        let observer_notified;
        let ready = if capture_diagnostics {
            let descriptors = &mut diagnostic_poll_descriptors;
            descriptors[0].fd = if eof { -1 } else { fd };
            descriptors[0].events = libc::POLLIN;
            descriptors[0].revents = 0;
            descriptors[1].fd = child_observer.as_ref().map_or(-1, ChildObserver::raw_fd);
            descriptors[1].events = libc::POLLIN;
            descriptors[1].revents = 0;
            for (descriptor, child) in descriptors[2..].iter_mut().zip(&children.0) {
                descriptor.fd = child
                    .stderr
                    .as_ref()
                    .map_or(-1, |stderr| stderr.as_raw_fd());
                descriptor.events = libc::POLLIN;
                descriptor.revents = 0;
            }
            let ready =
                unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, timeout_ms) };
            observer_notified = ready > 0 && descriptors[1].revents != 0;
            ready
        } else {
            let mut descriptors = [
                libc::pollfd {
                    fd: if eof { -1 } else { fd },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: child_observer.as_ref().map_or(-1, ChildObserver::raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout_ms) };
            observer_notified = ready > 0 && descriptors[1].revents != 0;
            ready
        };
        observer_ready |= observer_notified;
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("polling terminal pipe");
            }
        }
    }

    writer
        .join()
        .with_context(|| format!("line {}: feeding applet stdin", calls[0].2))?;
    let status = observed_stages
        .last()
        .and_then(Option::as_ref)
        .and_then(|stage| stage.status);
    Ok(GroupOutput {
        bytes: output,
        status,
        diagnostic: children
            .0
            .last()
            .map_or_else(Vec::new, |child| child.diagnostic.clone()),
        stdout_captured: terminal_io.captures_terminal_stdout() && sink.is_none(),
        stages: collect_native_stages(calls, &observed_stages, &children, signals),
    })
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
#[cfg(all(test, unix))]
fn cache_key(
    manifest: &Manifest,
    argv: &[String],
    executable: &Path,
    cwd: &Path,
    input: &[u8],
) -> Result<String> {
    cache_key_with_environment(manifest, argv, executable, cwd, input, std::env::vars_os)
}

fn cache_key_for_applet(
    registry: &Registry,
    manifest: &Manifest,
    argv: &[String],
    executable: &Path,
    cwd: &Path,
    input: &[u8],
) -> Result<String> {
    let environment = crate::capabilities::effective_environment(registry, &manifest.name)?;
    cache_key_with_environment(manifest, argv, executable, cwd, input, || environment)
}

fn cache_key_for_program_module(
    registry: &Registry,
    manifest: &Manifest,
    argv: &[String],
    resolved_parameters: &[u8],
    cwd: &Path,
    input: &[u8],
    module_fingerprint: &str,
) -> Result<String> {
    let environment = crate::capabilities::effective_environment(registry, &manifest.name)?;
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"flow-cache-program-module-v1");
    let mut serialized = Vec::with_capacity(128);
    serde_json::to_writer(&mut serialized, manifest)?;
    hash_field(&mut hash, &serialized);
    serialized.clear();
    serde_json::to_writer(&mut serialized, argv)?;
    hash_field(&mut hash, &serialized);
    hash_field(&mut hash, resolved_parameters);
    hash_field(&mut hash, cwd.as_os_str().as_encoded_bytes());
    hash_field(&mut hash, module_fingerprint.as_bytes());
    hash_environment(&mut hash, environment);
    hash_field(&mut hash, input);
    Ok(format!("{:x}", hash.finalize()))
}

fn program_module_cache_is_closed(session: &crate::modules::ProgramModuleSession) -> Result<bool> {
    for (scope, invocation) in session.scoped_body_invocations() {
        if scope.resolve_call(invocation)?.execution_backend() != AppletBackend::ProgramModule {
            // A module body that calls an ordinary applet depends on that
            // applet's manifest and executable identity as well as its own
            // source. Until those identities are part of the module cache
            // key, run it uncached rather than risk stale results.
            return Ok(false);
        }
    }
    Ok(true)
}

fn key_with_dependency_fingerprint(key: &str, fingerprint: [u8; 32]) -> String {
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"flow-cache-dependency-key-v1");
    hash_field(&mut hash, key.as_bytes());
    hash_field(&mut hash, &fingerprint);
    format!("{:x}", hash.finalize())
}

fn dependency_fingerprint_matches(manifest: &Manifest, cwd: &Path, expected: [u8; 32]) -> bool {
    cache::dependency_fingerprint(manifest, cwd).ok().flatten() == Some(expected)
}

fn cache_key_with_environment<F, I>(
    manifest: &Manifest,
    argv: &[String],
    executable: &Path,
    cwd: &Path,
    input: &[u8],
    environment_source: F,
) -> Result<String>
where
    F: FnOnce() -> I,
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"flow-cache-v1");
    let mut serialized = Vec::with_capacity(128);
    serde_json::to_writer(&mut serialized, manifest)?;
    hash_field(&mut hash, &serialized);
    serialized.clear();
    serde_json::to_writer(&mut serialized, argv)?;
    hash_field(&mut hash, &serialized);
    hash_field(&mut hash, cwd.as_os_str().as_encoded_bytes());
    hash_field(&mut hash, executable.as_os_str().as_encoded_bytes());
    let mut binary_hash = Sha256::new();
    let mut file = File::open(executable)?;
    let mut chunk = [0u8; 32768];
    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        binary_hash.update(&chunk[..count]);
    }
    hash_field(&mut hash, &binary_hash.finalize());
    hash_environment(&mut hash, environment_source());
    hash_field(&mut hash, input);
    Ok(format!("{:x}", hash.finalize()))
}

fn hash_environment<I>(hash: &mut Sha256, environment: I)
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    // Stable sorting preserves original order among duplicate names. Skipping
    // all but the last therefore matches BTreeMap collection's last-value rule.
    let mut environment: Vec<_> = environment.into_iter().collect();
    environment.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (index, (name, value)) in environment.iter().enumerate() {
        if environment
            .get(index + 1)
            .is_some_and(|(next_name, _)| next_name == name)
        {
            continue;
        }
        hash_field(hash, name.as_os_str().as_encoded_bytes());
        hash_field(hash, value.as_os_str().as_encoded_bytes());
    }
}

/// Fingerprint one ordinary applet call made by a preloaded program-module
/// body. Module imports are already covered by the package graph; remote
/// calls without a pinned descriptor return `None` because their identity is
/// not stable enough for checkpoint reuse.
pub(crate) fn program_module_body_call_fingerprint(
    registry: &Registry,
    invocation: &Invocation,
    cwd: &Path,
) -> Result<Option<String>> {
    let Some(call_identity) = resume::module_body_call_identity(invocation, registry, cwd)? else {
        return Ok(None);
    };
    let manifest = registry.resolve_call(invocation)?;
    let environment = crate::capabilities::effective_environment(registry, &manifest.name)?;
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"execute-module-body-call-environment-v1");
    hash_field(&mut hash, call_identity.as_bytes());
    hash_environment(&mut hash, environment);
    Ok(Some(format!("{:x}", hash.finalize())))
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(
        ".flow-{}-{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = crate::platform::private_options()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("creating temporary output for {}", path.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        crate::platform::replace_file(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

pub fn run(program: &Program, registry: &Registry, options: &RunOptions) -> Result<RunResult> {
    let deadline = deadline_after(options.total_timeout, "total timeout")?;
    let signals = Signals::register_with_deadline(deadline)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    validate(program, registry)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    if program.trigger.is_some() {
        bail!("triggered programs require run_service");
    }
    let cwd = fs::canonicalize(&options.working_directory)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    let options = configured_inputs(program, registry, options)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    crate::capabilities::preflight_with_cache_directory(
        program,
        registry,
        &cwd,
        options.cache_directory.as_deref(),
    )?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    preload_program_modules(program, registry, &signals, &cwd)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    run_finite(program, registry, &options, &signals, Input::Empty, None)
}

pub(super) fn authorize_checkpoint_persistence(
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

const VERSIONED_CACHE_MAGIC: &[u8] = b"FLOW-CONTRACT-CACHE\0\x01";

fn read_cached_output(
    manifest: &Manifest,
    file: File,
    limit: usize,
) -> Result<(Vec<u8>, Option<i32>)> {
    if manifest.execution_contract().is_none() {
        return Ok((capture(file, limit)?, None));
    }
    let header = VERSIONED_CACHE_MAGIC.len() + std::mem::size_of::<i32>();
    let entry = capture(file, limit.saturating_add(header))?;
    if entry.len() < header || !entry.starts_with(VERSIONED_CACHE_MAGIC) {
        bail!("cached versioned applet result has an invalid header");
    }
    let status_at = VERSIONED_CACHE_MAGIC.len();
    let status = i32::from_le_bytes(
        entry[status_at..status_at + std::mem::size_of::<i32>()]
            .try_into()
            .expect("fixed cache status width"),
    );
    if manifest.outcome_for(status).kind != OutcomeKind::Success {
        bail!("cached versioned applet result has a non-success status {status}");
    }
    let payload = entry[header..].to_vec();
    if payload.len() > limit {
        bail!("cached result exceeds maximum of {limit} bytes");
    }
    Ok((payload, Some(status)))
}

fn cache_output(manifest: &Manifest, bytes: &[u8], status: Option<i32>) -> Result<Vec<u8>> {
    let Some(_contract) = manifest.execution_contract() else {
        return Ok(bytes.to_vec());
    };
    let status = status.context("versioned applet result finished without an exit code")?;
    if manifest.outcome_for(status).kind != OutcomeKind::Success {
        bail!("refusing to cache non-success applet status {status}");
    }
    let mut entry = Vec::with_capacity(VERSIONED_CACHE_MAGIC.len() + 4 + bytes.len());
    entry.extend_from_slice(VERSIONED_CACHE_MAGIC);
    entry.extend_from_slice(&status.to_le_bytes());
    entry.extend_from_slice(bytes);
    Ok(entry)
}

fn resolved_json_arguments(
    call: &Invocation,
    manifest: &Manifest,
    bindings: &BTreeMap<String, Value>,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let values = call
        .parameters
        .iter()
        .map(|(name, value)| {
            let value = resolve(value, bindings)?;
            Ok((name.clone(), value))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    manifest.parameters_json(&values)
}

#[allow(clippy::too_many_arguments)]
fn bind_call_output(
    name: &str,
    manifest: &Manifest,
    line: usize,
    bytes: &[u8],
    status: Option<i32>,
    stdout_captured: bool,
    stderr: Option<&[u8]>,
    stages: &[execution_result::StageResult],
    retain_value: bool,
    bindings: &mut BTreeMap<String, Value>,
) -> Result<Option<Value>> {
    let value = decode_call_output(
        manifest,
        line,
        bytes,
        status,
        stdout_captured,
        stderr,
        stages,
    )?;
    if retain_value {
        bindings.insert(name.to_owned(), value.clone());
        Ok(Some(value))
    } else {
        bindings.insert(name.to_owned(), value);
        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_call_output(
    manifest: &Manifest,
    line: usize,
    bytes: &[u8],
    status: Option<i32>,
    stdout_captured: bool,
    stderr: Option<&[u8]>,
    stages: &[execution_result::StageResult],
) -> Result<Value> {
    let decode_failure = |error: anyhow::Error, outcome: Option<crate::contracts::Outcome>| {
        let error = error.context(format!("line {line}: decoding applet output"));
        execution_result::output_decode_failure(
            &error,
            outcome,
            stdout_captured.then(|| bytes.to_vec()),
            stderr.map(<[u8]>::to_vec),
            stages.to_vec(),
        )
    };
    let value = if manifest.execution_contract().is_none() {
        // Preserve the legacy named-output boundary exactly: it is always
        // strict UTF-8 text, even when a separately kept native status is
        // nonzero.
        let text = String::from_utf8(bytes.to_vec())
            .context("named results must be UTF-8 text")
            .map_err(|error| {
                decode_failure(error, status.map(|status| manifest.outcome_for(status)))
            })?;
        Value::String(text)
    } else {
        let status = status.ok_or_else(|| {
            let error = anyhow!("versioned applet finished without an exit code");
            decode_failure(error, None)
        })?;
        let decode_failure_for_status = |error: anyhow::Error| {
            let outcome = manifest.outcome_for(status);
            decode_failure(error, Some(outcome))
        };
        let decoded = manifest
            .decode_output(bytes, status)
            .map_err(decode_failure_for_status)?;
        match decoded {
            AppletOutput::Value(value) => value,
            AppletOutput::Bytes(bytes) => Value::String(
                String::from_utf8(bytes)
                    .context("named results must be UTF-8 text")
                    .map_err(decode_failure_for_status)?,
            ),
        }
    };
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
fn check_retained_status(
    call: &Invocation,
    manifest: &Manifest,
    line: usize,
    status: i32,
    diagnostic: &str,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
    registry: &Registry,
) -> Result<()> {
    let outcome = manifest.outcome_for(status);
    if outcome.kind != OutcomeKind::Success && call.status_binding.is_none() {
        let message = format!(
            "line {line}: applet {:?} produced {:?} outcome ({}): {}",
            manifest.name_for(call),
            outcome.name,
            status,
            crate::security::secret_redactor(registry).redact_text(diagnostic.trim())
        );
        let stderr =
            stderr.or_else(|| (!diagnostic.is_empty()).then(|| diagnostic.as_bytes().to_vec()));
        let effect_certainty = outcome.effect_certainty;
        let result = execution_result::ExecutionResult {
            kind: execution_result::ExecutionKind::from_outcome(outcome.kind),
            outcome: Some(outcome.name.clone()),
            status: Some(status),
            signal: None,
            stdout,
            value: None,
            stderr: stderr.clone(),
            stages: vec![execution_result::StageResult::exited(
                line,
                manifest.name_for(call),
                status,
                outcome,
                stderr,
            )],
            message: Some(message),
            effect_certainty,
        };
        return Err(execution_result::ExecutionFailure::new(result).into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_in_process(
    call: &Invocation,
    manifest: &Manifest,
    line: usize,
    input: Input,
    bindings: &BTreeMap<String, Value>,
    cwd: &Path,
    options: &RunOptions,
    signals: &Signals,
    registry: &Registry,
) -> Result<GroupOutput> {
    check_deadline(signals.deadline)?;
    if signals.cancelled.load(Ordering::Relaxed) {
        let mut failure = execution_result::ExecutionResult::runtime_failure(
            execution_result::ExecutionKind::Cancelled,
            "workflow cancelled before in_process applet execution",
            execution_result::EffectCertainty::NotStarted,
        );
        failure
            .stages
            .push(execution_result::StageResult::not_started(
                line,
                manifest.name_for(call),
            ));
        return Err(execution_result::ExecutionFailure::new(failure).into());
    }
    let call_started = Instant::now();
    let command_deadline = options
        .command_timeout
        .map(|timeout| {
            call_started
                .checked_add(timeout)
                .context("command timeout is too large")
        })
        .transpose()?;
    let deadline = match (signals.deadline, command_deadline) {
        (Some(total), Some(command)) => Some(total.min(command)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    };
    check_deadline(deadline)?;
    let input_timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    let input = input.materialize(options.max_output_bytes, signals, input_timeout)?;
    let arguments = resolved_json_arguments(call, manifest, bindings)?;
    check_deadline(deadline)?;
    let checkpoint = || {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            Err(crate::applets::ExecutionBudgetExceeded::Timeout)
        } else if signals.cancelled.load(Ordering::Relaxed) {
            Err(crate::applets::ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = crate::applets::ExecutionContext::new(&checkpoint);
    let result = crate::applets::execute_with_context(
        manifest.name_for(call),
        &arguments,
        &input,
        cwd,
        &context,
    );
    match result {
        Ok(value) => {
            let mut bytes = serde_json::to_vec(&value)?;
            bytes.push(b'\n');
            let outcome = manifest.outcome_for(0);
            let stage = execution_result::StageResult::exited(
                line,
                manifest.name_for(call),
                0,
                outcome.clone(),
                None,
            );
            if let Err(error) = check_deadline(deadline) {
                let mut failure = execution_result::ExecutionResult::from_outcome(
                    outcome,
                    Some(bytes.clone()),
                    None,
                    Some(crate::structured::from_json(value.clone())),
                    vec![stage],
                );
                failure.kind = execution_result::ExecutionKind::Timeout;
                failure.message = Some(format!("{error:#}"));
                return Err(execution_result::ExecutionFailure::new(failure).into());
            }
            if signals.cancelled.load(Ordering::Relaxed) {
                let mut failure = execution_result::ExecutionResult::from_outcome(
                    outcome,
                    Some(bytes.clone()),
                    None,
                    Some(crate::structured::from_json(value.clone())),
                    vec![stage],
                );
                failure.kind = execution_result::ExecutionKind::Cancelled;
                failure.message =
                    Some("workflow cancelled after in_process applet execution".into());
                return Err(execution_result::ExecutionFailure::new(failure).into());
            }
            if bytes.len() > options.max_output_bytes {
                return Err(execution_result::completed_output_limit_failure(
                    format!(
                        "output exceeds maximum of {} bytes",
                        options.max_output_bytes
                    ),
                    outcome,
                    line,
                    manifest.name_for(call),
                    Some(bytes[..options.max_output_bytes].to_vec()),
                    None,
                )
                .into());
            }
            Ok(GroupOutput {
                bytes,
                status: Some(0),
                diagnostic: Vec::new(),
                stdout_captured: true,
                stages: vec![execution_result::StageResult::exited(
                    line,
                    manifest.name_for(call),
                    0,
                    outcome,
                    None,
                )],
            })
        }
        Err(error) => {
            if let Some(failure) = in_process_budget_failure(&error, line, manifest.name_for(call))
            {
                return Err(execution_result::ExecutionFailure::new(failure).into());
            }
            let status = error.exit_status();
            let diagnostic = format!("execute-applet: {error}\n");
            let diagnostic_bytes = diagnostic.as_bytes().to_vec();
            check_retained_status(
                call,
                manifest,
                line,
                status,
                &diagnostic,
                None,
                Some(diagnostic_bytes),
                registry,
            )?;
            Ok(GroupOutput {
                bytes: Vec::new(),
                status: Some(status),
                diagnostic: diagnostic.as_bytes().to_vec(),
                stdout_captured: true,
                stages: vec![execution_result::StageResult::exited(
                    line,
                    manifest.name_for(call),
                    status,
                    manifest.outcome_for(status),
                    Some(diagnostic.into_bytes()),
                )],
            })
        }
    }
}

fn in_process_budget_failure(
    error: &crate::applets::AppletError,
    line: usize,
    applet: &str,
) -> Option<execution_result::ExecutionResult> {
    let kind = match error.kind {
        crate::applets::AppletErrorKind::Timeout => execution_result::ExecutionKind::Timeout,
        crate::applets::AppletErrorKind::Cancelled => execution_result::ExecutionKind::Cancelled,
        _ => return None,
    };
    let mut failure = execution_result::ExecutionResult::runtime_failure(
        kind,
        error.to_string(),
        execution_result::EffectCertainty::Uncertain,
    );
    failure
        .stages
        .push(execution_result::StageResult::unknown(line, applet));
    Some(failure)
}

#[allow(clippy::too_many_arguments)]
fn execute_json_lines_worker(
    call: &Invocation,
    manifest: &Manifest,
    line: usize,
    input: Input,
    cwd: &Path,
    options: &RunOptions,
    signals: &Signals,
    registry: &Registry,
    argv: Vec<String>,
    launched: &mut usize,
) -> Result<GroupOutput> {
    let input = input.materialize(options.max_output_bytes, signals, options.command_timeout)?;
    let launch = worker_launch_for_call(call, manifest, registry, cwd)?;
    let command_deadline = options
        .command_timeout
        .map(|duration| {
            Instant::now()
                .checked_add(duration)
                .context("command timeout is too large")
        })
        .transpose()?;
    let deadline = match (command_deadline, signals.deadline) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    };
    let spawned = Arc::new(AtomicBool::new(false));
    let request = worker_backend::WorkerRequest {
        arguments: argv,
        input,
        max_output_bytes: options.max_output_bytes,
        sensitivity: signals.sensitivity_floor(),
        deadline,
        cancelled: signals.cancelled.clone(),
        spawned: Arc::clone(&spawned),
    };
    let response = registry
        .host_state::<worker_backend::WorkerPool>()
        .invoke(launch, request)
        .with_context(|| {
            format!(
                "line {line}: running json_lines_worker applet {:?}",
                manifest.name_for(call)
            )
        });
    if spawned.load(Ordering::Acquire) {
        *launched += 1;
    }
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            if let Some(worker_error) = error.downcast_ref::<worker_backend::WorkerError>() {
                let outcome = worker_error
                    .completed_status
                    .map(|status| manifest.outcome_for(status));
                let failure = execution_result::worker_failure(
                    worker_error,
                    line,
                    manifest.name_for(call),
                    outcome,
                );
                return Err(anyhow::Error::from(failure)).with_context(|| {
                    format!(
                        "line {line}: running json_lines_worker applet {:?}",
                        manifest.name_for(call)
                    )
                });
            }
            return Err(error);
        }
    };
    check_retained_status(
        call,
        manifest,
        line,
        response.status,
        &String::from_utf8_lossy(&response.stderr),
        Some(response.output.clone()),
        Some(response.stderr.clone()),
        registry,
    )?;
    Ok(GroupOutput {
        bytes: response.output,
        status: Some(response.status),
        diagnostic: response.stderr.clone(),
        stdout_captured: true,
        stages: vec![execution_result::StageResult::exited(
            line,
            manifest.name_for(call),
            response.status,
            manifest.outcome_for(response.status),
            Some(response.stderr),
        )],
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_call_group(
    calls: &[(&Invocation, &Manifest, usize)],
    input: Input,
    prepared_first_call: Option<(PathBuf, Vec<String>)>,
    bindings: &BTreeMap<String, Value>,
    cwd: &Path,
    options: &RunOptions,
    launched: &mut usize,
    signals: &Signals,
    registry: &Registry,
) -> Result<GroupOutput> {
    let Some((call, manifest, line)) = calls.first().copied() else {
        bail!("cannot execute an empty applet group");
    };
    let backend = manifest.execution_backend();
    if backend == AppletBackend::Command {
        if calls.iter().any(|(_, stage_manifest, _)| {
            stage_manifest.execution_backend() != AppletBackend::Command
        }) {
            bail!("mixed command and non-command applet groups are unsupported");
        }
        return execute_group(
            registry,
            calls,
            input,
            prepared_first_call,
            bindings,
            cwd,
            options,
            launched,
            signals,
        );
    }
    let sensitivities = signals.sensitivity_floor();
    for (_, stage_manifest, _) in calls {
        crate::security::authorize_declared_filesystem_write(
            registry,
            stage_manifest,
            &sensitivities,
        )?;
    }
    if calls.len() != 1 {
        bail!(
            "{} applet calls cannot participate in native pipe groups",
            match backend {
                AppletBackend::InProcess => "in_process",
                AppletBackend::JsonLinesWorker => "json_lines_worker",
                AppletBackend::Command => "command",
                AppletBackend::ProgramModule => "program_module",
                AppletBackend::Remote => "remote",
            }
        );
    }
    let argv = if backend == AppletBackend::JsonLinesWorker {
        match prepared_first_call {
            Some((_, argv)) => argv,
            None => arguments_for_call(call, manifest, bindings)?,
        }
    } else {
        Vec::new()
    };
    match backend {
        AppletBackend::Command => unreachable!("handled above"),
        AppletBackend::InProcess => execute_in_process(
            call, manifest, line, input, bindings, cwd, options, signals, registry,
        ),
        AppletBackend::JsonLinesWorker => execute_json_lines_worker(
            call, manifest, line, input, cwd, options, signals, registry, argv, launched,
        ),
        AppletBackend::ProgramModule => {
            let session = current_module_session(signals)?;
            let loaded = session
                .get_for_manifest(registry, manifest)
                .ok_or_else(|| {
                    anyhow!(
                        "program module applet {:?} was not preloaded before effects",
                        manifest.name
                    )
                })?;
            let child_registry = session.registry_for(registry, manifest).ok_or_else(|| {
                anyhow!(
                    "program module scope for {:?} was not preloaded before effects",
                    manifest.name
                )
            })?;
            program_backend::execute(
                loaded,
                call,
                manifest,
                input,
                bindings,
                cwd,
                options,
                signals,
                child_registry,
                launched,
                line,
            )
        }
        AppletBackend::Remote => execute_remote_applet(
            call, manifest, line, input, bindings, cwd, options, signals, registry,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_remote_applet(
    call: &Invocation,
    manifest: &Manifest,
    line: usize,
    input: Input,
    bindings: &BTreeMap<String, Value>,
    cwd: &Path,
    options: &RunOptions,
    signals: &Signals,
    registry: &Registry,
) -> Result<GroupOutput> {
    use crate::contracts::{OutputEncoding, OutputType, RemoteAuthSource};
    use crate::remote::{RemoteClient, RemoteRequest, RemoteValue};
    let contract = manifest
        .execution_contract()
        .context("remote backend requires a versioned applet contract")?;
    let endpoint = contract
        .remote
        .as_ref()
        .context("remote backend requires an endpoint configuration")?;
    crate::capabilities::authorize_network_endpoint(registry, &manifest.name, &endpoint.endpoint)?;
    crate::security::authorize_data_flow_set(
        registry,
        &manifest.name,
        &endpoint.endpoint,
        &signals.sensitivity_floor(),
    )?;

    let credential_base = manifest
        .origin()
        .and_then(Path::parent)
        .unwrap_or(cwd)
        .to_path_buf();
    match endpoint.auth.as_ref() {
        Some(RemoteAuthSource::BearerEnv { name }) => {
            crate::capabilities::authorize_environment_variable(registry, &manifest.name, name)?;
        }
        Some(RemoteAuthSource::BearerFile { .. }) => {}
        None => {}
    }

    let mut client_config = endpoint.clone();
    if let Some(deadline) = signals.deadline {
        let remaining_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1)
            .min(u64::MAX as u128) as u64;
        client_config.timeout_ms = client_config.timeout_ms.min(remaining_ms);
    }
    let client = RemoteClient::new_with_credential_base(client_config, credential_base)?
        .with_secret_redactor(crate::security::secret_redactor(registry))
        .with_resolver_helper(std::env::current_exe()?);
    if let Some(path) = client.descriptor_cache_path() {
        crate::capabilities::authorize_applet_host_path(
            registry,
            &manifest.name,
            &path,
            false,
            cwd,
        )?;
    }
    if let Some(path) = client.bearer_file_path() {
        crate::capabilities::authorize_applet_host_path(
            registry,
            &manifest.name,
            &path,
            false,
            cwd,
        )?;
    }
    let descriptor = match client.load_configured_descriptor_cache()? {
        Some(descriptor) => {
            client.use_cached_descriptor(descriptor.clone())?;
            descriptor
        }
        None if endpoint.allow_discovery => client.discover_cancellable(&signals.cancelled)?,
        None => bail!(
            "remote applet {:?} needs a pinned descriptor cache or allow_discovery:true",
            manifest.name
        ),
    };
    client.check_compatible(&manifest.name, contract)?;

    let resolved = call
        .parameters
        .iter()
        .map(|(name, value)| Ok((name.clone(), resolve(value, bindings)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let json_arguments = manifest.parameters_json(&resolved)?.into_iter().collect();
    let mut request = RemoteRequest::new(&manifest.name);
    request.arguments = json_arguments;
    let input = input.materialize(options.max_output_bytes, signals, options.command_timeout)?;
    if manifest.accepts_stdin {
        request.set_input_bytes(contract, &input)?;
    }
    let response = match client.invoke_cancellable(&descriptor, &request, &signals.cancelled) {
        Ok(response) => response,
        Err(error) => {
            let failure = execution_result::remote_failure(&error, line, manifest.name_for(call));
            return Err(anyhow::Error::from(failure).context(format!(
                "line {line}: invoking remote applet {:?}",
                manifest.name_for(call)
            )));
        }
    };
    let status = response.status;
    let outcome = manifest.outcome_for(status);
    let diagnostic = response
        .error
        .as_ref()
        .map(|error| format!("{}: {}", error.code, error.message))
        .unwrap_or_default();
    let output_schema = contract.output.as_ref();
    let output_bytes = match (response.outputs.len(), output_schema) {
        (0, _) => Vec::new(),
        (1, schema) => {
            let (_, value) = response.outputs.iter().next().expect("one output");
            match value {
                RemoteValue::Base64(_) => value
                    .decode_bytes(endpoint.max_response_bytes)?
                    .unwrap_or_default(),
                RemoteValue::Json(value)
                    if schema.is_some_and(|schema| {
                        schema.encoding == OutputEncoding::Raw
                            && matches!(schema.kind, OutputType::Text | OutputType::Path)
                    }) =>
                {
                    value
                        .as_str()
                        .ok_or_else(|| anyhow!("remote raw text output was not a JSON string"))?
                        .as_bytes()
                        .to_vec()
                }
                RemoteValue::Json(value) => serde_json::to_vec(value)?,
            }
        }
        (_, _) => {
            let mut values = serde_json::Map::new();
            for (name, value) in &response.outputs {
                let value = match value {
                    RemoteValue::Json(value) => value.clone(),
                    RemoteValue::Base64(encoded) => serde_json::Value::String(encoded.clone()),
                };
                values.insert(name.clone(), value);
            }
            serde_json::to_vec(&serde_json::Value::Object(values))?
        }
    };
    check_retained_status(
        call,
        manifest,
        line,
        status,
        &diagnostic,
        Some(output_bytes.clone()),
        Some(diagnostic.as_bytes().to_vec()),
        registry,
    )?;
    if output_bytes.len() > options.max_output_bytes {
        let captured = output_bytes[..options.max_output_bytes].to_vec();
        let failure = execution_result::completed_output_limit_failure(
            format!(
                "remote applet output exceeds maximum of {} bytes",
                options.max_output_bytes
            ),
            outcome,
            line,
            manifest.name_for(call),
            Some(captured),
            Some(diagnostic.as_bytes().to_vec()),
        );
        return Err(failure.into());
    }
    let diagnostic_bytes = diagnostic.into_bytes();
    Ok(GroupOutput {
        bytes: output_bytes,
        status: Some(status),
        diagnostic: diagnostic_bytes.clone(),
        stdout_captured: true,
        stages: vec![execution_result::StageResult::exited(
            line,
            manifest.name_for(call),
            status,
            outcome,
            Some(diagnostic_bytes),
        )],
    })
}

fn value_contains_reference(value: &Value) -> bool {
    match value {
        Value::Reference(_) => true,
        Value::List(values) => values.iter().any(value_contains_reference),
        Value::Record(values) => values.values().any(value_contains_reference),
        Value::String(_) | Value::Integer(_) | Value::Bool(_) | Value::Null | Value::Number(_) => {
            false
        }
    }
}

fn worker_launch_for_call(
    call: &Invocation,
    manifest: &Manifest,
    registry: &Registry,
    cwd: &Path,
) -> Result<worker_backend::WorkerLaunch> {
    let contract = manifest
        .execution_contract()
        .context("json_lines_worker requires a versioned applet contract")?;
    let Lifecycle::JsonLines {
        startup_arguments,
        timeout_ms,
        max_frame_bytes,
        max_stderr_bytes,
    } = &contract.lifecycle
    else {
        bail!("json_lines_worker requires a json_lines lifecycle");
    };
    worker_backend::WorkerLaunch::inherited(
        registry.clone(),
        manifest.name_for(call).to_owned(),
        binary_path(manifest.binary_for(call), cwd)?,
        cwd.to_path_buf(),
        startup_arguments.iter().map(OsString::from).collect(),
        Duration::from_millis(*timeout_ms),
        *max_frame_bytes,
        *max_stderr_bytes,
    )
}

/// Read retained worker provenance for graph/runtime adapters that maintain
/// their own event provenance. Preflight may conservatively include workers
/// whose executable content changed at the same launch path; dispatch checks
/// the exact content identity before sending a request.
pub(crate) fn worker_sensitivity_history_for_call(
    call: &Invocation,
    manifest: &Manifest,
    registry: &Registry,
    cwd: &Path,
) -> Result<crate::security::SensitivitySet> {
    if manifest.execution_backend() != AppletBackend::JsonLinesWorker {
        return Ok(crate::security::SensitivitySet::empty());
    }
    let launch = worker_launch_for_call(call, manifest, registry, cwd)?;
    registry
        .host_state::<worker_backend::WorkerPool>()
        .sensitivity_history_for_launch(&launch)
}

fn append_invocation_sensitivities(
    call: &Invocation,
    registry: &Registry,
    cwd: &Path,
    sensitivities: &mut crate::security::SensitivitySet,
) -> Result<()> {
    let manifest = registry.resolve_call(call)?;
    append_manifest_sensitivities(call, manifest, registry, cwd, sensitivities)
}

fn append_manifest_sensitivities(
    call: &Invocation,
    manifest: &Manifest,
    registry: &Registry,
    cwd: &Path,
    sensitivities: &mut crate::security::SensitivitySet,
) -> Result<()> {
    use crate::contracts::Sensitivity;

    let contract = manifest.execution_contract();
    let ports = contract.and_then(|contract| contract.ports.as_ref());
    for (name, value) in &call.parameters {
        let declared = ports
            .and_then(|ports| ports.inputs.get(name))
            .map(|port| port.sensitivity);
        if value_contains_reference(value) {
            // Referenced values are tracked separately by the workflow floor;
            // retain Unknown as a distinct contributor until that provenance
            // is available at the source boundary.
            sensitivities.insert(Sensitivity::Unknown);
            if let Some(declared) = declared {
                sensitivities.insert(declared);
            }
        } else {
            sensitivities.insert(declared.unwrap_or(Sensitivity::Public));
        }
    }
    if manifest.accepts_stdin {
        sensitivities.insert(Sensitivity::Unknown);
    }
    match ports {
        Some(ports) if !ports.outputs.is_empty() => {
            for port in ports.outputs.values() {
                sensitivities.insert(port.sensitivity);
            }
        }
        _ => sensitivities.insert(Sensitivity::Unknown),
    }
    if let Some(ports) = ports {
        for port in ports.events.values() {
            sensitivities.insert(port.sensitivity);
        }
    }
    if crate::security::applet_has_declared_secret_environment(registry, &manifest.name) {
        sensitivities.insert(Sensitivity::Secret);
    }
    append_worker_sensitivity_history(call, manifest, registry, cwd, sensitivities)
}

fn append_worker_sensitivity_history(
    call: &Invocation,
    manifest: &Manifest,
    registry: &Registry,
    cwd: &Path,
    sensitivities: &mut crate::security::SensitivitySet,
) -> Result<()> {
    if manifest.execution_backend() == AppletBackend::JsonLinesWorker {
        let worker_history = worker_sensitivity_history_for_call(call, manifest, registry, cwd)?;
        sensitivities.extend(&worker_history);
    }
    Ok(())
}

fn invocation_sensitivities(
    call: &Invocation,
    manifest: &Manifest,
    registry: &Registry,
    cwd: &Path,
) -> Result<crate::security::SensitivitySet> {
    let mut sensitivities = crate::security::SensitivitySet::empty();
    append_manifest_sensitivities(call, manifest, registry, cwd, &mut sensitivities)?;
    if sensitivities.is_empty() {
        sensitivities.insert(crate::contracts::Sensitivity::Unknown);
    }
    Ok(sensitivities)
}

/// Resolve the finite program's actual applet references before effects and
/// retain every declared source label as a run-scoped conservative floor.
pub(crate) fn workflow_sensitivity_floor(
    program: &Program,
    registry: &Registry,
    module_session: &crate::modules::ProgramModuleSession,
    cwd: &Path,
) -> Result<crate::security::SensitivitySet> {
    let mut sensitivities = crate::security::SensitivitySet::empty();
    for step in &program.steps {
        match &step.kind {
            StepKind::Invoke { call, .. } => {
                let manifest = registry.resolve_call(call)?;
                if manifest.builtin.is_none() {
                    append_manifest_sensitivities(
                        call,
                        manifest,
                        registry,
                        cwd,
                        &mut sensitivities,
                    )?;
                }
            }
            StepKind::TaskGroup { tasks, .. } => {
                for call in tasks {
                    append_invocation_sensitivities(call, registry, cwd, &mut sensitivities)?;
                }
            }
            // Reusable calls are expanded into the step sequence before the
            // runtime; scanning its actual Invoke steps avoids unused defs.
            StepKind::CallProgram { .. }
            | StepKind::Loop { .. }
            | StepKind::EndLoop { .. }
            | StepKind::If { .. }
            | StepKind::Otherwise { .. }
            | StepKind::EndIf
            | StepKind::EndCall
            | StepKind::BreakLoop
            | StepKind::ContinueLoop
            | StepKind::Return
            | StepKind::Try { .. }
            | StepKind::Catch { .. }
            | StepKind::Finally
            | StepKind::EndTry
            | StepKind::UseFolder { .. }
            | StepKind::Read { .. }
            | StepKind::ReadStdin { .. }
            | StepKind::ReadCallInput { .. }
            | StepKind::ParseJson { .. }
            | StepKind::TakeJson { .. }
            | StepKind::TakeJsonDynamic { .. }
            | StepKind::EncodeJson { .. }
            | StepKind::Save { .. } => {}
        }
    }
    if let Some(crate::ast::Trigger::Listen {
        source: crate::ast::ListenSource::Applet(call),
        ..
    }) = &program.trigger
    {
        append_invocation_sensitivities(call, registry, cwd, &mut sensitivities)?;
    }
    if matches!(
        program.trigger.as_ref(),
        Some(crate::ast::Trigger::Listen {
            source: crate::ast::ListenSource::Stdin,
            ..
        })
    ) || program.steps.iter().any(|step| {
        matches!(
            &step.kind,
            StepKind::Read { .. } | StepKind::ReadStdin { .. }
        )
    }) {
        sensitivities.insert(crate::contracts::Sensitivity::Unknown);
    }
    for (scope, call) in module_session.scoped_body_invocations() {
        append_invocation_sensitivities(call, scope, cwd, &mut sensitivities)?;
    }
    if sensitivities.is_empty() {
        sensitivities.insert(crate::contracts::Sensitivity::Unknown);
    }
    Ok(sensitivities)
}

/// Invoke one applet with already-resolved typed parameters and raw stdin.
/// This is the common dispatch hook for graph/runtime adapters; it keeps the
/// applet backend transparent while returning the exact output, status, and
/// declared outcome. Language-level commands still use their explicit binding
/// syntax to decide whether a non-success outcome may continue.
pub fn invoke_applet(
    registry: &Registry,
    name: &str,
    parameters: BTreeMap<String, Value>,
    input: Vec<u8>,
    options: &RunOptions,
    cancelled: Arc<AtomicBool>,
) -> Result<AppletInvocationResult> {
    let call = Invocation {
        operation: name.to_owned(),
        direct: false,
        parameters,
        binding: None,
        cache: false,
        status_binding: Some("__flow_internal_status".to_owned()),
        result_binding: None,
    };
    let module_session = Arc::new(crate::modules::ProgramModuleSession::preload_invocations(
        registry,
        std::iter::once(&call),
    )?);
    invoke_applet_with_module_session(
        registry,
        name,
        call.parameters.clone(),
        input,
        options,
        cancelled,
        module_session,
    )
}

/// Invoke one applet using a module snapshot prepared by the enclosing
/// program/task preflight. All sources are therefore fixed before sibling
/// tasks or earlier workflow steps can produce effects.
pub fn invoke_applet_with_module_session(
    registry: &Registry,
    name: &str,
    parameters: BTreeMap<String, Value>,
    input: Vec<u8>,
    options: &RunOptions,
    cancelled: Arc<AtomicBool>,
    module_session: Arc<crate::modules::ProgramModuleSession>,
) -> Result<AppletInvocationResult> {
    let call = Invocation {
        operation: name.to_owned(),
        direct: false,
        parameters: parameters.clone(),
        binding: None,
        cache: false,
        status_binding: Some("__flow_internal_status".to_owned()),
        result_binding: None,
    };
    let manifest = registry.resolve_call(&call)?;
    let cwd = fs::canonicalize(&options.working_directory)?;
    let sensitivities = invocation_sensitivities(&call, manifest, registry, &cwd)?;
    invoke_applet_with_module_session_and_sensitivity(
        registry,
        name,
        parameters,
        input,
        options,
        cancelled,
        module_session,
        sensitivities,
    )
}

/// Invoke one applet with an explicit effective label set supplied by the
/// enclosing finite workflow or graph, preserving producer provenance after
/// parameter references have been resolved.
#[allow(clippy::too_many_arguments)]
pub(crate) fn invoke_applet_with_module_session_and_sensitivity(
    registry: &Registry,
    name: &str,
    parameters: BTreeMap<String, Value>,
    input: Vec<u8>,
    options: &RunOptions,
    cancelled: Arc<AtomicBool>,
    module_session: Arc<crate::modules::ProgramModuleSession>,
    sensitivities: crate::security::SensitivitySet,
) -> Result<AppletInvocationResult> {
    let mut invocation_options = options.clone();
    invocation_options.working_directory = fs::canonicalize(&options.working_directory)?;
    let call = Invocation {
        operation: name.to_owned(),
        direct: false,
        parameters,
        binding: None,
        cache: false,
        status_binding: Some("__flow_internal_status".to_owned()),
        result_binding: None,
    };
    let manifest = registry.resolve_call(&call)?;
    manifest.validate(&call)?;
    let program = Program {
        definitions: Default::default(),
        inputs: Vec::new(),
        steps: vec![crate::ast::Step {
            line: 1,
            kind: StepKind::Invoke {
                call: call.clone(),
                connection: Connection::Start,
            },
        }],
        trigger: None,
    };
    crate::capabilities::preflight(&program, registry, &invocation_options.working_directory)?;
    if manifest.execution_backend() == AppletBackend::JsonLinesWorker
        && !worker_backend::WorkerPool::supported()
    {
        bail!("json_lines_worker backend is unsupported on this platform");
    }
    let signals = Signals::with_cancellation_and_deadline(
        cancelled,
        true,
        deadline_after(invocation_options.total_timeout, "total timeout")?,
    )?;
    signals.set_sensitivity_floor(sensitivities);
    preflight_module_session(
        &module_session,
        registry,
        &invocation_options.working_directory,
    )?;
    *signals
        .program_modules
        .lock()
        .map_err(|_| anyhow!("program module session lock is poisoned"))? =
        Some(Arc::clone(&module_session));
    let bindings = BTreeMap::new();
    let mut launched = 0;
    let invoked = (|| -> Result<AppletInvocationResult> {
        let completed = execute_call_group(
            &[(&call, manifest, 1)],
            Input::Bytes(input),
            None,
            &bindings,
            &invocation_options.working_directory,
            &invocation_options,
            &mut launched,
            &signals,
            registry,
        )?;
        let status = completed
            .status
            .context("applet invocation finished without an exit status")?;
        let outcome = manifest.outcome_for(status);
        let value = if manifest.execution_contract().is_some()
            && (outcome.kind == OutcomeKind::Success || manifest.result_envelope())
        {
            let decoded = manifest
                .decode_output(&completed.bytes, status)
                .map_err(|error| {
                    execution_result::contract_decode_failure(
                        &error,
                        outcome.clone(),
                        completed.stdout_captured.then(|| completed.bytes.clone()),
                        captured_stage_stderr(manifest, &signals)
                            .then(|| completed.diagnostic.clone()),
                        completed.stages.clone(),
                    )
                })?;
            match decoded {
                AppletOutput::Value(value) => Some(value),
                AppletOutput::Bytes(_) => None,
            }
        } else {
            None
        };
        Ok(AppletInvocationResult {
            output: completed.bytes,
            status,
            uncertain: outcome.effect_certainty == crate::contracts::EffectCertainty::Uncertain,
            outcome,
            value,
            stderr: completed.diagnostic,
            launched_processes: launched,
        })
    })();
    invoked.map_err(|source| {
        AppletInvocationError {
            source,
            launched_processes: launched,
        }
        .into()
    })
}

fn run_finite(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    initial_input: Input,
    initial_binding: Option<(String, Value)>,
) -> Result<RunResult> {
    run_finite_inner(
        program,
        registry,
        options,
        signals,
        initial_input,
        initial_binding,
        None,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FiniteStateSource {
    Fresh,
    Restore,
}

#[allow(clippy::too_many_arguments)]
fn run_finite_inner(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    initial_input: Input,
    initial_binding: Option<(String, Value)>,
    journal: Option<&mut checkpoint::Journal>,
) -> Result<RunResult> {
    let state_source = if journal.is_some() {
        FiniteStateSource::Restore
    } else {
        FiniteStateSource::Fresh
    };
    run_finite_inner_with_state_source(
        program,
        registry,
        options,
        signals,
        initial_input,
        initial_binding,
        journal,
        state_source,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_finite_inner_with_state_source(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    initial_input: Input,
    initial_binding: Option<(String, Value)>,
    journal: Option<&mut checkpoint::Journal>,
    state_source: FiniteStateSource,
) -> Result<RunResult> {
    run_finite_state(
        program,
        registry,
        options,
        signals,
        initial_input,
        initial_binding.into_iter().collect(),
        journal,
        state_source,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_finite_state(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    initial_input: Input,
    initial_bindings: BTreeMap<String, Value>,
    mut journal: Option<&mut checkpoint::Journal>,
    state_source: FiniteStateSource,
    state_out: Option<&mut live::NodeState>,
) -> Result<RunResult> {
    let mut cwd = fs::canonicalize(&options.working_directory)?;
    let module_session = preload_program_modules(program, registry, signals, &cwd)?;
    let mut sensitivity_floor =
        workflow_sensitivity_floor(program, registry, &module_session, &cwd)?;
    // Caller-provided initial bytes have no trusted provenance declaration.
    if !matches!(&initial_input, Input::Empty) {
        sensitivity_floor.insert(crate::contracts::Sensitivity::Unknown);
    }
    signals.set_sensitivity_floor(sensitivity_floor);
    let cache_override = options.cache_directory.as_ref().map(|path| cwd.join(path));
    // Component and managed handlers may supply values without `accept`
    // declarations. Validate their known loop inputs before earlier effects.
    let preflight_context =
        !initial_bindings.is_empty() && state_source == FiniteStateSource::Fresh;
    let mut bindings = initial_bindings;
    let mut data = initial_input;
    let mut result = RunResult {
        output: Vec::new(),
        cache_hits: 0,
        launched_processes: 0,
        executions: Vec::new(),
        dropped_executions: 0,
        dropped_stages: 0,
    };
    let mut execution_history = execution_result::ExecutionHistory::default();
    let mut index = 0;
    let mut call_frames: Vec<calls::Frame<'_>> = Vec::new();
    let mut loop_frames = loops::LoopStack::default();
    let mut control_state = control_flow::RuntimeState::default();
    if let Some(journal) = journal.as_deref_mut() {
        if state_source == FiniteStateSource::Restore {
            let state = journal.take_restored_state()?;
            index = journal.completed_nodes().last().map_or(0, |node| node.end);
            cwd = state.cwd;
            bindings = state.bindings;
            data = Input::Bytes(state.output);
            if index == program.steps.len() {
                journal.discard_removed_tail()?;
            }
        } else {
            // The service just committed this exact path with the event snapshot.
            cwd = options.working_directory.clone();
        }
        if !cwd.is_dir() {
            bail!("checkpoint working directory no longer exists");
        }
        lazy_event(
            options.trace,
            signals,
            || serde_json::json!({"event":"checkpoint_resume","restored_nodes":journal.completed_nodes().len(),"next_step":index}),
        )?;
    }
    if !program.inputs.is_empty() {
        for (name, value) in &options.inputs {
            bindings
                .entry(name.clone())
                .or_insert_with(|| value.clone());
        }
        bindings.extend(crate::inputs::resolve(&program.inputs, &bindings, true)?);
    }
    if preflight_context {
        preflight_inputs(program, registry, &bindings)?;
    }
    let mut journal_end = None;
    let mut journal_uncertain = false;
    while index < program.steps.len() {
        if signals.cancelled.load(Ordering::Relaxed) && !control_state.cleanup_active() {
            let error = call_frames.iter().rev().fold(
                anyhow!("workflow cancelled by signal"),
                |error, frame| {
                    error.context(format!(
                        "program {:?}, called at line {}",
                        frame.name, frame.line
                    ))
                },
            );
            let action = control_state.fail(error, true)?;
            match apply_control_action(
                action,
                program,
                &mut control_state,
                &mut call_frames,
                &mut loop_frames,
                &mut bindings,
                &mut cwd,
            )? {
                AppliedControl::Continue => {}
                AppliedControl::Jump(target) => {
                    index = target;
                    continue;
                }
                AppliedControl::Rethrow(error) => {
                    if let Some(journal) = journal.as_deref_mut() {
                        if !journal.persistence_failed() {
                            journal.fail(&format!("{error:#}"), true)?;
                        }
                    }
                    return Err(error);
                }
            }
        }
        let parent_signals = signals;
        let was_cleanup = control_state.cleanup_active();
        let cleanup_signals = if was_cleanup {
            let deadline = match (parent_signals.deadline, control_state.cleanup_deadline()) {
                (Some(parent), Some(cleanup)) => Some(parent.min(cleanup)),
                (Some(parent), None) | (None, Some(parent)) => Some(parent),
                (None, None) => None,
            };
            Some(Signals::with_cancellation_and_deadline(
                Arc::new(AtomicBool::new(false)),
                parent_signals.capture_diagnostics,
                deadline,
            )?)
        } else {
            None
        };
        if let Some(cleanup_signals) = cleanup_signals.as_ref() {
            cleanup_signals.set_sensitivity_floor(parent_signals.sensitivity_floor());
        }
        let signals = cleanup_signals.as_ref().unwrap_or(parent_signals);
        let step = &program.steps[index];
        if let Some(journal) = journal.as_deref_mut().filter(|_| journal_end.is_none()) {
            let end = resume::node_end(program, index);
            journal_end = Some(end);
            journal_uncertain = matches!(
                step.kind,
                StepKind::If { .. } | StepKind::CallProgram { .. } | StepKind::Loop { .. }
            ) || resume::uncertain_on_failure(program, index, end, registry)?;
            let module_session = current_module_session(signals)?;
            let fingerprint = resume::node_fingerprint_with_modules(
                program,
                index,
                end,
                registry,
                &cwd,
                module_session.fingerprints(),
            )?;
            journal.begin_node(
                &fingerprint,
                index,
                end,
                step.line,
                &cwd,
                resume::retry_safe(program, index, end, registry)?,
            )?;
        }
        let mut jump_to = None;
        let mut control_rethrow = None;
        let execution = (|| -> Result<()> {
            if control_state.cleanup_active() {
                control_state.charge_cleanup_step()?;
                check_deadline(signals.deadline)?;
            }
            match &step.kind {
                StepKind::Loop { kind, end } => {
                    control_state.enter_loop(index, index + end)?;
                    let entered = loop_frames
                        .header(index, kind, &mut bindings)
                        .with_context(|| format!("line {}", step.line))?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"loop", "line":step.line, "entered":entered}),
                    )?;
                    if !entered {
                        control_state.leave_loop(index)?;
                        jump_to = Some(index + end);
                    }
                }
                StepKind::EndLoop { back } => {
                    let header = index.checked_sub(*back).context("invalid loop back edge")?;
                    loop_frames
                        .end_iteration(header, &mut bindings)
                        .with_context(|| format!("line {}", step.line))?;
                    jump_to = Some(header);
                }
                StepKind::If {
                    condition,
                    otherwise,
                    ..
                } => {
                    let matched = control::evaluate(condition, &bindings)
                        .with_context(|| format!("line {}", step.line))?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"condition","line":step.line,"matched":matched}),
                    )?;
                    if !matched {
                        jump_to = Some(index + otherwise);
                    }
                }
                StepKind::Otherwise { end } => jump_to = Some(index + end),
                StepKind::EndIf => {}
                StepKind::CallProgram {
                    call,
                    inputs,
                    connection,
                    end,
                } => {
                    let child = calls::bind(call, inputs, &bindings).with_context(|| {
                        format!("program {:?}, called at line {}", call.operation, step.line)
                    })?;
                    control_state.enter_call(index + end);
                    call_frames.push(calls::Frame {
                        name: &call.operation,
                        line: step.line,
                        binding: call.binding.as_deref(),
                        bindings: std::mem::replace(&mut bindings, child),
                        cwd: cwd.clone(),
                    });
                    if *connection == Connection::Start {
                        data = Input::Empty;
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"program_enter", "program":call.operation, "line":step.line}),
                    )?;
                }
                StepKind::EndCall => {
                    control_state.leave_call(index + 1)?;
                    let frame = call_frames.pop().context("program return without call")?;
                    bindings = frame.bindings;
                    cwd = frame.cwd;
                    if let Some(name) = frame.binding {
                        let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                            options.max_output_bytes,
                            signals,
                            options.command_timeout,
                        )?;
                        bindings.insert(
                            name.to_owned(),
                            Value::String(
                                String::from_utf8(bytes.clone())
                                    .context("named program results must be UTF-8 text")?,
                            ),
                        );
                        data = Input::Bytes(bytes);
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"program_return", "program":frame.name, "line":frame.line}),
                    )?;
                }
                StepKind::BreakLoop => {
                    let target = control_state.current_loop()?;
                    let action = control_state.transfer(
                        control_flow::TransferKind::Break,
                        target.finish,
                        control_flow::UnwindTo::new(control_state.call_depth(), target.depth - 1),
                    )?;
                    match apply_control_action(
                        action,
                        program,
                        &mut control_state,
                        &mut call_frames,
                        &mut loop_frames,
                        &mut bindings,
                        &mut cwd,
                    )? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::ContinueLoop => {
                    let target = control_state.current_loop()?;
                    let action = control_state.transfer(
                        control_flow::TransferKind::Continue,
                        target.finish - 1,
                        control_flow::UnwindTo::new(control_state.call_depth(), target.depth),
                    )?;
                    match apply_control_action(
                        action,
                        program,
                        &mut control_state,
                        &mut call_frames,
                        &mut loop_frames,
                        &mut bindings,
                        &mut cwd,
                    )? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::Return => {
                    let target = control_state.current_call()?;
                    let action = control_state.transfer(
                        control_flow::TransferKind::Return,
                        target.end_marker,
                        control_flow::UnwindTo::new(target.call_depth, target.loop_depth),
                    )?;
                    match apply_control_action(
                        action,
                        program,
                        &mut control_state,
                        &mut call_frames,
                        &mut loop_frames,
                        &mut bindings,
                        &mut cwd,
                    )? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::Try {
                    catch,
                    finally,
                    end,
                } => {
                    let catch = catch
                        .map(|offset| {
                            let marker = index + offset;
                            let Some(crate::ast::Step {
                                kind: StepKind::Catch { binding, .. },
                                ..
                            }) = program.steps.get(marker)
                            else {
                                bail!("invalid catch marker at line {}", step.line);
                            };
                            Ok((marker, binding.clone()))
                        })
                        .transpose()?;
                    control_state.enter_try(
                        index,
                        catch,
                        finally.map(|offset| index + offset),
                        index + end,
                    )?;
                }
                StepKind::Catch { .. } => {
                    let action = control_state.catch_marker(index)?;
                    match apply_control_action(
                        action,
                        program,
                        &mut control_state,
                        &mut call_frames,
                        &mut loop_frames,
                        &mut bindings,
                        &mut cwd,
                    )? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::Finally => {
                    let action = control_state.finally_marker(index)?;
                    match apply_control_action(
                        action,
                        program,
                        &mut control_state,
                        &mut call_frames,
                        &mut loop_frames,
                        &mut bindings,
                        &mut cwd,
                    )? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::EndTry => {
                    // `Try.end` is a relative offset to the instruction after
                    // EndTry, so the runtime marker uses the same half-open
                    // boundary convention.
                    let action = control_state.end_try(index + 1)?;
                    match apply_control_action(
                        action,
                        program,
                        &mut control_state,
                        &mut call_frames,
                        &mut loop_frames,
                        &mut bindings,
                        &mut cwd,
                    )? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::UseFolder { path } => {
                    let folder = path_value(path, &bindings, &cwd)?;
                    crate::capabilities::authorize_host_path(registry, &folder, false, &cwd)?;
                    cwd = fs::canonicalize(folder)?;
                    if !cwd.is_dir() {
                        bail!("line {}: use folder requires a directory", step.line);
                    }
                }
                StepKind::Read { binding, .. }
                | StepKind::ReadStdin { binding }
                | StepKind::ReadCallInput { binding } => {
                    data = match &step.kind {
                        StepKind::Read { path, .. } => {
                            let path = path_value(path, &bindings, &cwd)?;
                            crate::capabilities::authorize_host_path(registry, &path, false, &cwd)?;
                            Input::File(File::open(&path).with_context(|| {
                                format!("line {}: reading {}", step.line, path.display())
                            })?)
                        }
                        StepKind::ReadCallInput { .. } => {
                            std::mem::replace(&mut data, Input::Empty)
                        }
                        _ => Input::Stdin,
                    };
                    if let Some(name) = binding {
                        let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                            options.max_output_bytes,
                            signals,
                            options.command_timeout,
                        )?;
                        bindings.insert(
                            name.clone(),
                            Value::String(
                                String::from_utf8(bytes.clone())
                                    .context("named results must be UTF-8 text")?,
                            ),
                        );
                        data = Input::Bytes(bytes);
                    }
                }
                StepKind::ParseJson { binding } => {
                    let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                        options.max_output_bytes,
                        signals,
                        options.command_timeout,
                    )?;
                    let value = crate::structured::parse(&bytes)
                        .with_context(|| format!("line {}", step.line))?;
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"parse_json", "line":step.line, "input_bytes":bytes.len()}),
                    )?;
                    data = Input::Bytes(bytes);
                }
                StepKind::TakeJson {
                    pointer,
                    source,
                    kind,
                    binding,
                } => {
                    let source = structured_source(source, &bindings)?;
                    let value = crate::structured::take(pointer, &source, *kind, step.line)
                        .with_context(|| format!("line {}", step.line))?;
                    let value = value.clone();
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"take_json", "line":step.line, "type":kind.to_string()}),
                    )?;
                }
                StepKind::TakeJsonDynamic {
                    pointer,
                    source,
                    kind,
                    binding,
                } => {
                    let pointer = resolve(pointer, &bindings)?;
                    let Value::String(pointer) = pointer else {
                        bail!("line {}: dynamic JSON pointer must be text", step.line);
                    };
                    let source = structured_source(source, &bindings)?;
                    let value = crate::structured::take(&pointer, &source, *kind, step.line)
                        .with_context(|| format!("line {}", step.line))?
                        .clone();
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"take_json", "line":step.line, "type":kind.to_string()}),
                    )?;
                }
                StepKind::EncodeJson { source } => {
                    let source = structured_source(source, &bindings)?;
                    let bytes = crate::structured::encode(&source, options.max_output_bytes)
                        .with_context(|| format!("line {}", step.line))?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"encode_json", "line":step.line, "output_bytes":bytes.len()}),
                    )?;
                    data = Input::Bytes(bytes);
                }
                StepKind::TaskGroup {
                    mode,
                    limit,
                    binding,
                    tasks,
                } => {
                    let group = tasks::run_group(
                        *mode,
                        *limit,
                        tasks,
                        registry,
                        &bindings,
                        options,
                        Arc::clone(&signals.cancelled),
                        current_module_session(signals)?,
                        signals.sensitivity_floor(),
                        signals.deadline,
                    );
                    let group = match group {
                        Ok(group) => group,
                        Err(error) => {
                            if let Some(failure) = error.downcast_ref::<tasks::TaskGroupFailure>() {
                                result.launched_processes += failure.launched_processes;
                            }
                            return Err(error)
                                .with_context(|| format!("line {}: task group", step.line));
                        }
                    };
                    result.launched_processes += group.launched_processes;
                    let value = group.value;
                    let bytes = crate::structured::encode(&value, options.max_output_bytes)
                        .with_context(|| format!("line {}: task group output", step.line))?;
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"task_group","line":step.line,"mode":mode,"tasks":tasks.len(),"output_bytes":bytes.len()}),
                    )?;
                    data = Input::Bytes(bytes);
                }
                StepKind::Save { path, .. } => {
                    let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                        options.max_output_bytes,
                        signals,
                        options.command_timeout,
                    )?;
                    let destination = path_value(path, &bindings, &cwd)?;
                    crate::capabilities::authorize_host_path(registry, &destination, true, &cwd)?;
                    crate::security::authorize_persistence_set(
                        registry,
                        "workflow.save",
                        crate::security::PersistenceKind::Save,
                        &signals.sensitivity_floor(),
                    )?;
                    atomic_write(&destination, &bytes)?;
                    if journal.is_some() {
                        File::open(destination.parent().unwrap_or(&cwd))?.sync_all()?;
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"save","line":step.line,"bytes":bytes.len()}),
                    )?;
                    // Preserve the pipeline value for later steps and final stdout.
                    data = Input::Bytes(bytes);
                }
                StepKind::Invoke { call, connection } => {
                    let manifest = registry.resolve_call(call)?;
                    if manifest.builtin == Some(Builtin::Cd) {
                        let parameters = call
                            .parameters
                            .iter()
                            .map(|(name, value)| Ok((name.clone(), resolve(value, &bindings)?)))
                            .collect::<Result<BTreeMap<_, _>>>()?;
                        let args = manifest.build_arguments(&parameters)?;
                        let destination = cwd.join(&args[0]);
                        crate::capabilities::authorize_host_path(
                            registry,
                            &destination,
                            false,
                            &cwd,
                        )?;
                        cwd = fs::canonicalize(destination)
                            .with_context(|| format!("line {}: cd to {:?}", step.line, args[0]))?;
                        if !cwd.is_dir() {
                            bail!("line {}: cd requires a directory", step.line);
                        }
                        data = Input::Empty;
                    } else {
                        let input = if *connection != Connection::Start && manifest.accepts_stdin {
                            std::mem::replace(&mut data, Input::Empty)
                        } else {
                            Input::Empty
                        };
                        let mut calls = vec![(call, manifest, step.line)];
                        while pipe_at(program, index + 1) {
                            index += 1;
                            let next = &program.steps[index];
                            if let StepKind::Invoke { call, .. } = &next.kind {
                                calls.push((call, registry.resolve_call(call)?, next.line));
                            }
                        }
                        let output_call = calls.last().expect("invocation group is nonempty").0;
                        let output_manifest = calls.last().expect("invocation group is nonempty").1;
                        let (bytes, status, stdout_captured, stages) = if call.cache {
                            let input = input.materialize(
                                options.max_output_bytes,
                                signals,
                                options.command_timeout,
                            )?;
                            let argv = arguments_for_call(call, manifest, &bindings)?;
                            let module_session =
                                if manifest.execution_backend() == AppletBackend::ProgramModule {
                                    Some(current_module_session(signals)?)
                                } else {
                                    None
                                };
                            let module_fingerprint = module_session
                                .as_ref()
                                .map(|session| {
                                    session
                                        .get_for_manifest(registry, manifest)
                                        .context(
                                            "program module cache key requires a preloaded module",
                                        )
                                        .map(|loaded| loaded.checkpoint_fingerprint.clone())
                                })
                                .transpose()?;
                            let resolved_module_parameters = if module_fingerprint.is_some() {
                                let parameters =
                                    resolved_json_arguments(call, manifest, &bindings)?;
                                Some(serde_json::to_vec(&parameters)?)
                            } else {
                                None
                            };
                            let module_cache_is_closed = module_session
                                .as_deref()
                                .map(program_module_cache_is_closed)
                                .transpose()?
                                .unwrap_or(true);
                            // Program modules are already loaded and pinned by
                            // the run-scoped module session; their sentinel
                            // descriptor is not an executable identity.
                            let executable = if module_fingerprint.is_some() {
                                PathBuf::new()
                            } else {
                                binary_path(manifest.binary_for(call), &cwd)?
                            };
                            let directory = cache_override
                                .clone()
                                .unwrap_or_else(|| cwd.join(".flow-cache"));
                            let sensitivity = signals.sensitivity_floor();
                            let cache_path_allowed =
                                crate::capabilities::authorize_applet_host_path(
                                    registry,
                                    &manifest.name,
                                    &directory,
                                    false,
                                    &cwd,
                                )
                                .is_ok()
                                    && crate::capabilities::authorize_applet_host_path(
                                        registry,
                                        &manifest.name,
                                        &directory,
                                        true,
                                        &cwd,
                                    )
                                    .is_ok();
                            let cache_allowed = module_cache_is_closed
                                && cache_path_allowed
                                && crate::security::authorize_cache_purity(registry, manifest)
                                    .is_ok()
                                && crate::security::authorize_persistence_set(
                                    registry,
                                    &manifest.name,
                                    crate::security::PersistenceKind::Cache,
                                    &sensitivity,
                                )
                                .is_ok();
                            let dependencies_declared = !manifest.cache_dependencies().is_empty();
                            let dependency_fingerprint = if !dependencies_declared {
                                Some([0; 32])
                            } else if cache_allowed {
                                cache::dependency_fingerprint(manifest, &cwd)?
                            } else {
                                None
                            };
                            let mut key = match (cache_allowed, dependency_fingerprint) {
                                (true, Some(fingerprint)) => {
                                    let key = if let Some(module_fingerprint) =
                                        module_fingerprint.as_deref()
                                    {
                                        cache_key_for_program_module(
                                            registry,
                                            manifest,
                                            &argv,
                                            resolved_module_parameters
                                                .as_deref()
                                                .context(
                                                    "program module cache key requires resolved parameters",
                                                )?,
                                            &cwd,
                                            &input,
                                            module_fingerprint,
                                        )?
                                    } else {
                                        cache_key_for_applet(
                                            registry,
                                            manifest,
                                            &argv,
                                            &executable,
                                            &cwd,
                                            &input,
                                        )?
                                    };
                                    Some(if manifest.cache_dependencies().is_empty() {
                                        key
                                    } else {
                                        key_with_dependency_fingerprint(&key, fingerprint)
                                    })
                                }
                                _ => None,
                            };
                            let run_uncached = || {
                                execute_call_group(
                                    &calls,
                                    Input::Bytes(input),
                                    Some((executable, argv)),
                                    &bindings,
                                    &cwd,
                                    options,
                                    &mut result.launched_processes,
                                    signals,
                                    registry,
                                )
                            };
                            let mut hit = if let Some(cache_key) = key.as_deref() {
                                match cache::open(&directory, cache_key)? {
                                    Some(file) => {
                                        read_cached_output(manifest, file, options.max_output_bytes)
                                            .ok()
                                    }
                                    None => None,
                                }
                            } else {
                                None
                            };
                            if hit.is_some()
                                && dependencies_declared
                                && !dependency_fingerprint.is_some_and(|expected| {
                                    dependency_fingerprint_matches(manifest, &cwd, expected)
                                })
                            {
                                // Do not accept a result if any declared file
                                // changed or became unsafe since the key was
                                // first computed. Execute normally without
                                // storing under the stale identity.
                                hit = None;
                                key = None;
                            }
                            if let Some((bytes, status)) = hit {
                                result.cache_hits += 1;
                                let cache_key = key.as_deref().expect("cache key for hit");
                                lazy_event(
                                    options.trace,
                                    signals,
                                    || serde_json::json!({"event":"cache_hit","line":step.line,"applet":manifest.name_for(call),"key":cache_key,"bytes":bytes.len()}),
                                )?;
                                // Cache entries retain terminal bytes/status,
                                // not historical per-stage diagnostics.
                                (bytes, status, true, Vec::new())
                            } else {
                                let completed = run_uncached()?;
                                if let Some(cache_key) = key.take() {
                                    let dependencies_unchanged = !dependencies_declared
                                        || dependency_fingerprint.is_some_and(|expected| {
                                            dependency_fingerprint_matches(manifest, &cwd, expected)
                                        });
                                    if dependencies_unchanged {
                                        if let Ok(entry) = cache_output(
                                            manifest,
                                            &completed.bytes,
                                            completed.status,
                                        ) {
                                            let stored =
                                                cache::write(&directory, &cache_key, &entry)?;
                                            lazy_event(
                                                options.trace,
                                                signals,
                                                || serde_json::json!({"event":if stored {"cache_store"} else {"cache_skip"},"line":step.line,"applet":manifest.name_for(call),"key":cache_key,"bytes":completed.bytes.len()}),
                                            )?;
                                        }
                                    }
                                }
                                (
                                    completed.bytes,
                                    completed.status,
                                    completed.stdout_captured,
                                    completed.stages,
                                )
                            }
                        } else {
                            let completed = execute_call_group(
                                &calls,
                                input,
                                None,
                                &bindings,
                                &cwd,
                                options,
                                &mut result.launched_processes,
                                signals,
                                registry,
                            )?;
                            (
                                completed.bytes,
                                completed.status,
                                completed.stdout_captured,
                                completed.stages,
                            )
                        };
                        let stderr = stages
                            .iter()
                            .rev()
                            .find_map(|stage| stage.stderr.as_deref());
                        if let Some(name) = &call.status_binding {
                            let code = status.context("applet finished without an exit code")?;
                            bindings.insert(name.clone(), Value::Integer(i64::from(code)));
                        }
                        if output_call.result_binding.is_some() && status.is_none() {
                            let mut failure = execution_result::ExecutionResult::runtime_failure(
                                execution_result::ExecutionKind::UnknownError,
                                "applet completed without an exit status; cannot capture a successful execution result",
                                execution_result::EffectCertainty::Uncertain,
                            );
                            failure.stages = stages;
                            return Err(execution_result::ExecutionFailure::new(failure).into());
                        }
                        let output_value = if let Some(name) = &output_call.binding {
                            bind_call_output(
                                name,
                                output_manifest,
                                calls.last().expect("invocation group is nonempty").2,
                                &bytes,
                                status,
                                stdout_captured,
                                stderr,
                                &stages,
                                output_call.result_binding.is_some(),
                                &mut bindings,
                            )?
                        } else if output_call.result_binding.is_some()
                            && stdout_captured
                            && status.is_some_and(|status| {
                                output_manifest.result_envelope()
                                    || (output_manifest.outcome_for(status).kind
                                        == OutcomeKind::Success
                                        && output_manifest.output_kind().is_some())
                            })
                        {
                            Some(decode_call_output(
                                output_manifest,
                                calls.last().expect("invocation group is nonempty").2,
                                &bytes,
                                status,
                                stdout_captured,
                                stderr,
                                &stages,
                            )?)
                        } else {
                            None
                        };
                        if let Some(status) = status {
                            let outcome = output_manifest.outcome_for(status);
                            let line = calls.last().expect("invocation group is nonempty").2;
                            let applet = output_manifest.name_for(output_call);
                            let history_record = execution_history.push_outcome(
                                line,
                                applet,
                                &outcome,
                                stdout_captured.then_some(bytes.as_slice()),
                                stderr,
                                &stages,
                            );
                            if options.trace {
                                let execution_value = if let Some(record) = history_record {
                                    record.to_value()
                                } else {
                                    execution_result::ExecutionRecord::from_outcome(
                                        line,
                                        applet,
                                        &outcome,
                                        stdout_captured.then_some(bytes.as_slice()),
                                        stderr,
                                        &stages,
                                    )
                                    .to_value()
                                };
                                let execution_json = crate::structured::to_json(&execution_value)?;
                                lazy_event(true, signals, || {
                                    serde_json::json!({
                                        "event": "execution_result",
                                        "execution": execution_json,
                                    })
                                })?;
                            }
                            if let Some(name) = &output_call.result_binding {
                                // The full output bytes are retained only when
                                // the source program explicitly asks for them.
                                let execution = execution_result::ExecutionResult::from_outcome(
                                    outcome,
                                    stdout_captured.then(|| bytes.clone()),
                                    stderr.map(<[u8]>::to_vec),
                                    output_value,
                                    stages,
                                );
                                let record = execution.to_source_value(line, applet);
                                bindings.insert(name.clone(), record);
                            }
                        }
                        data = Input::Bytes(bytes);
                    }
                }
            }
            Ok(())
        })();
        if let Some(error) = control_rethrow {
            if let Some(journal) = journal.as_deref_mut() {
                if !journal.persistence_failed() {
                    journal.fail(
                        &format!("{error:#}"),
                        parent_signals.cancelled.load(Ordering::Relaxed)
                            || is_timeout_error(&error)
                            || journal_uncertain,
                    )?;
                }
            }
            return Err(error);
        }
        if let Err(error) = execution {
            let error = call_frames.iter().rev().fold(error, |error, frame| {
                error.context(format!(
                    "program {:?}, called at line {}",
                    frame.name, frame.line
                ))
            });
            let cancellation = !was_cleanup && parent_signals.cancelled.load(Ordering::Relaxed);
            let action = control_state.fail(error, cancellation)?;
            match apply_control_action(
                action,
                program,
                &mut control_state,
                &mut call_frames,
                &mut loop_frames,
                &mut bindings,
                &mut cwd,
            )? {
                AppliedControl::Continue => {}
                AppliedControl::Jump(target) => jump_to = Some(target),
                AppliedControl::Rethrow(error) => {
                    if let Some(journal) = journal.as_deref_mut() {
                        if !journal.persistence_failed() {
                            journal.fail(
                                &format!("{error:#}"),
                                cancellation
                                    || parent_signals.cancelled.load(Ordering::Relaxed)
                                    || is_timeout_error(&error)
                                    || journal_uncertain,
                            )?;
                        }
                    }
                    return Err(error);
                }
            }
        }
        index = jump_to.unwrap_or(index + 1);
        if let Some(journal) = journal
            .as_deref_mut()
            .filter(|_| journal_end == Some(index))
        {
            journal_end = None;
            journal_uncertain = false;
            authorize_checkpoint_persistence(registry, &signals.sensitivity_floor())?;
            let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                options.max_output_bytes,
                signals,
                options.command_timeout,
            )?;
            journal.complete_node_parts(
                &cwd,
                &bytes,
                bindings.iter().map(|(name, value)| (name.as_str(), value)),
            )?;
            data = Input::Bytes(bytes);
            lazy_event(
                options.trace,
                signals,
                || serde_json::json!({"event":"checkpoint_commit","completed_nodes":journal.completed_nodes().len(),"next_step":index}),
            )?;
        }
    }
    result.output = std::mem::replace(&mut data, Input::Empty).materialize(
        options.max_output_bytes,
        signals,
        options.command_timeout,
    )?;
    if let Some(state) = state_out {
        *state = live::NodeState {
            cwd,
            bindings,
            data: result.output.clone(),
        };
    }
    result.executions = execution_history.executions;
    result.dropped_executions = execution_history.dropped_executions;
    result.dropped_stages = execution_history.dropped_stages;
    Ok(result)
}

enum AppliedControl {
    Continue,
    Jump(usize),
    Rethrow(anyhow::Error),
}

fn apply_control_action<'a>(
    action: control_flow::ControlAction,
    program: &Program,
    state: &mut control_flow::RuntimeState,
    call_frames: &mut Vec<calls::Frame<'a>>,
    loop_frames: &mut loops::LoopStack,
    bindings: &mut BTreeMap<String, Value>,
    cwd: &mut PathBuf,
) -> Result<AppliedControl> {
    fn remove_bindings(bindings: &mut BTreeMap<String, Value>, names: Vec<String>) {
        for name in names {
            bindings.remove(&name);
        }
    }
    fn unwind(
        unwind: control_flow::UnwindTo,
        state: &mut control_flow::RuntimeState,
        call_frames: &mut Vec<calls::Frame<'_>>,
        loop_frames: &mut loops::LoopStack,
        bindings: &mut BTreeMap<String, Value>,
        cwd: &mut PathBuf,
    ) -> Result<()> {
        let control_flow::UnwindTo {
            call_depth,
            loop_depth,
            drop_bindings,
        } = unwind;
        if call_depth > call_frames.len() {
            bail!("control transfer targets an inactive program scope");
        }
        while call_frames.len() > call_depth {
            let frame = call_frames.pop().expect("call depth checked");
            *bindings = frame.bindings;
            *cwd = frame.cwd;
        }
        loop_frames.unwind_to(loop_depth, bindings)?;
        remove_bindings(bindings, drop_bindings);
        state.apply_unwind(&control_flow::UnwindTo::new(call_depth, loop_depth));
        Ok(())
    }

    match action {
        control_flow::ControlAction::Continue { drop_bindings } => {
            remove_bindings(bindings, drop_bindings);
            Ok(AppliedControl::Continue)
        }
        control_flow::ControlAction::Jump {
            target,
            transfer,
            unwind: target_scope,
        } => {
            let continuing = transfer == Some(control_flow::TransferKind::Continue);
            unwind(target_scope, state, call_frames, loop_frames, bindings, cwd)?;
            if continuing {
                let Some(StepKind::EndLoop { back }) =
                    program.steps.get(target).map(|step| &step.kind)
                else {
                    bail!("continue target is not an end-loop marker");
                };
                let header = target
                    .checked_sub(*back)
                    .context("invalid continue loop edge")?;
                loop_frames.end_iteration(header, bindings)?;
            }
            Ok(AppliedControl::Jump(target))
        }
        control_flow::ControlAction::Catch {
            target,
            binding,
            error,
            unwind: target_scope,
        } => {
            unwind(target_scope, state, call_frames, loop_frames, bindings, cwd)?;
            bindings.insert(binding, execution_result::error_value(&error));
            Ok(AppliedControl::Jump(target))
        }
        control_flow::ControlAction::Rethrow(error) => Ok(AppliedControl::Rethrow(error)),
    }
}

/// Execute a component worker using an explicit cancellation token and port values.
pub fn run_with_context(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    bindings: BTreeMap<String, Value>,
    input: Vec<u8>,
    cancelled: Arc<AtomicBool>,
) -> Result<RunResult> {
    if program.trigger.is_some() {
        bail!("triggered programs require run_service");
    }
    let cwd = fs::canonicalize(&options.working_directory)?;
    crate::capabilities::preflight_with_cache_directory(
        program,
        registry,
        &cwd,
        options.cache_directory.as_deref(),
    )?;
    if !options.inputs.is_empty() {
        crate::inputs::known_values(&program.inputs, &options.inputs)?;
    }
    validate_with_inputs(
        program,
        registry,
        bindings.keys().cloned().collect(),
        !bindings.is_empty() || !input.is_empty(),
    )?;
    #[cfg(not(target_arch = "wasm32"))]
    validate_worker_support(program, registry)?;
    if !program.inputs.is_empty() {
        let mut supplied = options.inputs.clone();
        supplied.extend(bindings.clone());
        let values = crate::inputs::resolve(&program.inputs, &supplied, true)?;
        preflight_inputs(program, registry, &values)?;
    }
    let signals = Signals::with_cancellation_and_deadline(
        cancelled,
        true,
        deadline_after(options.total_timeout, "total timeout")?,
    )?;
    preload_program_modules(program, registry, &signals, &cwd)?;
    run_finite_state(
        program,
        registry,
        options,
        &signals,
        Input::Bytes(input),
        bindings,
        None,
        FiniteStateSource::Fresh,
        None,
    )
}
#[cfg(unix)]
fn drain_diagnostic(child: &mut ManagedChild) -> Result<()> {
    let mut reached_eof = false;
    let Some(stderr) = child.stderr.as_mut() else {
        return Ok(());
    };
    let mut buffer = [0u8; 4096];
    for _ in 0..8 {
        match stderr.read(&mut buffer) {
            Ok(0) => {
                reached_eof = true;
                break;
            }
            Ok(count) => {
                let keep = count.min(8192usize.saturating_sub(child.diagnostic.len()));
                child.diagnostic.extend_from_slice(&buffer[..keep]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("reading applet diagnostics"),
        }
    }
    if reached_eof {
        // Removing the closed descriptor prevents poll from spinning on HUP.
        child.stderr.take();
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod child_observer_tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn kernel_creates_kqueue_close_on_exec() {
        let observer = ChildObserver::new().expect("creating test kqueue");
        let flags = unsafe { libc::fcntl(observer.raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    struct RestoreSigchld(libc::sigaction);
    impl Drop for RestoreSigchld {
        fn drop(&mut self) {
            unsafe {
                libc::sigaction(libc::SIGCHLD, &self.0, std::ptr::null_mut());
            }
        }
    }

    #[test]
    #[ignore = "isolated process-wide SIGCHLD disposition check"]
    fn runtime_observer_preserves_host_sigchld_ignore() {
        let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
        let mut ignored = unsafe { std::mem::zeroed::<libc::sigaction>() };
        unsafe {
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut previous),
                0
            );
            ignored.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut ignored.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &ignored, std::ptr::null_mut()),
                0
            );
        }
        let _restore = RestoreSigchld(previous);

        {
            let _signals =
                Signals::with_cancellation(Arc::new(AtomicBool::new(false)), true).unwrap();
            let _observer = ChildObserver::new();
        }

        let mut observed = unsafe { std::mem::zeroed::<libc::sigaction>() };
        unsafe {
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut observed),
                0
            );
        }
        assert_eq!(observed.sa_sigaction, libc::SIG_IGN);
    }
}

#[cfg(all(test, unix))]
mod argv_construction_tests {
    use super::*;
    use tempfile::tempdir;

    fn opt_runtime_registry(json: &str) -> Registry {
        let dir = tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        fs::write(&path, json).unwrap();
        Registry::load(&path).unwrap()
    }

    #[test]
    fn opt_runtime_borrows_only_uncached_literal_arguments() {
        let registry = opt_runtime_registry(
            r#"{"name":"check","binary":"tool","parameters":{"text":{"type":"string","required":true}},"arguments":[{"literal":"prefix"},{"parameter":"text"}]}"#,
        );
        let manifest = registry.resolve("check").unwrap();
        let bindings = BTreeMap::from([("result".into(), Value::String("resolved".into()))]);
        let literal = Invocation {
            operation: "check".into(),
            direct: false,
            parameters: BTreeMap::from([("text".into(), Value::String("literal".into()))]),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
        };
        match arguments_for_group_call(&literal, manifest, &bindings).unwrap() {
            GroupArguments::Borrowed(arguments) => {
                let rendered: Vec<&str> =
                    arguments.iter().map(|argument| argument.as_ref()).collect();
                assert_eq!(rendered, ["prefix", "literal"]);
            }
            GroupArguments::Owned(_) => panic!("uncached literal arguments should borrow"),
        }

        let mut referenced = literal.clone();
        referenced
            .parameters
            .insert("text".into(), Value::Reference("result".into()));
        match arguments_for_group_call(&referenced, manifest, &bindings).unwrap() {
            GroupArguments::Owned(arguments) => assert_eq!(arguments, ["prefix", "resolved"]),
            GroupArguments::Borrowed(_) => panic!("reference results must use owned arguments"),
        }

        let mut cached = literal;
        cached.cache = true;
        match arguments_for_group_call(&cached, manifest, &bindings).unwrap() {
            GroupArguments::Owned(arguments) => assert_eq!(arguments, ["prefix", "literal"]),
            GroupArguments::Borrowed(_) => panic!("cached calls must use owned arguments"),
        }
    }

    fn sample_command_construction(
        manifest: &Manifest,
        empty: &BTreeMap<String, Value>,
        borrowed: bool,
        iterations: usize,
    ) -> u128 {
        let started = Instant::now();
        for _ in 0..iterations {
            let mut command = Command::new("unused-binary");
            if borrowed {
                let arguments = manifest.build_arguments_borrowed(empty).unwrap();
                command.args(arguments.iter().map(|argument| argument.as_ref()));
            } else {
                let arguments = manifest.build_arguments(empty).unwrap();
                command.args(&arguments);
            }
            std::hint::black_box(command);
        }
        started.elapsed().as_nanos()
    }

    #[test]
    #[ignore = "opt-in in-process argv construction timing; this never spawns a process"]
    fn opt_runtime_command_construction_benchmark_raw_samples() {
        let dir = tempdir().unwrap();
        for argument_count in [2usize, 256, 4096] {
            let arguments = (0..argument_count)
                .map(|index| serde_json::json!({"literal": format!("arg-{index}")}))
                .collect::<Vec<_>>();
            let manifest_json = serde_json::json!({
                "name": "benchmark",
                "binary": "unused-binary",
                "arguments": arguments,
            });
            let path = dir.path().join(format!("{argument_count}.json"));
            fs::write(&path, serde_json::to_vec(&manifest_json).unwrap()).unwrap();
            let registry = Registry::load(&path).unwrap();
            let manifest = registry.resolve("benchmark").unwrap();
            let empty = BTreeMap::new();
            let iterations = match argument_count {
                2 => 10_000,
                256 => 500,
                _ => 50,
            };
            for batch in 0..5 {
                let modes = if batch % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                };
                for borrowed in modes {
                    let elapsed =
                        sample_command_construction(manifest, &empty, borrowed, iterations);
                    println!(
                        "opt_argv_sample mode={} argc={argument_count} batch={batch} iterations={iterations} total_ns={elapsed} ns_per_op={:.1}",
                        if borrowed { "borrowed" } else { "owned" },
                        elapsed as f64 / iterations as f64,
                    );
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod opt_manifest_path_cache_tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;
    use tempfile::tempdir;

    fn opt_manifest_registry(dir: &Path) -> Registry {
        let manifest_path = dir.join("unicode-manifest.json");
        fs::write(
            &manifest_path,
            r#"{"name":"λ","binary":"tool","arguments":[{"literal":"雪"}]}"#,
        )
        .unwrap();
        Registry::load(&manifest_path).unwrap()
    }

    #[test]
    fn opt_manifest_path_value_borrows_literal_and_one_level_string_reference() {
        let cwd = Path::new("/workspace/π");
        let bindings = BTreeMap::from([
            ("text".to_owned(), Value::String("nested/雪".to_owned())),
            ("number".to_owned(), Value::Integer(7)),
            ("reference".to_owned(), Value::Reference("text".to_owned())),
        ]);
        assert_eq!(
            path_value(&Value::String("literal/é".to_owned()), &bindings, cwd).unwrap(),
            cwd.join("literal/é")
        );
        assert_eq!(
            path_value(&Value::Reference("text".to_owned()), &bindings, cwd).unwrap(),
            cwd.join("nested/雪")
        );
        assert_eq!(
            path_value(&Value::Reference("reference".to_owned()), &bindings, cwd)
                .unwrap_err()
                .to_string(),
            "path result must contain text"
        );
        assert_eq!(
            path_value(&Value::Reference("number".to_owned()), &bindings, cwd)
                .unwrap_err()
                .to_string(),
            "path result must contain text"
        );
        assert_eq!(
            path_value(&Value::Reference("missing".to_owned()), &bindings, cwd)
                .unwrap_err()
                .to_string(),
            "undefined result \"missing\""
        );
    }

    fn opt_manifest_legacy_cache_key_with_environment<F, I>(
        manifest: &Manifest,
        argv: &[String],
        executable: &Path,
        cwd: &Path,
        input: &[u8],
        environment_source: F,
    ) -> Result<String>
    where
        F: FnOnce() -> I,
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        let mut hash = Sha256::new();
        hash_field(&mut hash, b"flow-cache-v1");
        hash_field(&mut hash, &serde_json::to_vec(manifest)?);
        hash_field(&mut hash, &serde_json::to_vec(argv)?);
        hash_field(&mut hash, cwd.as_os_str().as_encoded_bytes());
        hash_field(&mut hash, executable.as_os_str().as_encoded_bytes());
        let mut binary_hash = Sha256::new();
        let mut file = File::open(executable)?;
        let mut chunk = [0u8; 32768];
        loop {
            let count = file.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            binary_hash.update(&chunk[..count]);
        }
        hash_field(&mut hash, &binary_hash.finalize());
        let environment: BTreeMap<_, _> = environment_source().into_iter().collect();
        for (name, value) in environment {
            hash_field(&mut hash, name.as_os_str().as_encoded_bytes());
            hash_field(&mut hash, value.as_os_str().as_encoded_bytes());
        }
        hash_field(&mut hash, input);
        Ok(format!("{:x}", hash.finalize()))
    }

    fn opt_manifest_env_snapshot() -> Vec<(OsString, OsString)> {
        vec![
            (OsString::from("Z_LAST"), OsString::from("tail")),
            (OsString::from("DUP_KEY"), OsString::from("first value")),
            (
                OsString::from_vec(vec![0x80, b'K']),
                OsString::from_vec(vec![0xff, b'V']),
            ),
            (OsString::from("A_FIRST"), OsString::from("雪")),
            (OsString::from("DUP_KEY"), OsString::from("last value")),
        ]
    }

    fn opt_manifest_legacy_hash_environment<I>(hash: &mut Sha256, environment: I)
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        let environment: BTreeMap<_, _> = environment.into_iter().collect();
        for (name, value) in environment {
            hash_field(hash, name.as_os_str().as_encoded_bytes());
            hash_field(hash, value.as_os_str().as_encoded_bytes());
        }
    }

    #[test]
    fn opt_manifest_cache_key_matches_legacy_bytes_for_unicode_and_os_environment() {
        let dir = tempdir().unwrap();
        let registry = opt_manifest_registry(dir.path());
        let manifest = registry.resolve("λ").unwrap();
        let executable = dir.path().join("dummy-binary");
        fs::write(&executable, b"owned synthetic executable bytes").unwrap();
        let cwd = dir.path().join("工作目录");
        let argv = vec![String::new(), "東京".to_owned(), "café".to_owned()];
        let input = b"cache input";
        let environment = opt_manifest_env_snapshot();

        let optimized =
            cache_key_with_environment(manifest, &argv, &executable, &cwd, input, || {
                environment.clone()
            })
            .unwrap();
        let legacy = opt_manifest_legacy_cache_key_with_environment(
            manifest,
            &argv,
            &executable,
            &cwd,
            input,
            || environment,
        )
        .unwrap();
        assert_eq!(optimized, legacy);
    }

    #[test]
    #[ignore = "opt-in in-process cache-key timing; uses a temporary file and never spawns"]
    fn opt_manifest_cache_key_benchmark_raw_samples() -> Result<()> {
        let dir = tempdir()?;
        let registry = opt_manifest_registry(dir.path());
        let manifest = registry.resolve("λ").unwrap();
        let executable = dir.path().join("dummy-binary");
        fs::write(&executable, vec![b'x'; 1024])?;
        let cwd = dir.path().join("工作目录");
        let argv = vec![String::new(), "東京".to_owned(), "café".to_owned()];
        let input = b"cache input";
        let snapshot: Vec<_> = std::env::vars_os().collect();
        assert_eq!(
            cache_key_with_environment(manifest, &argv, &executable, &cwd, input, || snapshot
                .clone(),)?,
            opt_manifest_legacy_cache_key_with_environment(
                manifest,
                &argv,
                &executable,
                &cwd,
                input,
                || snapshot.clone(),
            )?
        );

        let iterations = 200;
        for batch in 0..5 {
            let modes = if batch % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for optimized in modes {
                let started = Instant::now();
                for _ in 0..iterations {
                    let key = if optimized {
                        cache_key(manifest, &argv, &executable, &cwd, input)?
                    } else {
                        opt_manifest_legacy_cache_key_with_environment(
                            manifest,
                            &argv,
                            &executable,
                            &cwd,
                            input,
                            std::env::vars_os,
                        )?
                    };
                    std::hint::black_box(key);
                }
                let elapsed = started.elapsed().as_nanos();
                println!(
                    "opt_cache_key_sample mode={} batch={batch} iterations={iterations} total_ns={elapsed} ns_per_op={:.1}",
                    if optimized {
                        "reused_vec_sorted_env"
                    } else {
                        "to_vec_btreemap"
                    },
                    elapsed as f64 / iterations as f64,
                );
            }
        }

        // Isolate the environment container/sort cost at ordinary and unusually
        // large sizes without changing the process-wide environment.
        for environment_size in [16usize, 512] {
            let environment: Vec<_> = (0..environment_size)
                .rev()
                .map(|index| {
                    (
                        OsString::from(format!("OPT_BENCH_{:04}", index % 128)),
                        OsString::from(format!("value-{index}")),
                    )
                })
                .collect();
            let iterations = if environment_size == 16 { 5_000 } else { 100 };
            for batch in 0..5 {
                let modes = if batch % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                };
                for optimized in modes {
                    let started = Instant::now();
                    for _ in 0..iterations {
                        let mut hash = Sha256::new();
                        if optimized {
                            hash_environment(&mut hash, environment.iter().cloned());
                        } else {
                            opt_manifest_legacy_hash_environment(
                                &mut hash,
                                environment.iter().cloned(),
                            );
                        }
                        std::hint::black_box(hash.finalize());
                    }
                    let elapsed = started.elapsed().as_nanos();
                    println!(
                        "opt_env_sample mode={} vars={environment_size} batch={batch} iterations={iterations} total_ns={elapsed} ns_per_op={:.1}",
                        if optimized { "sorted_vec" } else { "btreemap" },
                        elapsed as f64 / iterations as f64,
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod opt_manifest_binary_path_tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn opt_manifest_legacy_binary_path(binary: &str, cwd: &Path, path: &OsStr) -> Result<PathBuf> {
        // This preserves the original candidate construction as a parity oracle.
        if binary.contains('/')
            || (cfg!(windows) && binary.contains('\\'))
            || Path::new(binary).is_absolute()
        {
            let candidate = crate::platform::executable_path(cwd.join(binary))?;
            let metadata = fs::metadata(&candidate)?;
            if !metadata.is_file() || !crate::platform::executable(&metadata) {
                bail!("{} is not an executable file", candidate.display());
            }
            Ok(candidate)
        } else {
            std::env::split_paths(path)
                .find_map(|entry| {
                    let candidate = cwd.join(entry).join(binary);
                    let metadata = fs::metadata(&candidate).ok()?;
                    (metadata.is_file() && crate::platform::executable(&metadata))
                        .then_some(candidate)
                })
                .ok_or_else(|| anyhow!("executable {binary:?} not found in PATH"))
        }
    }

    fn opt_manifest_join_path(entries: &[PathBuf]) -> OsString {
        std::env::join_paths(entries.iter().map(|entry| entry.as_os_str()))
            .expect("test PATH components are representable")
    }

    fn opt_manifest_write_file(path: &Path, mode: u32) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, b"synthetic executable metadata fixture")?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    fn opt_manifest_assert_path_parity(binary: &str, cwd: &Path, path: &OsStr) {
        let legacy = opt_manifest_legacy_binary_path(binary, cwd, path);
        let optimized = binary_path_from_path(binary, cwd, path);
        match (legacy, optimized) {
            (Ok(legacy), Ok(optimized)) => assert_eq!(
                legacy.as_os_str().as_encoded_bytes(),
                optimized.as_os_str().as_encoded_bytes(),
                "selected path bytes differ"
            ),
            (Err(legacy), Err(optimized)) => {
                assert_eq!(legacy.to_string(), optimized.to_string())
            }
            (legacy, optimized) => {
                panic!("lookup outcomes differ: legacy={legacy:?}, optimized={optimized:?}")
            }
        }
    }

    #[test]
    fn opt_manifest_binary_path_preserves_path_entry_spelling_and_os_bytes() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let binary = "probe";

        let mut double_slash_bytes = cwd.as_os_str().as_encoded_bytes().to_vec();
        double_slash_bytes.extend_from_slice(b"/double//child");
        let double_slash = PathBuf::from(OsString::from_vec(double_slash_bytes));
        let non_utf8 = PathBuf::from(OsString::from_vec(vec![b'n', 0x80, b'd']));
        let absolute = root.path().join("absolute");
        let entries = vec![
            PathBuf::from("relative"),
            PathBuf::from("."),
            PathBuf::new(),
            double_slash.clone(),
            non_utf8.clone(),
            absolute.clone(),
        ];

        for entry in &entries {
            let candidate = cwd.join(entry).join(binary);
            let path = opt_manifest_join_path(std::slice::from_ref(entry));
            // APFS rejects invalid UTF-8 filenames; that entry still exercises
            // byte-preserving PATH splitting and identical failed lookup.
            if entry == &non_utf8 && cfg!(target_os = "macos") {
                opt_manifest_assert_path_parity(binary, &cwd, &path);
                assert!(binary_path_from_path(binary, &cwd, &path).is_err());
                continue;
            }
            opt_manifest_write_file(&candidate, 0o755).unwrap();
            opt_manifest_assert_path_parity(binary, &cwd, &path);
            let selected = binary_path_from_path(binary, &cwd, &path).unwrap();
            assert_eq!(
                selected.as_os_str().as_encoded_bytes(),
                candidate.as_os_str().as_encoded_bytes()
            );
        }

        // A joined PATH retains non-UTF-8 bytes; splitting it must do so too.
        let path = opt_manifest_join_path(std::slice::from_ref(&non_utf8));
        let split = std::env::split_paths(&path).next().unwrap();
        assert_eq!(
            split.as_os_str().as_encoded_bytes(),
            non_utf8.as_os_str().as_encoded_bytes()
        );
        assert!(double_slash
            .as_os_str()
            .as_bytes()
            .windows(2)
            .any(|pair| pair == b"//"));
    }

    #[test]
    fn opt_manifest_binary_path_preserves_misses_permissions_and_late_hit() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let binary = "probe";

        let missing_path = opt_manifest_join_path(&[PathBuf::from("absent")]);
        opt_manifest_assert_path_parity(binary, &cwd, &missing_path);
        assert_eq!(
            binary_path_from_path(binary, &cwd, &missing_path)
                .unwrap_err()
                .to_string(),
            "executable \"probe\" not found in PATH"
        );

        let non_executable = PathBuf::from("no-execute");
        opt_manifest_write_file(&cwd.join(&non_executable).join(binary), 0o644).unwrap();
        let non_executable_path = opt_manifest_join_path(std::slice::from_ref(&non_executable));
        opt_manifest_assert_path_parity(binary, &cwd, &non_executable_path);
        assert_eq!(
            binary_path_from_path(binary, &cwd, &non_executable_path)
                .unwrap_err()
                .to_string(),
            "executable \"probe\" not found in PATH"
        );

        let mut entries = Vec::with_capacity(65);
        for index in 0..64 {
            let entry = PathBuf::from(format!("miss-{index:02}"));
            fs::create_dir_all(cwd.join(&entry)).unwrap();
            entries.push(entry);
        }
        let late_hit = PathBuf::from("late-hit");
        let expected = cwd.join(&late_hit).join(binary);
        opt_manifest_write_file(&expected, 0o755).unwrap();
        entries.push(late_hit);
        let late_path = opt_manifest_join_path(&entries);
        opt_manifest_assert_path_parity(binary, &cwd, &late_path);
        let selected = binary_path_from_path(binary, &cwd, &late_path).unwrap();
        assert_eq!(
            selected.as_os_str().as_encoded_bytes(),
            expected.as_os_str().as_encoded_bytes()
        );
    }

    #[test]
    fn opt_manifest_slash_binary_keeps_direct_lookup_and_metadata_error() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("work");
        let binary = "nested/probe";
        let expected = cwd.join(binary);
        opt_manifest_write_file(&expected, 0o755).unwrap();
        let irrelevant_path = OsStr::new("unused");
        let legacy = opt_manifest_legacy_binary_path(binary, &cwd, irrelevant_path).unwrap();
        let current = binary_path(binary, &cwd).unwrap();
        assert_eq!(
            legacy.as_os_str().as_encoded_bytes(),
            expected.as_os_str().as_encoded_bytes()
        );
        assert_eq!(
            current.as_os_str().as_encoded_bytes(),
            expected.as_os_str().as_encoded_bytes()
        );

        let missing = "missing/probe";
        let legacy_error = opt_manifest_legacy_binary_path(missing, &cwd, irrelevant_path)
            .unwrap_err()
            .to_string();
        let current_error = binary_path(missing, &cwd).unwrap_err().to_string();
        assert_eq!(current_error, legacy_error);
        assert_ne!(
            current_error,
            "executable \"missing/probe\" not found in PATH"
        );
    }

    fn opt_manifest_lookup_sample(
        lookup: fn(&str, &Path, &OsStr) -> Result<PathBuf>,
        binary: &str,
        cwd: &Path,
        path: &OsStr,
        iterations: usize,
    ) -> u128 {
        let started = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(lookup(binary, cwd, path).expect("warm executable fixture"));
        }
        started.elapsed().as_nanos()
    }

    #[test]
    #[ignore = "opt-in PATH lookup timing; metadata is warmed and no process is launched"]
    fn opt_manifest_binary_path_benchmark_raw_samples() -> Result<()> {
        let root = tempdir()?;
        let cwd = root.path().join("work");
        fs::create_dir_all(&cwd)?;

        let single_entry = PathBuf::from("single");
        let single_candidate = cwd.join(&single_entry).join("probe");
        opt_manifest_write_file(&single_candidate, 0o755)?;
        let single_path = opt_manifest_join_path(std::slice::from_ref(&single_entry));

        let mut late_entries = Vec::with_capacity(65);
        for index in 0..64 {
            let entry = PathBuf::from(format!("miss-{index:02}"));
            fs::create_dir_all(cwd.join(&entry))?;
            late_entries.push(entry);
        }
        let late_entry = PathBuf::from("late-hit");
        opt_manifest_write_file(&cwd.join(&late_entry).join("probe"), 0o755)?;
        late_entries.push(late_entry);
        let late_path = opt_manifest_join_path(&late_entries);

        // Prime both code paths so subsequent batches include warmed metadata.
        for path in [&single_path, &late_path] {
            opt_manifest_legacy_binary_path("probe", &cwd, path)?;
            binary_path_from_path("probe", &cwd, path)?;
        }
        for (scenario, path, iterations) in [
            ("path1", &single_path, 2_000usize),
            ("path64_miss_prefix_late_hit", &late_path, 150usize),
        ] {
            for batch in 0..5 {
                let modes = if batch % 2 == 0 { [0, 1] } else { [1, 0] };
                for mode in modes {
                    let (name, lookup) = if mode == 0 {
                        (
                            "legacy",
                            opt_manifest_legacy_binary_path
                                as fn(&str, &Path, &OsStr) -> Result<PathBuf>,
                        )
                    } else {
                        (
                            "reused_buffer",
                            binary_path_from_path as fn(&str, &Path, &OsStr) -> Result<PathBuf>,
                        )
                    };
                    let elapsed =
                        opt_manifest_lookup_sample(lookup, "probe", &cwd, path, iterations);
                    println!(
                        "opt_manifest_binary_path_sample mode={name} scenario={scenario} batch={batch} iterations={iterations} total_ns={elapsed} ns_per_op={:.1}",
                        elapsed as f64 / iterations as f64,
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod input_writer_drop_tests {
    use super::InputWriter;
    use std::io::{PipeWriter, Write};
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    struct NotifyingPipeWriter {
        writer: PipeWriter,
        started: Option<mpsc::Sender<()>>,
        finished: Arc<AtomicBool>,
    }

    impl Write for NotifyingPipeWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            self.writer.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.writer.flush()
        }
    }

    impl AsRawFd for NotifyingPipeWriter {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            self.writer.as_raw_fd()
        }
    }

    impl Drop for NotifyingPipeWriter {
        fn drop(&mut self) {
            self.finished.store(true, Ordering::Release);
        }
    }

    fn fill_pipe(writer: &mut PipeWriter) {
        let fd = writer.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );

        let chunk = [0u8; 4096];
        loop {
            match writer.write(&chunk) {
                Ok(0) => panic!("pipe accepted a zero-length write"),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("filling stdin pipe: {error}"),
            }
        }

        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) }, 0);
    }

    #[test]
    fn dropping_input_writer_does_not_join_while_pipe_reader_is_retained() {
        let (reader, mut pipe_writer) = std::io::pipe().unwrap();
        fill_pipe(&mut pipe_writer);

        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let (started_tx, started_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            let mut input_writer = InputWriter::default();
            input_writer.spawn(
                NotifyingPipeWriter {
                    writer: pipe_writer,
                    started: Some(started_tx),
                    finished: worker_finished,
                },
                vec![1],
                0,
                Arc::new(AtomicBool::new(false)),
                None,
            );
            let write_started = started_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            thread::sleep(Duration::from_millis(20));
            drop(input_writer);
            dropped_tx.send(write_started).unwrap();
        });

        let returned_while_reader_open = dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or(false);
        let finish_deadline = Instant::now() + Duration::from_secs(1);
        while !finished.load(Ordering::Acquire) && Instant::now() < finish_deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let finished_with_reader_open = finished.load(Ordering::Acquire);
        // Always release the owned reader before assertions so regressions cannot
        // strand the test's writer. The fixed worker stops before this release.
        drop(reader);
        dropper.join().unwrap();
        assert!(
            returned_while_reader_open,
            "writer Drop blocked on the full pipe"
        );
        assert!(
            finished_with_reader_open,
            "dropping the owner failed to stop its writer while a reader remained open"
        );
    }

    #[test]
    fn joined_input_writer_observes_deadline_and_cancellation_with_a_retained_reader() {
        for cancel in [false, true] {
            let (reader, mut pipe_writer) = std::io::pipe().unwrap();
            fill_pipe(&mut pipe_writer);
            let cancelled = Arc::new(AtomicBool::new(false));
            let (release, retained) = mpsc::channel();
            let guard = thread::spawn(move || {
                let _ = retained.recv_timeout(Duration::from_secs(1));
                drop(reader);
            });
            let mut writer = InputWriter::default();
            writer.spawn(
                pipe_writer,
                vec![1],
                0,
                cancelled.clone(),
                (!cancel).then(|| Instant::now() + Duration::from_millis(30)),
            );
            if cancel {
                cancelled.store(true, Ordering::Relaxed);
            }
            let result = writer.join();
            let _ = release.send(());
            guard.join().unwrap();
            let error =
                result.expect_err("a retained reader must not hide cancellation or deadline");
            if cancel {
                assert!(error.to_string().contains("cancelled"));
            } else {
                assert!(error.is::<super::CommandTimedOut>());
            }
        }
    }
}

#[cfg(test)]
mod in_process_budget_tests {
    use super::*;

    #[test]
    fn cooperative_budget_errors_are_typed_with_uncertain_effects() {
        for (operation, budget, applet_kind, expected_kind) in [
            (
                "json.map",
                crate::applets::ExecutionBudgetExceeded::Timeout,
                crate::applets::AppletErrorKind::Timeout,
                execution_result::ExecutionKind::Timeout,
            ),
            (
                "json.filter",
                crate::applets::ExecutionBudgetExceeded::Cancelled,
                crate::applets::AppletErrorKind::Cancelled,
                execution_result::ExecutionKind::Cancelled,
            ),
        ] {
            let items = (0..1024).collect::<Vec<_>>();
            let document = serde_json::json!({
                "items": items
                    .iter()
                    .map(|value| serde_json::json!({"value": value}))
                    .collect::<Vec<_>>()
            });
            let arguments = if operation == "json.map" {
                serde_json::json!({"document": document, "pointer": "/items", "select": "/value"})
            } else {
                serde_json::json!({
                    "document": document,
                    "pointer": "/items",
                    "where": [{"pointer": "/value", "exists": true}]
                })
            }
            .as_object()
            .unwrap()
            .clone();
            let checkpoints = std::sync::atomic::AtomicUsize::new(0);
            let checkpoint = || {
                if checkpoints.fetch_add(1, Ordering::Relaxed) + 1 == 23 {
                    Err(budget)
                } else {
                    Ok(())
                }
            };
            let context = crate::applets::ExecutionContext::new(&checkpoint);
            let error = crate::applets::execute_with_context(
                operation,
                &arguments,
                &[],
                Path::new("."),
                &context,
            )
            .expect_err("the controlled in-loop checkpoint should stop the applet");
            assert_eq!(error.kind, applet_kind);
            let result = in_process_budget_failure(&error, 17, "fixture.transform")
                .expect("budget failures have a runtime classification");

            assert_eq!(result.kind, expected_kind);
            assert_eq!(
                result.effect_certainty,
                execution_result::EffectCertainty::Uncertain
            );
            assert_eq!(result.stages.len(), 1);
            assert_eq!(result.stages[0].line, 17);
            assert_eq!(result.stages[0].applet, "fixture.transform");
            assert_eq!(
                result.stages[0].state,
                execution_result::StageState::Unknown
            );
            assert_eq!(
                result.stages[0].effect_certainty,
                execution_result::EffectCertainty::Uncertain
            );
            assert_eq!(checkpoints.load(Ordering::Relaxed), 23);
        }

        let ordinary = crate::applets::AppletError {
            code: "invalid_request",
            kind: crate::applets::AppletErrorKind::Permanent,
            message: "ordinary applet failure".into(),
        };
        assert!(in_process_budget_failure(&ordinary, 17, "fixture.transform").is_none());
    }
}
