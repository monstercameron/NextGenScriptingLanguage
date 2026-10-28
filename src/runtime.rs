//! Unix program executor with finite pipelines and persistent event services.
use crate::ast::{
    Connection, Invocation, ListenSource, Program, RetryPolicy, StepKind, Trigger, Value,
};
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
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::fd::{FromRawFd, OwnedFd};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(all(test, unix))]
use std::process::Command;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

mod cache;
mod calls;
pub(crate) mod checkpoint;
mod control;
mod control_flow;
pub mod execution_result;
pub(crate) mod handoff;
pub mod invocation_io;
mod jobs;
pub(crate) mod live;
mod loops;
pub use loops::ForEachInputTypeError;
pub mod observe;
mod redirect;
mod spawn;
mod step_loop;
#[cfg(unix)]
use spawn::execute_group_with_sink;
use spawn::{
    captured_stage_stderr, collect_native_stages, execute_group, redact_command_failure,
    GroupOutput, GroupRequest,
};
// Used by the Windows process path (windows_process.rs) through this module.
#[cfg(windows)]
use spawn::{
    attach_abort_stdout, check_native_abort, native_output_limit_failure, native_spawn_failure,
    resolved_call_stdio,
};
use step_loop::{
    run_finite, run_finite_inner, run_finite_inner_with_state_source, run_finite_state, FiniteRun,
    FiniteStart, FiniteStateSource,
};
pub(crate) mod statement_hook;
mod temp;
mod terminal;
use crate::working_dir::enter_directory;
use observe::execute_call_group;
use redirect::{
    append_to_open_file, open_output_file, open_redirect_target, verify_opened_identity,
    ForceStderrCapture, PendingRedirect, StreamScope,
};
mod program_backend;
pub(crate) mod resume;
mod service;
mod streaming;
pub mod tasks;
pub mod worker_backend;
#[cfg(feature = "graph")]
pub(crate) use checkpoint::graph::{open_graph_checkpoint, PersistenceAuthorizer};
pub use checkpoint::{read_repair_state, CheckpointOptions, RepairState};
pub use resume::run_checkpointed;

/// Mark descriptors inherited by this process close-on-exec before starting runtime work.
///
/// Call this once at executable startup, before creating threads. Child process launches then
/// inherit the usual standard streams only, without a per-spawn fork hook.
#[cfg(unix)]
pub fn mark_inherited_descriptors_close_on_exec() -> Result<()> {
    crate::platform::unix_ffi::mark_inherited_descriptors_close_on_exec()
        .context("marking inherited descriptors close-on-exec")
}

#[cfg(feature = "graph")]
pub(crate) use streaming::run_stream_with_context_and_process_observer;
pub use streaming::{
    run_stream, run_stream_with_cancellation, run_stream_with_context, StreamExecutionContext,
    StreamResult,
};
#[cfg_attr(windows, path = "runtime/windows_stdio.rs")]
mod stdio;
#[cfg(feature = "live")]
pub(crate) use service::next_interval;
#[cfg(feature = "graph")]
pub(crate) use service::{run_program_module_service, ProgramServiceContext};
pub use service::{
    run_service, run_service_checkpointed, run_service_checkpointed_with_schedule_options,
    run_service_with_cancellation, run_service_with_schedule_options, CalendarDedupPolicy,
    MissedTickPolicy, ScheduleOptions, ServiceLimits, ServiceResult,
};
pub use stdio::{write_cancellable, write_diagnostic, write_exit_diagnostic, write_with_deadline};

pub(crate) type ProcessObserver = Arc<dyn Fn(usize) + Send + Sync>;
type PreparedJobCommands = Vec<(PathBuf, Vec<String>)>;

/// Default maximum number of iterations entered by one loop execution.
pub const DEFAULT_MAX_LOOP_ITERATIONS: usize = loops::MAX_LOOP_ITERATIONS;

/// How entered iterations are charged. Legacy shared accounting is retained
/// for replaying records written before per-loop limits were introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopBudgetScope {
    PerLoop,
    LegacySharedRun,
}

#[derive(Clone)]
pub struct RunOptions {
    pub inputs: BTreeMap<String, Value>,
    pub working_directory: PathBuf,
    pub cache_directory: Option<PathBuf>,
    pub trace: bool,
    pub max_output_bytes: usize,
    /// Maximum entered iterations for the selected scope (per loop by default).
    pub max_loop_iterations: usize,
    /// Selects legacy run-wide accounting when replaying an older record.
    pub loop_budget_scope: LoopBudgetScope,
    /// Budget for each native pipeline group, finite stdin capture and
    /// cooperative checkpoints in an in-process applet. It cannot interrupt a
    /// synchronous OS or library call.
    pub command_timeout: Option<Duration>,
    /// Maximum lifetime of the full runtime operation, including scheduler waits.
    pub total_timeout: Option<Duration>,
}

impl RunOptions {
    /// Options for a run in `working_directory` with no inputs, cache, trace or
    /// timeouts, a 1 MiB output limit, and one million iterations per loop.
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            inputs: BTreeMap::new(),
            working_directory,
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024 * 1024,
            max_loop_iterations: loops::MAX_LOOP_ITERATIONS,
            loop_budget_scope: LoopBudgetScope::PerLoop,
            command_timeout: None,
            total_timeout: None,
        }
    }
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
    /// Child/helper process spawns that succeeded before the invocation
    /// failed. Processes are counted at spawn, even if later cancelled or
    /// reaped; their own descendants are outside this count.
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

/// Whether an error already reports an interrupt cancellation. Only these are
/// rewritten when a repeat interrupt lands during cleanup: native
/// `ExecutionKind::Cancelled` failures plus the plain-text cancellations the
/// task-group, parallel fan-out, in-process applet and model backends report
/// (`task group ...`, `parallel for each was cancelled ...`, `observed parent
/// cancellation`, `model request cancelled ...`). Timeouts, cleanup budgets
/// and ordinary step failures keep their own errors so they still unwind
/// through the cleanup frames.
fn is_cancellation_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<execution_result::ExecutionFailure>()
            .is_some_and(|failure| {
                failure.result.kind == execution_result::ExecutionKind::Cancelled
            })
    }) || error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("workflow cancelled")
            || message.contains("cancelled by handle")
            || message.contains("cancelled while")
            || message.contains("task group cancelled")
            || message.contains("was cancelled")
            || message.contains("observed parent cancellation")
            || message.contains("model request cancelled")
    })
}

/// A repeat interrupt landed while cleanup was active: the cleanup stops at
/// once and its running command is killed. The pending failure stays on the
/// error chain as context when the control-flow machinery records this.
fn cleanup_interrupted() -> anyhow::Error {
    anyhow!("cleanup interrupted")
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
        let maximum_ms = if maximum_ms < 0 {
            libc::c_int::MAX as u128
        } else {
            maximum_ms as u128
        };
        remaining_ms.min(maximum_ms) as libc::c_int
    })
}
pub struct RunResult {
    pub output: Vec<u8>,
    /// Runtime types of bindings retained by a successful finite run. Values are
    /// intentionally omitted so observers can inspect types without copying data.
    pub binding_types: BTreeMap<String, &'static str>,
    pub cache_hits: usize,
    /// Child/helper process spawns that succeeded during this run. This does
    /// not include descendants started internally by those processes.
    pub launched_processes: usize,
    pub executions: Vec<execution_result::ExecutionRecord>,
    pub dropped_executions: usize,
    pub dropped_stages: usize,
    /// Set when the program ended with `exit with status` or `fail with message`.
    /// `output` still holds the pipeline output at that point.
    pub exit: Option<ProgramExit>,
}

/// An explicit, deliberate end of a finite run. It is not an error: output is
/// still delivered, and the CLI uses `status` as the process exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramExit {
    /// 0-255. `exit with status` may use 0; `fail with message` uses 1-255.
    pub status: u8,
    /// Text written to stderr followed by a newline; set only by `fail`.
    pub message: Option<String>,
}

/// Evaluate an exit or fail step's operands without any effect. Shared by live
/// execution and checkpoint restore, which re-derives a completed exit.
fn program_exit(
    kind: &StepKind,
    bindings: &BTreeMap<String, Value>,
) -> Result<Option<ProgramExit>> {
    fn status(value: &Value, bindings: &BTreeMap<String, Value>, minimum: i64) -> Result<u8> {
        let Value::Integer(code) = resolve(value, bindings)? else {
            bail!("exit status must be an integer");
        };
        u8::try_from(code)
            .ok()
            .filter(|code| i64::from(*code) >= minimum)
            .with_context(|| {
                format!("exit status must be between {minimum} and 255, received {code}")
            })
    }
    Ok(match kind {
        StepKind::Exit { status: code } => Some(ProgramExit {
            status: status(code, bindings, 0)?,
            message: None,
        }),
        StepKind::Fail {
            message,
            status: code,
        } => {
            let Value::String(text) = resolve(message, bindings)? else {
                bail!("fail message must be text");
            };
            Some(ProgramExit {
                status: match code {
                    Some(code) => status(code, bindings, 1)?,
                    None => 1,
                },
                message: Some(crate::terminal_text::escape_controls(&text)),
            })
        }
        _ => None,
    })
}

#[cfg(test)]
mod program_exit_tests {
    use super::program_exit;
    use crate::ast::{StepKind, Value};
    use std::collections::BTreeMap;

    #[test]
    fn fail_messages_escape_terminal_controls() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "msg".to_owned(),
            Value::String("before\u{1b}[2J\u{9b}31m\nafter".to_owned()),
        );
        let exit = program_exit(
            &StepKind::Fail {
                message: Value::Reference("msg".to_owned()),
                status: None,
            },
            &bindings,
        )
        .unwrap()
        .expect("fail should produce an exit");
        assert_eq!(
            exit.message.as_deref(),
            Some("before\\u{1b}[2J\\u{9b}31m\nafter")
        );
    }
}

#[cfg(test)]
mod cleanup_interrupt_tests {
    use super::*;

    fn cancelled_failure(message: &str) -> anyhow::Error {
        anyhow::Error::new(execution_result::ExecutionFailure::new(
            execution_result::ExecutionResult::runtime_failure(
                execution_result::ExecutionKind::Cancelled,
                message,
                execution_result::EffectCertainty::Uncertain,
            ),
        ))
    }

    #[test]
    fn cleanup_interrupted_names_the_interruption() {
        assert!(
            format!("{:#}", cleanup_interrupted()).contains("cleanup interrupted"),
            "repeat interrupts must report the interruption, not the abandoned cleanup step"
        );
    }

    #[test]
    fn cancellation_errors_are_rewritten_and_others_are_not() {
        assert!(is_cancellation_error(&cancelled_failure(
            "workflow cancelled by signal"
        )));
        // The pending failure stays recognizable through added context.
        let chained = Err::<(), _>(cancelled_failure("workflow cancelled by signal"))
            .context("outer step")
            .unwrap_err();
        assert!(is_cancellation_error(&chained));
        assert!(is_cancellation_error(&anyhow::anyhow!(
            "cancelled by handle"
        )));
        assert!(is_cancellation_error(&anyhow::anyhow!(
            "cancelled while draining"
        )));
        // Task-group, parallel fan-out, in-process applet and model
        // cancellations unwound mid-statement must rewrite the same way.
        assert!(is_cancellation_error(&anyhow::anyhow!(
            "task group cancelled"
        )));
        assert!(is_cancellation_error(&anyhow::anyhow!(
            "task group was cancelled before preflight"
        )));
        assert!(is_cancellation_error(&anyhow::anyhow!(
            "parallel for each was cancelled before element launch"
        )));
        assert!(is_cancellation_error(&anyhow::anyhow!(
            "in-process applet observed parent cancellation"
        )));
        assert!(is_cancellation_error(&anyhow::anyhow!(
            "model request cancelled; its remote outcome may be uncertain"
        )));
        // Timeouts, cleanup budgets and ordinary step failures keep their own
        // errors so they still unwind through the cleanup frames.
        assert!(!is_cancellation_error(&anyhow::Error::new(
            execution_result::ExecutionFailure::new(
                execution_result::ExecutionResult::runtime_failure(
                    execution_result::ExecutionKind::Timeout,
                    "command timed out",
                    execution_result::EffectCertainty::Uncertain,
                ),
            )
        )));
        // A task-group error without a cancellation keeps its own error: the
        // match is on the cancellation, not on the subsystem name.
        assert!(!is_cancellation_error(&anyhow::anyhow!(
            "task group result metadata exceeds the configured output limit"
        )));
        assert!(!is_cancellation_error(&anyhow::Error::new(CommandTimedOut)));
        assert!(!is_cancellation_error(&anyhow::Error::new(
            control_flow::CleanupBudgetExceeded {
                after_success: false
            }
        )));
        assert!(!is_cancellation_error(&anyhow::anyhow!("boom")));
    }

    #[test]
    fn cleanup_scope_shares_the_second_interrupt_flag() {
        let signals =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(false)), false, None)
                .unwrap();
        let scope = signals.cleanup_scope(None);
        assert!(Arc::ptr_eq(&scope.cancelled, &signals.cleanup_cancel));
        assert!(Arc::ptr_eq(&scope.cleanup_cancel, &signals.cleanup_cancel));
        assert!(!scope.is_cancelled());
        signals.cleanup_cancel.store(true, Ordering::SeqCst);
        assert!(scope.is_cancelled());
    }

    #[test]
    fn entering_cleanup_does_not_inherit_the_first_interrupt() {
        let signals =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(true)), false, None)
                .unwrap();
        let scope = signals.cleanup_scope(None);
        assert!(
            !scope.is_cancelled(),
            "the first interrupt runs cleanup; only a repeat interrupt sets the shared flag"
        );
    }

    #[test]
    fn cleanup_scope_inherits_outer_cancellation_but_not_the_first_interrupt() {
        let mut signals =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(true)), false, None)
                .unwrap();
        let ancestor = Arc::new(AtomicBool::new(false));
        signals.outer_cancelled.push(Arc::clone(&ancestor));
        let scope = signals.cleanup_scope(None);
        assert!(
            !scope.is_cancelled(),
            "neither the first interrupt nor an unset ancestor cancels cleanup"
        );
        ancestor.store(true, Ordering::SeqCst);
        assert!(
            scope.is_cancelled(),
            "an ancestor task-group cancel during cleanup must abort it"
        );
    }

    #[test]
    fn cleanup_scope_inherits_job_cancellation() {
        let mut signals =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(false)), false, None)
                .unwrap();
        let job = Arc::new(AtomicBool::new(false));
        signals.job_cancelled = Some(Arc::clone(&job));
        let scope = signals.cleanup_scope(None);
        assert!(!scope.is_cancelled());
        job.store(true, Ordering::SeqCst);
        assert!(
            scope.is_cancelled(),
            "a job-handle cancel during cleanup must abort it"
        );
    }

    #[test]
    fn expired_drain_marks_cleanup_cancel_like_a_second_interrupt() {
        let drain = Arc::new(AtomicU8::new(crate::platform::DRAIN_REQUESTED));
        let cancelled = Arc::new(AtomicBool::new(false));
        let cleanup_cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        spawn_drain_watchdog(
            Arc::clone(&drain),
            Arc::clone(&cancelled),
            Arc::clone(&cleanup_cancel),
            Duration::from_millis(50),
            Arc::clone(&stop),
            None,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cleanup_cancel.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            cleanup_cancel.load(Ordering::SeqCst),
            "the expired drain never marked cleanup"
        );
        assert!(cancelled.load(Ordering::SeqCst));
        assert_eq!(
            drain.load(Ordering::SeqCst),
            crate::platform::DRAIN_CANCELLED
        );
        stop.store(true, Ordering::SeqCst);
    }
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
    /// Child/helper process spawns that succeeded during this invocation,
    /// including a supervised DNS resolver helper but excluding descendants
    /// started internally by those processes or workers reused from a pool.
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
        preflight_inputs_with_loop_limit(
            program,
            registry,
            &options.inputs,
            options.max_loop_iterations,
        )?;
        return Ok(Cow::Borrowed(options));
    }
    // Resolve supplied values once; cloning options first copied the complete
    // input tree only to discard that copy when replacing the inputs field.
    let configured = RunOptions {
        inputs: crate::inputs::resolve(&program.inputs, &options.inputs, false)?,
        cache_directory: options.cache_directory.clone(),
        trace: options.trace,
        max_output_bytes: options.max_output_bytes,
        max_loop_iterations: options.max_loop_iterations,
        loop_budget_scope: options.loop_budget_scope,
        command_timeout: options.command_timeout,
        total_timeout: options.total_timeout,
        ..RunOptions::new(options.working_directory.clone())
    };
    preflight_inputs_with_loop_limit(
        program,
        registry,
        &configured.inputs,
        configured.max_loop_iterations,
    )?;
    Ok(Cow::Owned(configured))
}

/// The two signatures of a program's inputs. New checkpoints record the keyed form, which
/// only the per-user key can reproduce. The legacy unkeyed digest exists only to recognize
/// journals that were written before the switch.
struct InputSignatures {
    keyed: String,
    legacy: String,
}

/// Where the per-user input key comes from when a checkpoint's input signature is
/// computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InputKey {
    /// A fresh checkpoint, or a legacy journal being upgraded: create the key if needed.
    CreateIfMissing,
    /// A journal that already records keyed inputs: only the key that wrote it can
    /// match, so a missing key is an error naming its path, never a new key.
    Existing,
}

fn input_signature(
    program: &Program,
    values: &BTreeMap<String, Value>,
    key: InputKey,
) -> Result<Option<InputSignatures>> {
    if program.inputs.is_empty() {
        return Ok(None);
    }
    // The digest is keyed with the per-user key. An unkeyed hash of input values could be
    // guessed offline from the checkpoint's own journal, so every input is covered by the key.
    let key = match key {
        InputKey::CreateIfMissing => crate::agent::what_if_key::load_or_create(),
        InputKey::Existing => crate::agent::what_if_key::load_existing(),
    }
    .context("a checkpoint with inputs needs the per-user key")?;
    let signature = program
        .inputs
        .iter()
        .map(|d| {
            let value = values.get(&d.name);
            let secret = matches!(
                d.source,
                Some(crate::ast::InputSource::Prompt { secret: true, .. })
            );
            let shown = match (secret, value) {
                (true, Some(value)) => {
                    let mut message = d.name.as_bytes().to_vec();
                    message.push(0);
                    message.extend(serde_json::to_vec(value)?);
                    serde_json::json!({
                        "secret_hmac_sha256": crate::agent::what_if_key::hmac_sha256(&key, &message)
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                    })
                }
                (_, Some(value)) => serde_json::to_value(value)?,
                (_, None) => serde_json::Value::Null,
            };
            Ok((d.name.clone(), (d.kind, shown)))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let plain = serde_json::to_vec(&signature)?;
    let keyed = crate::agent::what_if_key::hmac_sha256(&key, &plain);
    Ok(Some(InputSignatures {
        keyed: format!(
            "inputs-v2:{}",
            keyed
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
        legacy: format!("inputs-v1:{:x}", Sha256::digest(&plain)),
    }))
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
    /// Interrupt drain state (`platform::DRAIN_*`); shared with cleanup scopes.
    drain: Arc<AtomicU8>,
    /// Set by a repeat interrupt (or an expired interrupt drain) while cleanup
    /// may be active. Cleanup scopes share this flag instead of minting a fresh
    /// one, so a second Ctrl-C aborts the cleanup at once and kills its running
    /// command. The first interrupt never sets it, so cleanup still runs.
    cleanup_cancel: Arc<AtomicBool>,
    /// Signal number of the interrupt that cancelled this run; 0 when none.
    /// Each run owns its slot, so overlapping signal-owning runs keep their
    /// own exit codes and runs that never install handlers cannot inherit a
    /// previous run's code. Only entry points without a run (the CLI error
    /// reporter) read the process-wide fallback in `platform`. OS delivery is
    /// broadcast: one process-wide signal runs every installed owner's
    /// handler set, so overlapping owners each record that same signal while
    /// runs without handlers stay clean.
    cancel_signal: Arc<AtomicI32>,
    /// Stops this run's drain watchdog when the run's signals are dropped.
    drain_stop: Option<Arc<AtomicBool>>,
    registrations: Vec<signal_hook::SigId>,
    capture_diagnostics: bool,
    deadline: Option<Instant>,
    graph_service_cancellation: bool,
    background_jobs_allowed: bool,
    program_modules: Arc<Mutex<Option<Arc<crate::modules::ProgramModuleSession>>>>,
    sensitivity_floor: Arc<OnceLock<crate::security::SensitivitySet>>,
    process_launches: Arc<AtomicUsize>,
    process_observer: Arc<OnceLock<ProcessObserver>>,
    worker_pool: Option<Arc<worker_backend::WorkerPool>>,
    outer_cancelled: Vec<Arc<AtomicBool>>,
    cancel_wakes: Vec<Arc<crate::tasks::CancellationWake>>,
    /// True when a cancellation flag can be changed by a caller that cannot
    /// signal these wake descriptors (for example, an externally owned Arc).
    unnotified_cancellation: bool,
    job_cancelled: Option<Arc<AtomicBool>>,
    job_launch_ack: Option<Arc<Mutex<Option<jobs::LaunchAcknowledger>>>>,
    job_output_budget: Option<Arc<AtomicUsize>>,
    job_output_limit: Option<usize>,
    group_diagnostic_budget: Option<(Arc<AtomicUsize>, usize)>,
    prepared_job_commands: Option<Arc<PreparedJobCommands>>,
    job_diagnostic_truncation: Option<Arc<Mutex<Vec<bool>>>>,
    job_stage_results: Option<Arc<Mutex<Vec<Option<execution_result::StageResult>>>>>,
    job_run_context: Arc<Mutex<Option<JobRunFrame>>>,
    output_writers: Arc<stdio::OutputWriterPool>,
    #[cfg(test)]
    job_panic_before_ack: Option<Arc<JobAckPanicProbe>>,
}

pub(super) struct JobRunFrame {
    depth: usize,
    budget: Arc<jobs::RunBudget>,
}

pub(crate) struct JobRunGuard {
    context: Arc<Mutex<Option<JobRunFrame>>>,
}

impl Drop for JobRunGuard {
    fn drop(&mut self) {
        if let Ok(mut context) = self.context.lock() {
            if let Some(frame) = context.as_mut() {
                frame.depth = frame.depth.saturating_sub(1);
                if frame.depth == 0 {
                    *context = None;
                }
            }
        }
    }
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct JobAckPanicProbe {
    panic_next: AtomicBool,
    launched_pids: Mutex<Vec<u32>>,
}

/// Immutable execution state inherited by nested applet invocations.
/// Process launch counters remain local to each invocation; only the observer
/// and run-wide job budget are shared across concurrent children.
#[derive(Clone)]
pub(crate) struct AppletInvocationContext {
    pub(crate) module_session: Arc<crate::modules::ProgramModuleSession>,
    pub(crate) sensitivity_floor: crate::security::SensitivitySet,
    pub(crate) worker_pool: Option<Arc<worker_backend::WorkerPool>>,
    pub(crate) process_observer: Option<ProcessObserver>,
    pub(crate) job_run_context: Arc<Mutex<Option<JobRunFrame>>>,
    pub(crate) background_jobs_allowed: bool,
    /// Cancellation inherited from enclosing invocations. Shared flags let
    /// nested task children observe cancellation from every ancestor without a
    /// polling bridge thread.
    pub(crate) outer_cancelled: Vec<Arc<AtomicBool>>,
    /// One-shot event descriptors for every enclosing task group. Keeping the
    /// full chain lets native supervisors wake immediately on ancestor cancel.
    pub(crate) cancel_wakes: Vec<Arc<crate::tasks::CancellationWake>>,
    pub(crate) unnotified_cancellation: bool,
    /// Absolute deadline inherited by a nested invocation.
    pub(crate) deadline: Option<Instant>,
    /// A task group's shared stdout budget and its limit: native output is charged
    /// against it as it arrives, so the group stops at its aggregate limit.
    pub(crate) group_output_budget: Option<(Arc<AtomicUsize>, usize)>,
}

impl AppletInvocationContext {
    pub(crate) fn new(
        module_session: Arc<crate::modules::ProgramModuleSession>,
        sensitivity_floor: crate::security::SensitivitySet,
    ) -> Self {
        Self {
            module_session,
            sensitivity_floor,
            worker_pool: None,
            process_observer: None,
            job_run_context: Arc::new(Mutex::new(None)),
            background_jobs_allowed: true,
            outer_cancelled: Vec::new(),
            cancel_wakes: Vec::new(),
            unnotified_cancellation: false,
            deadline: None,
            group_output_budget: None,
        }
    }

    pub(crate) fn from_signals(signals: &Signals) -> Result<Self> {
        Ok(Self {
            module_session: current_module_session(signals)?,
            sensitivity_floor: signals.sensitivity_floor(),
            worker_pool: signals.worker_pool.clone(),
            process_observer: signals.process_observer.get().cloned(),
            job_run_context: Arc::clone(&signals.job_run_context),
            background_jobs_allowed: signals.background_jobs_allowed,
            outer_cancelled: signals.outer_cancelled.clone(),
            cancel_wakes: signals.cancel_wakes.clone(),
            unnotified_cancellation: signals.unnotified_cancellation,
            deadline: signals.deadline,
            group_output_budget: None,
        })
    }
}

impl Signals {
    pub(crate) fn register_with_deadline(deadline: Option<Instant>) -> Result<Self> {
        Self::register(deadline, None)
    }

    /// Like `register_with_deadline`, but the first interrupt drains the run rather
    /// than cancelling it, for at most `drain_budget`; a second interrupt cancels.
    pub(crate) fn register_with_drain(
        deadline: Option<Instant>,
        drain_budget: Duration,
    ) -> Result<Self> {
        Self::register(deadline, Some(drain_budget))
    }

    fn register(deadline: Option<Instant>, drain: Option<Duration>) -> Result<Self> {
        // A fresh per-run slot starts at zero, so a new signal-owned run can
        // neither inherit nor erase another run's recorded interrupt.
        let cleanup_cancel = Arc::new(AtomicBool::new(false));
        let cancel_signal = Arc::new(AtomicI32::new(0));
        let mut guard = Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            drain: Arc::new(AtomicU8::new(crate::platform::DRAIN_IDLE)),
            cleanup_cancel: Arc::clone(&cleanup_cancel),
            cancel_signal: Arc::clone(&cancel_signal),
            drain_stop: None,
            registrations: Vec::with_capacity(3),
            capture_diagnostics: false,
            deadline,
            graph_service_cancellation: false,
            background_jobs_allowed: true,
            program_modules: Arc::new(Mutex::new(None)),
            sensitivity_floor: Arc::new(OnceLock::new()),
            process_launches: Arc::new(AtomicUsize::new(0)),
            process_observer: Arc::new(OnceLock::new()),
            worker_pool: None,
            outer_cancelled: Vec::new(),
            cancel_wakes: Vec::new(),
            unnotified_cancellation: false,
            job_cancelled: None,
            job_launch_ack: None,
            job_output_budget: None,
            job_output_limit: None,
            group_diagnostic_budget: None,
            prepared_job_commands: None,
            job_diagnostic_truncation: None,
            job_stage_results: None,
            job_run_context: Arc::new(Mutex::new(None)),
            output_writers: Arc::new(stdio::OutputWriterPool::new()),
            #[cfg(test)]
            job_panic_before_ack: None,
        };
        let signal_wake = Arc::new(crate::tasks::CancellationWake::new()?);
        let armed = match drain {
            Some(budget) => {
                let stop = Arc::new(AtomicBool::new(false));
                spawn_drain_watchdog(
                    Arc::clone(&guard.drain),
                    Arc::clone(&guard.cancelled),
                    Arc::clone(&guard.cleanup_cancel),
                    budget,
                    Arc::clone(&stop),
                    Some(Arc::clone(&signal_wake)),
                )?;
                guard.drain_stop = Some(stop);
                Some(Arc::clone(&guard.drain))
            }
            None => None,
        };
        guard.cancel_wakes.push(Arc::clone(&signal_wake));
        guard.registrations = crate::platform::signals(
            guard.cancelled.clone(),
            armed,
            Some(signal_wake),
            Arc::clone(&guard.cleanup_cancel),
            Arc::clone(&guard.cancel_signal),
        )?;
        Ok(guard)
    }

    pub(crate) fn register_with_cancellation_and_deadline(
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        // As with `register`: the fresh per-run slot starts without a recorded
        // interrupt, so this run can neither inherit nor erase another run's.
        // The caller can set this flag directly without notifying the signal
        // wake, so native supervisors retain the bounded cancellation check.
        let mut guard = Self::with_cancellation_and_deadline(cancelled, false, deadline)?;
        let signal_wake = Arc::new(crate::tasks::CancellationWake::new()?);
        guard.cancel_wakes.push(Arc::clone(&signal_wake));
        guard.registrations = crate::platform::signals(
            guard.cancelled.clone(),
            None,
            Some(signal_wake),
            Arc::clone(&guard.cleanup_cancel),
            Arc::clone(&guard.cancel_signal),
        )?;
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
            drain: Arc::new(AtomicU8::new(crate::platform::DRAIN_IDLE)),
            cleanup_cancel: Arc::new(AtomicBool::new(false)),
            cancel_signal: Arc::new(AtomicI32::new(0)),
            drain_stop: None,
            registrations: Vec::new(),
            capture_diagnostics,
            deadline,
            graph_service_cancellation: false,
            background_jobs_allowed: true,
            program_modules: Arc::new(Mutex::new(None)),
            sensitivity_floor: Arc::new(OnceLock::new()),
            process_launches: Arc::new(AtomicUsize::new(0)),
            process_observer: Arc::new(OnceLock::new()),
            worker_pool: None,
            outer_cancelled: Vec::new(),
            cancel_wakes: Vec::new(),
            unnotified_cancellation: true,
            job_cancelled: None,
            job_launch_ack: None,
            job_output_budget: None,
            job_output_limit: None,
            group_diagnostic_budget: None,
            prepared_job_commands: None,
            job_diagnostic_truncation: None,
            job_stage_results: None,
            job_run_context: Arc::new(Mutex::new(None)),
            output_writers: Arc::new(stdio::OutputWriterPool::new()),
            #[cfg(test)]
            job_panic_before_ack: None,
        })
    }

    /// Fork the execution context for bounded finally cleanup. Cleanup keeps
    /// the shared second-interrupt flag and wakes so a repeat interrupt aborts
    /// it at once, with a fresh deadline, while retaining its enclosing
    /// module, worker, provenance, accounting, observation, and job-run scope.
    /// Ancestor task-group (`outer_cancelled`) and job-handle (`job_cancelled`)
    /// flags are inherited so an outer or job cancel during cleanup still
    /// aborts it; the run's own first-interrupt flag is deliberately not
    /// inherited, so one interrupt still runs cleanup to completion.
    fn cleanup_scope(&self, deadline: Option<Instant>) -> Self {
        Self {
            cancelled: Arc::clone(&self.cleanup_cancel),
            drain: Arc::clone(&self.drain),
            cleanup_cancel: Arc::clone(&self.cleanup_cancel),
            cancel_signal: Arc::clone(&self.cancel_signal),
            drain_stop: None,
            registrations: Vec::new(),
            capture_diagnostics: self.capture_diagnostics,
            deadline,
            graph_service_cancellation: self.graph_service_cancellation,
            background_jobs_allowed: self.background_jobs_allowed,
            program_modules: Arc::clone(&self.program_modules),
            sensitivity_floor: Arc::clone(&self.sensitivity_floor),
            process_launches: Arc::clone(&self.process_launches),
            process_observer: Arc::clone(&self.process_observer),
            worker_pool: self.worker_pool.clone(),
            outer_cancelled: self.outer_cancelled.clone(),
            cancel_wakes: self.cancel_wakes.clone(),
            unnotified_cancellation: self.unnotified_cancellation,
            job_cancelled: self.job_cancelled.clone(),
            job_launch_ack: None,
            job_output_budget: None,
            job_output_limit: None,
            group_diagnostic_budget: None,
            prepared_job_commands: None,
            job_diagnostic_truncation: None,
            job_stage_results: None,
            job_run_context: Arc::clone(&self.job_run_context),
            output_writers: Arc::clone(&self.output_writers),
            #[cfg(test)]
            job_panic_before_ack: None,
        }
    }

    /// A module may replace intermediate output before returning. Keep the
    /// parent group's capture budget for the final module result, not for its
    /// internal commands; those retain their own per-invocation limit.
    fn module_capture_scope(&self) -> Option<Self> {
        let (group_budget, _) = self.group_diagnostic_budget.as_ref()?;
        let group_owns_stdout = self
            .job_output_budget
            .as_ref()
            .is_some_and(|budget| Arc::ptr_eq(budget, group_budget));
        Some(Self {
            cancelled: Arc::clone(&self.cancelled),
            drain: Arc::clone(&self.drain),
            cleanup_cancel: Arc::clone(&self.cleanup_cancel),
            cancel_signal: Arc::clone(&self.cancel_signal),
            drain_stop: None,
            registrations: Vec::new(),
            capture_diagnostics: self.capture_diagnostics,
            deadline: self.deadline,
            graph_service_cancellation: self.graph_service_cancellation,
            background_jobs_allowed: self.background_jobs_allowed,
            program_modules: Arc::clone(&self.program_modules),
            sensitivity_floor: Arc::clone(&self.sensitivity_floor),
            process_launches: Arc::clone(&self.process_launches),
            process_observer: Arc::clone(&self.process_observer),
            worker_pool: self.worker_pool.clone(),
            outer_cancelled: self.outer_cancelled.clone(),
            cancel_wakes: self.cancel_wakes.clone(),
            unnotified_cancellation: self.unnotified_cancellation,
            job_cancelled: self.job_cancelled.clone(),
            job_launch_ack: self.job_launch_ack.clone(),
            job_output_budget: if group_owns_stdout {
                None
            } else {
                self.job_output_budget.clone()
            },
            job_output_limit: if group_owns_stdout {
                None
            } else {
                self.job_output_limit
            },
            group_diagnostic_budget: None,
            prepared_job_commands: self.prepared_job_commands.clone(),
            job_diagnostic_truncation: self.job_diagnostic_truncation.clone(),
            job_stage_results: self.job_stage_results.clone(),
            job_run_context: Arc::clone(&self.job_run_context),
            output_writers: Arc::clone(&self.output_writers),
            #[cfg(test)]
            job_panic_before_ack: self.job_panic_before_ack.clone(),
        })
    }

    pub(crate) fn inherit_applet_context(
        &mut self,
        context: &AppletInvocationContext,
    ) -> Result<()> {
        self.set_program_module_session(Arc::clone(&context.module_session))?;
        self.set_sensitivity_floor(context.sensitivity_floor.clone());
        self.worker_pool = context.worker_pool.clone();
        self.job_run_context = Arc::clone(&context.job_run_context);
        self.background_jobs_allowed = context.background_jobs_allowed;
        for cancelled in &context.outer_cancelled {
            if !self
                .outer_cancelled
                .iter()
                .any(|inherited| Arc::ptr_eq(inherited, cancelled))
            {
                self.outer_cancelled.push(Arc::clone(cancelled));
            }
        }
        for wake in &context.cancel_wakes {
            if !self
                .cancel_wakes
                .iter()
                .any(|inherited| Arc::ptr_eq(inherited, wake))
            {
                self.cancel_wakes.push(Arc::clone(wake));
            }
        }
        self.unnotified_cancellation =
            context.unnotified_cancellation || context.cancel_wakes.is_empty();
        if let Some((budget, limit)) = &context.group_output_budget {
            self.group_diagnostic_budget = Some((Arc::clone(budget), *limit));
            if self.job_output_budget.is_none() {
                self.job_output_budget = Some(Arc::clone(budget));
                self.job_output_limit = Some(*limit);
            }
        }
        if let Some(observer) = context.process_observer.clone() {
            self.set_process_observer(observer)?;
        }
        Ok(())
    }

    pub(crate) fn begin_job_run(&self, output_limit: usize) -> Result<JobRunGuard> {
        let mut context = self
            .job_run_context
            .lock()
            .map_err(|_| anyhow!("background job run context is poisoned"))?;
        match context.as_mut() {
            Some(frame) => {
                frame.depth = frame
                    .depth
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("background job run nesting limit exceeded"))?;
            }
            None => {
                *context = Some(JobRunFrame {
                    depth: 1,
                    budget: Arc::new(jobs::RunBudget::new(output_limit)),
                });
            }
        }
        Ok(JobRunGuard {
            context: Arc::clone(&self.job_run_context),
        })
    }

    #[cfg(feature = "live")]
    fn begin_job_run_with_budget(&self, budget: Arc<jobs::RunBudget>) -> Result<JobRunGuard> {
        let mut context = self
            .job_run_context
            .lock()
            .map_err(|_| anyhow!("background job run context is poisoned"))?;
        match context.as_mut() {
            Some(frame) => {
                if !Arc::ptr_eq(&frame.budget, &budget) {
                    bail!("live background jobs cannot change their run-wide budget");
                }
                frame.depth = frame
                    .depth
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("background job run nesting limit exceeded"))?;
            }
            None => {
                *context = Some(JobRunFrame { depth: 1, budget });
            }
        }
        Ok(JobRunGuard {
            context: Arc::clone(&self.job_run_context),
        })
    }

    fn current_job_run_budget(&self) -> Option<Arc<jobs::RunBudget>> {
        self.job_run_context
            .lock()
            .ok()?
            .as_ref()
            .map(|frame| Arc::clone(&frame.budget))
    }

    /// True while an `on interrupt` body should run: a drain is requested or under way.
    /// A second interrupt cancels and turns this off, so a hard cancel skips the body.
    fn drain_body_runs(&self) -> bool {
        matches!(
            self.drain.load(Ordering::SeqCst),
            crate::platform::DRAIN_REQUESTED | crate::platform::DRAIN_STARTED
        )
    }

    /// Starts a requested drain: true exactly once, at a statement boundary.
    fn begin_drain(&self) -> bool {
        self.drain
            .compare_exchange(
                crate::platform::DRAIN_REQUESTED,
                crate::platform::DRAIN_STARTED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Record the signal that interrupted this run. Production signal handlers
    /// store into both this slot and the process-wide fallback; tests use this
    /// to record against one run without delivering a real signal.
    #[cfg(test)]
    fn record_cancel_signal(&self, signal: i32) {
        self.cancel_signal.store(signal, Ordering::SeqCst);
    }

    /// Exit status for this run when cancelled by an interrupt: 128 plus the
    /// recorded signal. Runs that never recorded a signal report `None`, even
    /// when another run in this process was interrupted.
    pub(crate) fn cancel_exit_code(&self) -> Option<i32> {
        match self.cancel_signal.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(crate::platform::exit_code_for_signal(signal)),
        }
    }

    /// Exit status of this run ended by an interrupt drain: 128 plus the
    /// recorded signal (130 for SIGINT), or 130 when no signal was recorded.
    pub(crate) fn drain_exit_status(&self) -> u8 {
        self.cancel_exit_code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(130)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
            || self
                .outer_cancelled
                .iter()
                .any(|cancelled| cancelled.load(Ordering::Relaxed))
            || self
                .job_cancelled
                .as_ref()
                .is_some_and(|cancelled| cancelled.load(Ordering::Relaxed))
    }

    fn reserve_job_output(&self, requested: usize) -> usize {
        let (Some(budget), Some(limit)) = (&self.job_output_budget, self.job_output_limit) else {
            return requested;
        };
        let mut retained = budget.load(Ordering::Acquire);
        loop {
            let available = limit.saturating_sub(retained);
            let reserved = requested.min(available);
            match budget.compare_exchange_weak(
                retained,
                retained.saturating_add(reserved),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return reserved,
                Err(observed) => retained = observed,
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn acknowledge_job_launch(
        &self,
        calls: &[(&Invocation, &Manifest, usize)],
        children: &Children,
    ) -> Result<()> {
        #[cfg(test)]
        if let Some(probe) = &self.job_panic_before_ack {
            if probe.panic_next.swap(false, Ordering::AcqRel) {
                let pids = children
                    .0
                    .iter()
                    .map(|child| child.child.id())
                    .collect::<Vec<_>>();
                if let Ok(mut launched_pids) = probe.launched_pids.lock() {
                    launched_pids.extend(pids);
                }
                panic!("test injected panic before job launch acknowledgment");
            }
        }
        if self.job_launch_ack.is_some() && children.0.len() != calls.len() {
            bail!(
                "cannot acknowledge background job start: created {} of {} pipeline stages",
                children.0.len(),
                calls.len()
            );
        }
        let ack = self
            .job_launch_ack
            .as_ref()
            .and_then(|ack| ack.lock().ok()?.take());
        let Some(ack) = ack else {
            return Ok(());
        };
        let stages = calls
            .iter()
            .zip(&children.0)
            .map(|((call, manifest, line), child)| jobs::StageLaunch {
                line: *line,
                applet: manifest.name_for(call).to_owned(),
                pid: child.child.id(),
            })
            .collect();
        ack.send_started(stages)?;
        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn record_job_stage_launch(&self, stage: jobs::StageLaunch) {
        if let Some(ack) = self
            .job_launch_ack
            .as_ref()
            .and_then(|ack| ack.lock().ok()?.as_ref().cloned())
        {
            ack.record_stage(stage);
        }
    }

    fn record_job_stage_result(&self, index: usize, stage: &execution_result::StageResult) {
        if let Some(results) = &self.job_stage_results {
            if let Ok(mut results) = results.lock() {
                if let Some(target) = results.get_mut(index) {
                    *target = Some(stage.clone());
                }
            }
        }
    }

    fn fail_job_launch(&self, failure: jobs::LaunchFailure) {
        if let Some(ack) = self
            .job_launch_ack
            .as_ref()
            .and_then(|ack| ack.lock().ok()?.take())
        {
            ack.send_failed(failure);
        }
    }

    fn set_sensitivity_floor(&self, sensitivity: crate::security::SensitivitySet) {
        let _ = self.sensitivity_floor.set(sensitivity);
    }

    pub(crate) fn set_program_module_session(
        &self,
        session: Arc<crate::modules::ProgramModuleSession>,
    ) -> Result<()> {
        let mut slot = self
            .program_modules
            .lock()
            .map_err(|_| anyhow!("program module session lock is poisoned"))?;
        if let Some(existing) = slot.as_ref() {
            if !Arc::ptr_eq(existing, &session) {
                bail!("program module session was already initialized for this execution");
            }
            return Ok(());
        }
        *slot = Some(session);
        Ok(())
    }

    fn sensitivity_floor(&self) -> crate::security::SensitivitySet {
        self.sensitivity_floor.get().cloned().unwrap_or_default()
    }

    pub(crate) fn set_process_observer(&self, observer: ProcessObserver) -> Result<()> {
        self.process_observer
            .set(observer)
            .map_err(|_| anyhow!("process observer was already initialized for this execution"))
    }

    fn record_process_launch(&self) {
        self.account_process_launches(1);
    }

    fn account_process_launches(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.process_launches.fetch_add(count, Ordering::Relaxed);
        if let Some(observer) = self.process_observer.get() {
            observer(count);
        }
    }

    fn account_child_process_launches(&self, count: usize) {
        if count > 0 {
            self.process_launches.fetch_add(count, Ordering::Relaxed);
        }
    }

    fn process_launch_count(&self) -> usize {
        self.process_launches.load(Ordering::Relaxed)
    }

    #[cfg(feature = "remote")]
    fn process_launch_callback(&self) -> ProcessObserver {
        let process_launches = Arc::clone(&self.process_launches);
        let process_observer = Arc::clone(&self.process_observer);
        Arc::new(move |count| {
            if count == 0 {
                return;
            }
            process_launches.fetch_add(count, Ordering::Relaxed);
            if let Some(observer) = process_observer.get() {
                observer(count);
            }
        })
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
        if let Some(stop) = &self.drain_stop {
            stop.store(true, Ordering::SeqCst);
        }
    }
}

/// The budget an interrupt drain gets before it cancels, unless the caller sets one.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Decides when a drain has lasted its budget. The state is polled, so the clock
/// starts at the first poll that sees a requested drain.
struct DrainClock {
    budget: Duration,
    since: Option<Instant>,
}

impl DrainClock {
    fn new(budget: Duration) -> Self {
        Self {
            budget,
            since: None,
        }
    }

    /// True once a requested or started drain has lasted more than the budget.
    /// Idle and cancelled states never expire, and they reset the clock.
    fn expired(&mut self, state: u8, now: Instant) -> bool {
        match state {
            crate::platform::DRAIN_REQUESTED | crate::platform::DRAIN_STARTED => {
                let since = *self.since.get_or_insert(now);
                now.saturating_duration_since(since) >= self.budget
            }
            _ => {
                self.since = None;
                false
            }
        }
    }
}

/// Polls a run's drain state and turns an expired drain into a cancel: the same
/// state change and flags a second interrupt makes. It exits once the run cancels
/// or its signals are dropped.
fn spawn_drain_watchdog(
    drain: Arc<AtomicU8>,
    cancelled: Arc<AtomicBool>,
    cleanup_cancel: Arc<AtomicBool>,
    budget: Duration,
    stop: Arc<AtomicBool>,
    wake: Option<Arc<crate::tasks::CancellationWake>>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("execute-drain-watchdog".into())
        .spawn(move || {
            let mut clock = DrainClock::new(budget);
            while !stop.load(Ordering::SeqCst) {
                let state = drain.load(Ordering::SeqCst);
                if state == crate::platform::DRAIN_CANCELLED {
                    return;
                }
                if clock.expired(state, Instant::now()) {
                    drain.store(crate::platform::DRAIN_CANCELLED, Ordering::SeqCst);
                    cancelled.store(true, Ordering::SeqCst);
                    cleanup_cancel.store(true, Ordering::SeqCst);
                    if let Some(wake) = &wake {
                        wake.notify();
                    }
                    return;
                }
                // 50 ms keeps an idle run with an `on interrupt` body to 20 wakeups a second;
                // a 10 s default budget needs no finer resolution.
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .context("starting the interrupt drain watchdog")?;
    Ok(())
}
pub(crate) fn binary_path(binary: &str, cwd: &Path) -> Result<PathBuf> {
    binary_path_with_path(
        binary,
        cwd,
        crate::capabilities::command_overlay_path().as_deref(),
    )
}

/// Resolve an executable using a caller-supplied PATH when present. This keeps
/// static checks and runtime launch resolution on the same search path.
///
/// A manifest-selected executable decides what runs, so the selected
/// candidate passes the same trusted-input ownership and writability gate as
/// manifests (leaf plus ancestors, hardlink/setuid, ACLs): a trusted manifest
/// naming a writable binary is refused instead of launched. The canonical
/// target is verified but the selected spelling is returned, so multicall
/// tools and symlink aliases keep dispatching on argv[0]. A replacement
/// between this verification and the later spawn remains a race; see
/// SECURITY.md.
pub(crate) fn binary_path_with_path(
    binary: &str,
    cwd: &Path,
    path_override: Option<&OsStr>,
) -> Result<PathBuf> {
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
        verify_trusted_executable(&candidate)?;
        Ok(candidate)
    } else {
        let path = path_override
            .map(OsStr::to_os_string)
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        binary_path_from_path(binary, cwd, &path)
    }
}

/// Verify a manifest-selected executable against the trusted-input gate. The
/// canonical target passes the leaf plus ancestor, hardlink/setuid and ACL
/// checks; the caller's spelling is kept for launch. Failures name the
/// selected path and fail closed instead of falling through to another PATH
/// entry: the first executable hit is what would run.
fn verify_trusted_executable(candidate: &Path) -> Result<()> {
    let canonical = std::fs::canonicalize(candidate)
        .with_context(|| format!("resolving executable {}", candidate.display()))?;
    crate::security::verify_trusted_input_file(&canonical, "executable")
        .with_context(|| format!("trusting executable {}", candidate.display()))?;
    Ok(())
}

/// Locate the version-matched generic applet helper beside this executable.
/// The helper decides what the `@execute-applet` applets run, so it passes
/// the same trusted-input ownership and writability gate as manifests,
/// including the writable-ancestor, hardlink and setuid checks: a writable
/// helper parent would let an attacker replace the verified binary before it
/// is executed. The path-side check cannot pin the inode through `exec`, so a
/// replacement between this verification and the later spawn remains a race;
/// see SECURITY.md.
pub fn bundled_applet_helper() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locating the running interpreter")?;
    let helper = executable.with_file_name(if cfg!(windows) {
        "execute-applet.exe"
    } else {
        "execute-applet"
    });
    if fs::symlink_metadata(&helper).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("bundled execute-applet helper is a symbolic link; pass the real path");
    }
    let metadata = crate::security::verify_trusted_input_file(
        &helper,
        "bundled execute-applet helper",
    )
    .with_context(|| {
        "bundled execute-applet helper missing or untrusted; build the interpreter and helper binaries with cargo build --release"
    })?;
    if !metadata.is_file() || !crate::platform::executable(&metadata) {
        bail!("bundled execute-applet helper is not an executable file");
    }
    Ok(helper)
}

/// Locate the version-matched native model applet beside this executable.
/// The helper passes the same trusted-input gate as the applet helper,
/// including the writable-parent check; the `exec` race note above applies.
pub fn bundled_model_applet() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locating the running interpreter")?;
    let helper = executable.with_file_name(if cfg!(windows) {
        "execute-model.exe"
    } else {
        "execute-model"
    });
    if fs::symlink_metadata(&helper).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("bundled execute-model helper is a symbolic link; pass the real path");
    }
    let metadata = crate::security::verify_trusted_input_file(&helper, "bundled execute-model helper")
        .with_context(|| {
        "bundled execute-model helper missing or untrusted; build both binaries with cargo build --release"
    })?;
    if !metadata.is_file() || !crate::platform::executable(&metadata) {
        bail!("bundled execute-model helper is not an executable file");
    }
    Ok(helper)
}

fn binary_path_from_path(binary: &str, cwd: &Path, path: &std::ffi::OsStr) -> Result<PathBuf> {
    if path.is_empty() {
        bail!("executable {binary:?} not found in PATH");
    }
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
            verify_trusted_executable(&candidate)?;
            return Ok(candidate);
        }
    }
    Err(anyhow!("executable {binary:?} not found in PATH"))
}

/// Cache only PATH parsing, never an executable or its metadata. Registry host state
/// scopes this lazy cache to the caller without adding process-global state.
#[derive(Default)]
struct LaunchPathCache {
    key: Option<(OsString, PathBuf)>,
    entries: Arc<[PathBuf]>,
}

impl LaunchPathCache {
    fn components(&mut self, cwd: &Path, path: &OsStr) -> Arc<[PathBuf]> {
        if !self.key.as_ref().is_some_and(|(saved_path, saved_cwd)| {
            saved_path == path && saved_cwd.as_os_str() == cwd.as_os_str()
        }) {
            self.entries = if path.is_empty() {
                Vec::new().into()
            } else {
                std::env::split_paths(path)
                    .map(|entry| cwd.join(entry))
                    .collect::<Vec<_>>()
                    .into()
            };
            self.key = Some((path.to_owned(), cwd.to_path_buf()));
        }
        Arc::clone(&self.entries)
    }

    #[cfg(test)]
    fn lookup(&mut self, binary: &str, cwd: &Path, path: &OsStr) -> Result<PathBuf> {
        binary_path_in_components(binary, &self.components(cwd, path))
    }
}

fn binary_path_in_components(binary: &str, entries: &[PathBuf]) -> Result<PathBuf> {
    if entries.is_empty() {
        bail!("executable {binary:?} not found in PATH");
    }
    let mut candidate = PathBuf::new();
    for entry in entries {
        candidate.clear();
        candidate.push(entry);
        candidate.push(binary);
        candidate = crate::platform::executable_path(candidate)?;
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file() && crate::platform::executable(&metadata) {
            verify_trusted_executable(&candidate)?;
            return Ok(candidate);
        }
    }
    Err(anyhow!("executable {binary:?} not found in PATH"))
}

fn binary_path_from_cached_path(
    binary: &str,
    cwd: &Path,
    path: &OsStr,
    registry: &Registry,
) -> Result<PathBuf> {
    let entries = registry
        .host_state::<Mutex<LaunchPathCache>>()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .components(cwd, path);
    // Release the cache lock before filesystem I/O, so parallel jobs can resolve
    // executables concurrently, including runs with different PATH/cwd keys.
    binary_path_in_components(binary, &entries)
}

fn binary_path_for_launch(binary: &str, cwd: &Path, registry: &Registry) -> Result<PathBuf> {
    if matches!(binary, "@execute-model" | "@execute-applet")
        || binary.contains('/')
        || (cfg!(windows) && binary.contains('\\'))
        || Path::new(binary).is_absolute()
    {
        return binary_path(binary, cwd);
    }
    let path = crate::capabilities::command_overlay_path()
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    if path.is_empty() {
        bail!("executable {binary:?} not found in PATH");
    }
    binary_path_from_cached_path(binary, cwd, &path, registry)
}
fn event(enabled: bool, mut value: serde_json::Value, signals: &Signals) -> Result<()> {
    if enabled {
        if let (Some(object), Some(id)) = (value.as_object_mut(), current_step_id()) {
            object.insert("id".to_owned(), serde_json::Value::String(id.to_string()));
        }
        stdio::write_with_deadline(
            2,
            format!("{value}\n").as_bytes(),
            &signals.cancelled,
            signals.deadline,
        )?;
    }
    Ok(())
}

/// The longest delay before one declared retry, the same limit the parser enforces.
const MAX_RETRY_DELAY_MS: u64 = 3_600_000;

/// What a failed command with a declared `and retry` does next.
enum RetryPlan {
    /// No declared retry applies, so the failure propagates unchanged.
    NotDeclared,
    /// A declared retry does not apply to this failure, with the reason.
    Refused(String),
    /// The declared retries are used up after this many attempts.
    Exhausted { attempts: u32 },
    /// Run again from `restart` after `delay`; `attempt` numbers the new attempt.
    Again {
        restart: usize,
        delay: std::time::Duration,
        attempt: u32,
    },
}

/// Decide whether a failed declared-retry command runs again. Only outcomes the
/// contract marks retryable, or a nonzero exit of a command without a contract, are
/// retried. A failure whose effects may be uncertain needs `even if uncertain`,
/// unless the outcome says nothing started or the applet is retry-safe.
fn plan_retry(
    program: &Program,
    index: usize,
    error: &anyhow::Error,
    registry: &Registry,
    state: &mut Option<(usize, u32)>,
) -> Result<RetryPlan> {
    let StepKind::Invoke { call, .. } = &program.steps[index].kind else {
        return Ok(RetryPlan::NotDeclared);
    };
    let Some(policy) = call.retry else {
        return Ok(RetryPlan::NotDeclared);
    };
    let earlier = match *state {
        Some((at, count)) if at == index => count,
        _ => 0,
    };
    let failures = earlier + 1;
    let Some(failure) = error.downcast_ref::<CommandFailed>() else {
        *state = None;
        return Ok(RetryPlan::Refused(
            "the failure is not a command exit".to_owned(),
        ));
    };
    let Some(outcome) = failure.outcome.as_ref() else {
        *state = None;
        return Ok(RetryPlan::Refused(
            "the command ended without an exit status".to_owned(),
        ));
    };
    let manifest = registry.resolve_call(call)?;
    let declared = outcome.kind == OutcomeKind::Retryable
        || (outcome.kind == OutcomeKind::Failure && manifest.execution_contract().is_none());
    if !declared {
        *state = None;
        return Ok(RetryPlan::Refused(format!(
            "the outcome is {}, not retryable",
            outcome.kind.as_str()
        )));
    }
    let start = command_start(program, index);
    let end = resume::node_end(program, start);
    let effects_safe = outcome.effect_certainty == crate::contracts::EffectCertainty::NotStarted
        || resume::retry_safe(program, start, end, registry)?;
    if !effects_safe && !policy.even_if_uncertain {
        *state = None;
        return Ok(RetryPlan::Refused(
            "its effects may already have happened; add `even if uncertain` to retry anyway"
                .to_owned(),
        ));
    }
    if failures > policy.retries {
        *state = None;
        return Ok(RetryPlan::Exhausted { attempts: failures });
    }
    *state = Some((index, failures));
    Ok(RetryPlan::Again {
        restart: start,
        delay: retry_delay(policy, failures),
        attempt: failures + 1,
    })
}

/// The first statement of the command at `index`. Its environment and redirection
/// markers come directly before it, and a retry restarts there.
fn command_start(program: &Program, index: usize) -> usize {
    let mut start = index;
    while start > 0
        && matches!(
            program.steps[start - 1].kind,
            StepKind::Environment { .. } | StepKind::Redirect { .. } | StepKind::Interactive
        )
    {
        start -= 1;
    }
    start
}

/// The wait before retry number `retry_number` (1-based). With doubling, each later
/// wait is twice the one before it, up to the parser's limit.
fn retry_delay(policy: RetryPolicy, retry_number: u32) -> std::time::Duration {
    let Some(base) = policy.backoff_ms else {
        return std::time::Duration::ZERO;
    };
    let factor = if policy.doubling {
        1u64.checked_shl(retry_number.saturating_sub(1))
            .unwrap_or(u64::MAX)
    } else {
        1
    };
    std::time::Duration::from_millis(base.saturating_mul(factor).min(MAX_RETRY_DELAY_MS))
}

/// Sleep before a retry, in short slices so cancellation and the deadline still apply.
fn wait_before_retry(signals: &Signals, delay: std::time::Duration) -> Result<()> {
    let until = std::time::Instant::now() + delay;
    loop {
        check_deadline(signals.deadline)?;
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        let now = std::time::Instant::now();
        if now >= until {
            return Ok(());
        }
        std::thread::sleep((until - now).min(std::time::Duration::from_millis(20)));
    }
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
/// The per-call inputs a native child status scan needs.
#[derive(Clone, Copy)]
struct StatusScan<'a> {
    calls: &'a [(&'a Invocation, &'a Manifest, usize)],
    trace: bool,
    signals: &'a Signals,
    started: Instant,
}

#[cfg(unix)]
fn scan_child_statuses(
    children: &mut Children,
    scan: StatusScan<'_>,
    observed_stages: &mut [Option<execution_result::StageResult>],
    observer: &mut Option<ChildObserver>,
    observer_ready: &mut bool,
) -> Result<()> {
    let StatusScan {
        calls,
        trace,
        signals,
        started,
    } = scan;
    #[cfg(target_os = "linux")]
    let mut exited_pids = Vec::new();
    let drain_failed = *observer_ready
        && observer.as_ref().is_some_and(|current| {
            #[cfg(target_os = "linux")]
            {
                match current.drain() {
                    Ok(exited) => {
                        exited_pids = exited;
                        false
                    }
                    Err(_) => true,
                }
            }
            #[cfg(not(target_os = "linux"))]
            current.drain().is_err()
        });
    *observer_ready = false;
    if drain_failed {
        // The readiness source is optional. On failure, use the periodic poll
        // fallback and preserve try_wait as the only child-status authority.
        *observer = None;
    }
    for (index, managed) in children.0.iter_mut().enumerate() {
        drain_diagnostic(managed, signals)?;
        if !managed.reaped {
            // An applet must stay alive until its work is complete. Reclaim descendants
            // that outlive their owning applet: reap_exited signals the group before reaping.
            #[cfg(target_os = "linux")]
            let known_exited = exited_pids.contains(&managed.child.id());
            #[cfg(not(target_os = "linux"))]
            let known_exited = false;
            let status = if known_exited {
                // Readiness leaves the leader unreaped: its PID is still reserved
                // while we kill descendants, just as in reap_exited's WNOWAIT path.
                kill_group(managed.child.id());
                Some(managed.child.wait()?)
            } else {
                reap_exited(&mut managed.child)?
            };
            if let Some(status) = status {
                managed.reaped = true;
                // The process may have exited with more than one bounded scan's
                // worth of stderr still buffered in its pipe.
                drain_exited_diagnostic(managed, signals)?;
                let code = status.code();
                let stderr = captured_stage_stderr(calls[index].0, calls[index].1, signals)
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
                signals.record_job_stage_result(
                    index,
                    observed_stages[index]
                        .as_ref()
                        .expect("stage result was just set"),
                );
                lazy_event(
                    trace,
                    signals,
                    || serde_json::json!({"event":"exit","line":calls[index].2,"applet":calls[index].1.name_for(calls[index].0),"status":status.code(),"group_elapsed_us":started.elapsed().as_micros()}),
                )?;
                let implicit_success = code.is_some_and(|code| {
                    calls[index].1.outcome_for(code).kind == crate::contracts::OutcomeKind::Success
                });
                let status_kept = calls[index].0.status_binding.is_some() && code.is_some();
                let broken_pipe = execution_result::exit_signal(status) == Some(libc::SIGPIPE);
                let inherited_reader_closed =
                    index + 1 == calls.len() && INHERIT_TERMINAL_STDOUT.with(|flag| flag.get());
                if broken_pipe && (index + 1 < calls.len() || inherited_reader_closed) {
                    continue;
                }
                if !(implicit_success || status_kept) {
                    drain_diagnostic(managed, signals)?;
                    return Err(CommandFailed {
                        line: calls[index].2,
                        operation: calls[index].1.name_for(calls[index].0).to_owned(),
                        status,
                        outcome: code.map(|code| calls[index].1.outcome_for(code)),
                        diagnostic: String::from_utf8_lossy(&managed.diagnostic)
                            .trim()
                            .to_owned(),
                        stderr: captured_stage_stderr(calls[index].0, calls[index].1, signals)
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
    /// Captured terminal stdout: what a native group read before and while its
    /// command failed, bounded by the output limit. `None` when the group does not
    /// capture stdout (streamed, inherited or discarded output).
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

thread_local! {
    /// Stable step ids of the finite run on this thread, and the index of its current step.
    static STEP_IDS: std::cell::RefCell<Option<std::sync::Arc<[std::sync::Arc<str>]>>> =
        const { std::cell::RefCell::new(None) };
    static STEP_INDEX: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Installs the step ids of one finite run on this thread. Dropping it restores the
/// enclosing run's ids, so a nested program does not relabel its caller's events.
struct StepIdScope {
    previous: Option<std::sync::Arc<[std::sync::Arc<str>]>>,
    previous_index: usize,
}

impl StepIdScope {
    /// Builds the table only when something reads step ids: a trace or a run observer.
    /// Otherwise the run pays for the step index alone.
    fn enter(steps: &[crate::ast::Step], enabled: bool) -> Self {
        let ids = enabled.then(|| {
            crate::ast::step_ids(steps)
                .into_iter()
                .map(|id| std::sync::Arc::from(id.as_str()))
                .collect()
        });
        let previous = STEP_IDS.with(|cell| cell.replace(ids));
        let previous_index = STEP_INDEX.with(|index| index.replace(0));
        Self {
            previous,
            previous_index,
        }
    }
}

impl Drop for StepIdScope {
    fn drop(&mut self) {
        STEP_IDS.with(|cell| *cell.borrow_mut() = self.previous.take());
        STEP_INDEX.with(|index| index.set(self.previous_index));
    }
}

/// Marks the step now executing on this thread.
fn set_current_step(index: usize) {
    STEP_INDEX.with(|cell| cell.set(index));
}

/// The stable id of the step executing on this thread, when a finite run is active.
pub(crate) fn current_step_id() -> Option<std::sync::Arc<str>> {
    STEP_IDS.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|ids| ids.get(STEP_INDEX.with(std::cell::Cell::get)).cloned())
    })
}

/// Maximum diagnostic stderr retained per native process stage on every platform.
pub(crate) const MAX_DIAGNOSTIC_BYTES_PER_STAGE: usize = 64 * 1024;

/// Stderr kept for a command whose stderr is merged (or redirected without a native
/// process to hand the file to): it is the redirected output itself, not a
/// diagnostic tail, so it is kept far past the diagnostic cap, but still bounded so
/// a runaway command cannot exhaust memory. A native command with `stderr to FILE`
/// writes the file directly and is not bounded by this.
pub(crate) const FORCED_STDERR_CAPTURE_LIMIT: usize = 64 * 1024 * 1024;

thread_local! {
    /// Set by the CLI around a top-level finite run. Nested runs never see it.
    static STREAM_REQUEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Finite-program nesting on this thread; only depth zero may stream.
    static FINITE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Set only while one native command whose stdout has no consumer runs.
    static INHERIT_TERMINAL_STDOUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set only while a native command with merged stderr runs (or a stderr file
    /// redirect that is not handed to a native process); its stderr is captured.
    static FORCE_STDERR_CAPTURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The kind of step whose output a thread is capturing, so an output limit error can
/// name it. Foreground is the default. Jobs and task groups set their kind on the
/// thread that runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputScope {
    Foreground,
    BackgroundJob,
    TaskGroup,
}

thread_local! {
    static OUTPUT_SCOPE: std::cell::Cell<OutputScope> =
        const { std::cell::Cell::new(OutputScope::Foreground) };
}

/// Sets the output scope of the current thread until it is dropped.
pub(crate) struct OutputScopeGuard(OutputScope);

impl OutputScopeGuard {
    pub(crate) fn enter(scope: OutputScope) -> Self {
        Self(OUTPUT_SCOPE.with(|cell| cell.replace(scope)))
    }
}

impl Drop for OutputScopeGuard {
    fn drop(&mut self) {
        OUTPUT_SCOPE.with(|cell| cell.set(self.0));
    }
}

/// The message for a captured step that exceeded its output limit. It names the kind of
/// step that overflowed and says how to fix it.
pub(crate) fn output_limit_message(run_wide: bool, limit: usize) -> String {
    let kind = match OUTPUT_SCOPE.with(|cell| cell.get()) {
        OutputScope::Foreground => "foreground command",
        OutputScope::BackgroundJob => "background job",
        OutputScope::TaskGroup => "task group",
    };
    let scope = if run_wide && kind == "task group" {
        format!("task group output exceeds the shared group limit of {limit} bytes")
    } else if run_wide {
        format!("retained {kind} output exceeds the run-wide limit of {limit} bytes")
    } else {
        format!("{kind} output exceeds the limit of {limit} bytes")
    };
    format!(
        "{scope}; use a streaming final `pipe to` command such as `tee FILE`, or raise --max-output-bytes"
    )
}

/// Run a top-level finite program so that a `run` whose stdout has no consumer
/// writes to this process's stdout as it runs. Library callers keep capture.
pub fn with_streamed_stdout<T>(run: impl FnOnce() -> T) -> T {
    let previous = STREAM_REQUEST.with(|request| request.replace(true));
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            STREAM_REQUEST.with(|request| request.set(self.0));
        }
    }
    let _restore = Restore(previous);
    run()
}

/// Tracks finite-program nesting for the current thread while a run is active.
struct FiniteDepth;
impl FiniteDepth {
    fn enter() -> (Self, usize) {
        let depth = FINITE_DEPTH.with(|depth| depth.replace(depth.get() + 1));
        (Self, depth)
    }
}
impl Drop for FiniteDepth {
    fn drop(&mut self) {
        FINITE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Scopes the inherit flag to one native command; restored even on error.
struct InheritTerminalStdout(bool);
impl InheritTerminalStdout {
    fn enter(on: bool) -> Self {
        Self(INHERIT_TERMINAL_STDOUT.with(|flag| flag.replace(on)))
    }
}
impl Drop for InheritTerminalStdout {
    fn drop(&mut self) {
        INHERIT_TERMINAL_STDOUT.with(|flag| flag.set(self.0));
    }
}

/// Static consumer rule for uncaptured stdout. A fresh `run` whose output can
/// only be replaced, or that ends the program, has no consumer and may stream.
/// Bindings (including status/result capture), cache, and reachable pipeline
/// consumers retain buffering. Control markers alone do not consume stdout.
fn streams_terminal_stdout(program: &Program, index: usize) -> bool {
    let Some(crate::ast::Step {
        kind:
            StepKind::Invoke {
                call,
                connection: Connection::Start,
            },
        ..
    }) = program.steps.get(index)
    else {
        return false;
    };
    let captures = |call: &Invocation| {
        call.cache
            || call.binding.is_some()
            || call.result_binding.is_some()
            || call.status_binding.is_some()
    };
    if captures(call) {
        return false;
    }
    let mut end = index + 1;
    while pipe_at(program, end) {
        // Any stage of the pipe group may capture (`pipe to b as x`); then the
        // group's output has a consumer and must not stream.
        if let Some(crate::ast::Step {
            kind: StepKind::Invoke { call, .. },
            ..
        }) = program.steps.get(end)
        {
            if captures(call) {
                return false;
            }
        }
        end += 1;
    }
    !crate::ast::has_stdout_consumer(&program.steps, end)
}

/// Write owned output from a streamed step without copying its captured bytes.
fn write_streamed_stdout_owned(bytes: Vec<u8>, signals: &Signals) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    observe::note_terminal_output(&bytes);
    if observe::writes_streamed_stdout() {
        stdio::write_with_deadline_owned_reusing(
            &signals.output_writers,
            1,
            bytes,
            &signals.cancelled,
            signals.deadline,
        )
        .context("could not stream workflow stdout")?;
    }
    Ok(())
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
    #[cfg(windows)]
    {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let stem = name
            .trim_end_matches([' ', '.'])
            .split('.')
            .next()
            .unwrap_or_default();
        let upper = stem.to_ascii_uppercase();
        if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (upper
                .strip_prefix("COM")
                .or_else(|| upper.strip_prefix("LPT"))
                .is_some_and(|suffix| {
                    suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
                }))
        {
            bail!(
                "refusing to write to reserved device name: {}",
                path.display()
            );
        }
    }
    let existing_permissions = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to replace a symbolic link: {}", path.display());
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("refusing to replace a non-regular file: {}", path.display());
        }
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() > 1 {
                    bail!("refusing to write a hard-linked file: {}", path.display());
                }
            }
            #[cfg(windows)]
            {
                let existing =
                    File::open(path).with_context(|| format!("checking {}", path.display()))?;
                if redirect::windows_file_info(&existing)?.nNumberOfLinks > 1 {
                    bail!("refusing to write a hard-linked file: {}", path.display());
                }
            }
            #[cfg(not(unix))]
            let _ = &metadata;
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("checking {}", path.display())),
    };
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
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        crate::platform::replace_file(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

/// Programs with an `on interrupt` body drain on their first interrupt.
fn has_interrupt_handler(program: &Program) -> bool {
    program
        .steps
        .iter()
        .any(|step| matches!(step.kind, StepKind::OnInterrupt { .. }))
}

pub fn run(program: &Program, registry: &Registry, options: &RunOptions) -> Result<RunResult> {
    run_with_drain_timeout(program, registry, options, DEFAULT_DRAIN_TIMEOUT)
}

/// `run` with an explicit interrupt drain budget. The budget applies only to programs
/// with an `on interrupt` body.
pub fn run_with_drain_timeout(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    drain_timeout: Duration,
) -> Result<RunResult> {
    #[cfg(target_os = "linux")]
    let _sandbox_probe_run = crate::platform::sandbox::begin_probe_run();
    let deadline = deadline_after(options.total_timeout, "total timeout")?;
    let signals = if has_interrupt_handler(program) {
        Signals::register_with_drain(deadline, drain_timeout)?
    } else {
        Signals::register_with_deadline(deadline)?
    };
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
    file: impl Read,
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
    resolved_json_arguments_skipping(call, manifest, bindings, None)
}

/// Resolve, validate and convert a call's arguments. A `skipped` parameter is validated
/// like the others but left out of the converted map, for an applet that reads it as a
/// typed value.
fn resolved_json_arguments_skipping(
    call: &Invocation,
    manifest: &Manifest,
    bindings: &BTreeMap<String, Value>,
    skipped: Option<&str>,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    // Literal-only arguments are already in the shape expected by the
    // manifest validator. Avoid cloning every key and nested Value into a
    // temporary map before the required JSON conversion.
    if !call.parameters.values().any(contains_reference) {
        return manifest.parameters_json_skipping(&call.parameters, skipped);
    }

    let values = call
        .parameters
        .iter()
        .map(|(name, value)| Ok((name.clone(), resolve_borrowed(value, bindings)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    manifest.parameters_json_skipping(&values, skipped)
}

/// The applet's typed JSON parameter and its bound value, when the call passes that
/// parameter as a direct reference to a reference-free binding.
fn typed_json_argument<'a>(
    call: &Invocation,
    manifest: &Manifest,
    bindings: &'a BTreeMap<String, Value>,
) -> Option<(&'static str, &'a Value)> {
    let parameter = crate::applets::typed_json_parameter(manifest.name_for(call))?;
    let Some(Value::Reference(name)) = call.parameters.get(parameter) else {
        return None;
    };
    let value = bindings
        .get(name)
        .filter(|value| !contains_reference(value))?;
    Some((parameter, value))
}

/// A parameter value with its references resolved, borrowed where possible: a direct
/// reference borrows the binding, so a large document or text is not cloned before it
/// is validated and converted; only composite values that contain references are built.
fn resolve_borrowed<'a>(
    value: &'a Value,
    bindings: &'a BTreeMap<String, Value>,
) -> Result<Cow<'a, Value>> {
    match value {
        Value::Reference(name) => bindings
            .get(name)
            .map(Cow::Borrowed)
            .ok_or_else(|| anyhow!("undefined result {name:?}")),
        value if !contains_reference(value) => Ok(Cow::Borrowed(value)),
        value => resolve(value, bindings).map(Cow::Owned),
    }
}

/// One applet call's captured output, as the decoder reads it.
struct CallOutput<'a> {
    manifest: &'a Manifest,
    line: usize,
    bytes: &'a [u8],
    status: Option<i32>,
    stdout_captured: bool,
    stderr: Option<&'a [u8]>,
    stages: &'a [execution_result::StageResult],
    typed_json: Option<serde_json::Value>,
}

fn bind_call_output(
    name: &str,
    output: CallOutput<'_>,
    retain_value: bool,
    bindings: &mut BTreeMap<String, Value>,
) -> Result<Option<Value>> {
    let value = decode_call_output(output)?;
    if retain_value {
        bindings.insert(name.to_owned(), value.clone());
        Ok(Some(value))
    } else {
        bindings.insert(name.to_owned(), value);
        Ok(None)
    }
}

fn decode_call_output(output: CallOutput<'_>) -> Result<Value> {
    let CallOutput {
        manifest,
        line,
        bytes,
        status,
        stdout_captured,
        stderr,
        stages,
        typed_json,
    } = output;
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
        let decoded = match typed_json {
            Some(value) if manifest.has_json_encoded_output_schema() => manifest
                .decode_json_output_value(value, bytes, status)
                .map_err(decode_failure_for_status)?,
            _ => manifest
                .decode_output(bytes, status)
                .map_err(decode_failure_for_status)?,
        };
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

/// One applet call as the runtime sees it: the invocation, its manifest and its
/// source line.
#[derive(Clone, Copy)]
pub(crate) struct CallSite<'a> {
    pub(crate) call: &'a Invocation,
    pub(crate) manifest: &'a Manifest,
    pub(crate) line: usize,
}

/// The exit status, diagnostic and captured streams of a call whose failure is
/// retained rather than raised.
pub(crate) struct RetainedStatus<'a> {
    pub(crate) status: i32,
    pub(crate) diagnostic: &'a str,
    pub(crate) stdout: Option<Vec<u8>>,
    pub(crate) stderr: Option<Vec<u8>>,
}

pub(crate) fn check_retained_status(
    site: CallSite<'_>,
    retained: RetainedStatus<'_>,
    registry: &Registry,
) -> Result<()> {
    let CallSite {
        call,
        manifest,
        line,
    } = site;
    let RetainedStatus {
        status,
        diagnostic,
        stdout,
        stderr,
    } = retained;
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

/// What a single applet launch runs against, shared by the in-process, worker
/// and remote backends.
#[derive(Clone, Copy)]
struct LaunchEnv<'a> {
    bindings: &'a BTreeMap<String, Value>,
    cwd: &'a Path,
    options: &'a RunOptions,
    signals: &'a Signals,
    registry: &'a Registry,
}

fn execute_in_process(site: CallSite<'_>, input: Input, env: LaunchEnv<'_>) -> Result<GroupOutput> {
    let CallSite {
        call,
        manifest,
        line,
    } = site;
    let LaunchEnv {
        bindings,
        cwd,
        options,
        signals,
        registry,
    } = env;
    check_deadline(signals.deadline)?;
    if signals.is_cancelled() {
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
    // A typed JSON parameter is validated with the others but not converted: the applet
    // reads the bound value directly, so a large document is not copied into a second tree.
    let typed_json = typed_json_argument(call, manifest, bindings);
    let arguments = resolved_json_arguments_skipping(
        call,
        manifest,
        bindings,
        typed_json.map(|(parameter, _)| parameter),
    )?;
    check_deadline(deadline)?;
    let checkpoint = || {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            Err(crate::applets::ExecutionBudgetExceeded::Timeout)
        } else if signals.is_cancelled() {
            Err(crate::applets::ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = crate::applets::ExecutionContext::new(&checkpoint);
    let result = match typed_json {
        Some((_, typed)) => crate::applets::execute_with_typed_json(
            manifest.name_for(call),
            &arguments,
            typed,
            &context,
        ),
        None => crate::applets::execute_with_context(
            manifest.name_for(call),
            &arguments,
            &input,
            cwd,
            &context,
        ),
    };
    match result {
        Ok(value) => {
            // Text and path results are raw UTF-8; records and lists stay JSON.
            // The byte output owns the same allocation as the applet's String.
            let keep_typed_value = (call.binding.is_some() || call.result_binding.is_some())
                && manifest.has_json_encoded_output_schema();
            let (bytes, value) = match value {
                serde_json::Value::String(text) if manifest.in_process_text_output() => {
                    if keep_typed_value {
                        (
                            text.as_bytes().to_vec(),
                            Some(serde_json::Value::String(text)),
                        )
                    } else {
                        (text.into_bytes(), None)
                    }
                }
                value => {
                    let mut json = serde_json::to_vec(&value)?;
                    json.push(b'\n');
                    (json, Some(value))
                }
            };
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
                    value.clone().map(crate::structured::from_json).or_else(|| {
                        String::from_utf8(bytes.clone())
                            .ok()
                            .map(crate::ast::Value::String)
                    }),
                    vec![stage],
                );
                failure.kind = execution_result::ExecutionKind::Timeout;
                failure.message = Some(format!("{error:#}"));
                return Err(execution_result::ExecutionFailure::new(failure).into());
            }
            if signals.is_cancelled() {
                let mut failure = execution_result::ExecutionResult::from_outcome(
                    outcome,
                    Some(bytes.clone()),
                    None,
                    value.clone().map(crate::structured::from_json).or_else(|| {
                        String::from_utf8(bytes.clone())
                            .ok()
                            .map(crate::ast::Value::String)
                    }),
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
                typed_json: if keep_typed_value { value } else { None },
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
                CallSite {
                    call,
                    manifest,
                    line,
                },
                RetainedStatus {
                    status,
                    diagnostic: &diagnostic,
                    stdout: None,
                    stderr: Some(diagnostic_bytes),
                },
                registry,
            )?;
            Ok(GroupOutput {
                bytes: Vec::new(),
                typed_json: None,
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

fn execute_json_lines_worker(
    site: CallSite<'_>,
    input: Input,
    env: LaunchEnv<'_>,
    argv: Vec<String>,
    launched: &mut usize,
) -> Result<GroupOutput> {
    let CallSite {
        call,
        manifest,
        line,
    } = site;
    let LaunchEnv {
        bindings,
        cwd,
        options,
        signals,
        registry,
        ..
    } = env;
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
    #[cfg(not(target_arch = "wasm32"))]
    let json_output = manifest.has_json_encoded_output_schema();
    #[cfg(target_arch = "wasm32")]
    let json_output = false;
    let (arguments, value) = if launch.protocol_version == 2 {
        let resolved = call
            .parameters
            .iter()
            .map(|(name, value)| {
                Ok((
                    name.clone(),
                    resolve_borrowed(value, bindings)?.into_owned(),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        manifest.build_worker_arguments(&resolved)?
    } else {
        (argv, None)
    };
    let request = worker_backend::WorkerRequest {
        arguments,
        input,
        value,
        json_output,
        max_output_bytes: options.max_output_bytes,
        sensitivity: signals.sensitivity_floor(),
        deadline,
        cancelled: signals.cancelled.clone(),
        spawned: Arc::clone(&spawned),
    };
    let worker_pool = signals
        .worker_pool
        .as_ref()
        .cloned()
        .unwrap_or_else(|| registry.host_state::<worker_backend::WorkerPool>());
    let response = worker_pool.invoke(launch, request).with_context(|| {
        format!(
            "line {line}: running json_lines_worker applet {:?}",
            manifest.name_for(call)
        )
    });
    if spawned.load(Ordering::Acquire) {
        signals.record_process_launch();
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
        CallSite {
            call,
            manifest,
            line,
        },
        RetainedStatus {
            status: response.status,
            diagnostic: &String::from_utf8_lossy(&response.stderr),
            stdout: Some(response.output.clone()),
            stderr: Some(response.stderr.clone()),
        },
        registry,
    )?;
    Ok(GroupOutput {
        bytes: response.output,
        typed_json: response.typed_json,
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

/// Run one command condition and report whether its declared outcome matched
/// `expect_success`. Stdin is closed and the pipeline value is not touched; the
/// captured output is dropped. Spawn failures, timeouts and cancellation are
/// errors, and a command that ends without an exit status is also an error.
/// One command condition: the call that decides it, the outcome it expects and
/// the group environment it runs in.
struct ConditionRequest<'a> {
    call: &'a Invocation,
    expect_success: bool,
    line: usize,
    bindings: &'a BTreeMap<String, Value>,
    cwd: &'a crate::working_dir::WorkingDirectory,
    options: &'a RunOptions,
    signals: &'a Signals,
    registry: &'a Registry,
    environment: Option<crate::capabilities::CommandEnvOverlay>,
}

fn run_command_condition(request: ConditionRequest<'_>, launched: &mut usize) -> Result<bool> {
    let ConditionRequest {
        call,
        expect_success,
        line,
        bindings,
        cwd,
        options,
        signals,
        registry,
        environment,
    } = request;
    let _environment =
        environment.map(|overlay| crate::capabilities::CommandEnvScope::enter(Some(overlay)));
    let manifest = registry.resolve_call(call)?;
    // A nonzero exit is the answer here, not an applet failure. Retaining the
    // status is the process layer's switch for that; the placeholder name is
    // never bound, and it is set only on this private copy.
    let mut retained = call.clone();
    retained.status_binding = Some(String::from("command condition"));
    retained.discard_stderr = true;
    let completed = execute_call_group(GroupRequest {
        calls: &[(&retained, manifest, line)],
        input: Input::Empty,
        prepared_first_call: None,
        bindings,
        cwd,
        options,
        launched,
        signals,
        registry,
    })
    .with_context(|| format!("line {line}: command condition {:?}", call.operation))?;
    let status = completed.status.with_context(|| {
        format!(
            "line {line}: command condition {:?} ended without an exit status",
            call.operation
        )
    })?;
    let succeeded = manifest.outcome_for(status).kind == OutcomeKind::Success;
    Ok(succeeded == expect_success)
}

fn execute_call_group_unobserved(group: GroupRequest<'_>) -> Result<GroupOutput> {
    let GroupRequest {
        registry,
        calls,
        input,
        prepared_first_call,
        bindings,
        cwd,
        options,
        launched,
        signals,
    } = group;
    let output = execute_call_group_unobserved_inner(GroupRequest {
        registry,
        calls,
        input,
        prepared_first_call,
        bindings,
        cwd,
        options,
        launched,
        signals,
    });
    // Sensitive-output registration emits mask lines, so it observes the
    // run's cancel/deadline instead of hanging on a wedged terminal; a
    // cancelled registration fails the group instead of echoing unmasked.
    match &output {
        Ok(output) => register_sensitive_group_output(
            registry,
            calls,
            bindings,
            output,
            &signals.cancelled,
            signals.deadline,
        )?,
        Err(error) => register_sensitive_group_failure(
            registry,
            calls,
            error,
            &signals.cancelled,
            signals.deadline,
        )?,
    }
    output
}

fn execute_call_group_unobserved_inner(group: GroupRequest<'_>) -> Result<GroupOutput> {
    let GroupRequest {
        registry,
        calls,
        input,
        prepared_first_call,
        bindings,
        cwd,
        options,
        launched,
        signals,
    } = group;
    let Some((call, manifest, line)) = calls.first().copied() else {
        bail!("cannot execute an empty applet group");
    };
    // A build without an applet's gated feature refuses it before anything launches.
    for (_, stage, stage_line) in calls {
        if let Some(message) = stage.build_unavailable_message() {
            bail!("line {stage_line}: {message}");
        }
    }
    let backend = manifest.execution_backend();
    if backend == AppletBackend::Command {
        if calls.iter().any(|(_, stage_manifest, _)| {
            stage_manifest.execution_backend() != AppletBackend::Command
        }) {
            bail!("mixed command and non-command applet groups are unsupported");
        }
        return execute_group(GroupRequest {
            registry,
            calls,
            input,
            prepared_first_call,
            bindings,
            cwd,
            options,
            launched,
            signals,
        });
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
    let worker_uses_typed_v2 = backend == AppletBackend::JsonLinesWorker
        && manifest.execution_contract().is_some_and(|contract| {
            matches!(
                &contract.lifecycle,
                Lifecycle::JsonLines {
                    protocol_version: 2,
                    ..
                }
            )
        });
    let argv = if backend == AppletBackend::JsonLinesWorker && !worker_uses_typed_v2 {
        match prepared_first_call {
            Some((_, argv)) => argv,
            None => arguments_for_call(call, manifest, bindings)?,
        }
    } else {
        Vec::new()
    };
    let site = CallSite {
        call,
        manifest,
        line,
    };
    let env = LaunchEnv {
        bindings,
        cwd,
        options,
        signals,
        registry,
    };
    match backend {
        AppletBackend::Command => unreachable!("handled above"),
        AppletBackend::InProcess => execute_in_process(site, input, env),
        AppletBackend::JsonLinesWorker => {
            execute_json_lines_worker(site, input, env, argv, launched)
        }
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
                site,
                input,
                LaunchEnv {
                    registry: child_registry,
                    ..env
                },
                launched,
            )
        }
        AppletBackend::Remote => {
            let process_launches_before = signals.process_launch_count();
            let result = execute_remote_applet(site, input, env);
            *launched += signals
                .process_launch_count()
                .saturating_sub(process_launches_before);
            result
        }
    }
}

fn register_sensitive_group_output(
    registry: &Registry,
    calls: &[(&Invocation, &Manifest, usize)],
    bindings: &BTreeMap<String, Value>,
    output: &GroupOutput,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    // Only the terminal pipeline stage may bind output; in-process env.load
    // cannot join a native pipeline, so every valid sensitive binding is last.
    let Some((call, manifest, line)) = calls.last().copied() else {
        return Ok(());
    };
    let sensitive_group = calls.iter().any(|(call, manifest, _)| {
        call.sensitive_binding
            || call.marks_sensitive_output()
            || crate::security::applet_has_declared_secret_environment(
                registry,
                manifest.name_for(call),
            )
    });
    if !sensitive_group {
        return Ok(());
    }
    if call.marks_sensitive_output() && manifest.name_for(call) == "env.load" {
        let arguments = resolved_json_arguments(call, manifest, bindings)?;
        let output_json = output
            .typed_json
            .clone()
            .or_else(|| serde_json::from_slice(&output.bytes).ok())
            .ok_or_else(|| anyhow!("line {line}: could not register env.load secret values"))?;
        let values =
            crate::applets::secret_values(manifest.name_for(call), &arguments, &output_json)
                .map_err(|_| anyhow!("line {line}: could not register env.load secret values"))?;
        let redactor = crate::security::secret_redactor(registry);
        for value in values {
            redactor
                .register(value.as_bytes(), cancelled, deadline)
                .map_err(|_| anyhow!("line {line}: could not register env.load secret values"))?;
        }
        return Ok(());
    }
    let redactor = crate::security::secret_redactor(registry);
    for bytes in std::iter::once(output.bytes.as_slice())
        .chain(std::iter::once(output.diagnostic.as_slice()))
        .chain(
            output
                .stages
                .iter()
                .filter_map(|stage| stage.stderr.as_deref()),
        )
    {
        redactor
            .register(bytes, cancelled, deadline)
            .map_err(|_| anyhow!("line {line}: could not register sensitive command output"))?;
        if bytes.ends_with(b"\n") {
            redactor
                .register(&bytes[..bytes.len() - 1], cancelled, deadline)
                .map_err(|_| anyhow!("line {line}: could not register sensitive command output"))?;
        }
    }
    if let Some(value) = &output.typed_json {
        crate::security::register_sensitive_value(
            registry,
            &crate::structured::from_json(value.clone()),
            cancelled,
            deadline,
        )?;
    }
    Ok(())
}

fn register_sensitive_group_failure(
    registry: &Registry,
    calls: &[(&Invocation, &Manifest, usize)],
    error: &anyhow::Error,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    let Some((_, _, line)) = calls.last().copied() else {
        return Ok(());
    };
    let sensitive_call = calls.iter().any(|(call, manifest, _)| {
        call.sensitive_binding
            || call.marks_sensitive_output()
            || crate::security::applet_has_declared_secret_environment(
                registry,
                manifest.name_for(call),
            )
    });
    if !sensitive_call {
        return Ok(());
    }
    let result = execution_result::result_for_error(error);
    let redactor = crate::security::secret_redactor(registry);
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
            .register(output, cancelled, deadline)
            .map_err(|_| anyhow!("line {line}: could not register sensitive command output"))?;
        if output.ends_with(b"\n") {
            redactor
                .register(&output[..output.len() - 1], cancelled, deadline)
                .map_err(|_| anyhow!("line {line}: could not register sensitive command output"))?;
        }
    }
    if let Some(value) = &result.value {
        crate::security::register_sensitive_value(registry, value, cancelled, deadline)?;
    }
    Ok(())
}

#[cfg(feature = "remote")]
fn execute_remote_applet(
    site: CallSite<'_>,
    input: Input,
    env: LaunchEnv<'_>,
) -> Result<GroupOutput> {
    let CallSite {
        call,
        manifest,
        line,
    } = site;
    let LaunchEnv {
        bindings,
        cwd,
        options,
        signals,
        registry,
    } = env;
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
        .with_resolver_helper(std::env::current_exe()?)
        .with_process_observer(signals.process_launch_callback())
        .with_ssrf_grant(crate::capabilities::remote_origin_is_explicitly_granted(
            registry,
            &manifest.name,
            &endpoint.endpoint,
        ));
    if let Some(path) = client.descriptor_cache_path() {
        crate::capabilities::authorize_applet_host_path(
            registry,
            &manifest.name,
            &path,
            crate::capabilities::HostAccess::Read,
            cwd,
        )?;
    }
    if let Some(path) = client.bearer_file_path() {
        crate::capabilities::authorize_applet_host_path(
            registry,
            &manifest.name,
            &path,
            crate::capabilities::HostAccess::Read,
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
        .map(|(name, value)| Ok((name.clone(), resolve_borrowed(value, bindings)?)))
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
        CallSite {
            call,
            manifest,
            line,
        },
        RetainedStatus {
            status,
            diagnostic: &diagnostic,
            stdout: Some(output_bytes.clone()),
            stderr: Some(diagnostic.as_bytes().to_vec()),
        },
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
        typed_json: None,
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

#[cfg(not(feature = "remote"))]
fn execute_remote_applet(
    site: CallSite<'_>,
    input: Input,
    env: LaunchEnv<'_>,
) -> Result<GroupOutput> {
    let CallSite { manifest, line, .. } = site;
    let _ = (input, env);
    bail!(
        "line {line}: applet {} is remote-backed, and this build has no remote backend; use the full build",
        manifest.name
    )
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
        protocol_version,
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
        binary_path_for_launch(manifest.binary_for(call), cwd, registry)?,
        cwd.to_path_buf(),
        startup_arguments.iter().map(OsString::from).collect(),
        worker_backend::WorkerLimits {
            protocol_version: *protocol_version,
            timeout: Duration::from_millis(*timeout_ms),
            max_frame_bytes: *max_frame_bytes,
            max_stderr_bytes: *max_stderr_bytes,
        },
    )
}

/// Read retained worker provenance for graph/runtime adapters that maintain
/// their own event provenance. Preflight may conservatively include workers
/// whose executable content changed at the same launch path; dispatch checks
/// the exact content identity before sending a request.
#[cfg(feature = "graph")]
pub(crate) fn worker_sensitivity_history_for_call(
    call: &Invocation,
    manifest: &Manifest,
    registry: &Registry,
    cwd: &Path,
) -> Result<crate::security::SensitivitySet> {
    worker_sensitivity_history_for_call_with_pool(call, manifest, registry, cwd, None)
}

fn worker_sensitivity_history_for_call_with_pool(
    call: &Invocation,
    manifest: &Manifest,
    registry: &Registry,
    cwd: &Path,
    worker_pool: Option<&worker_backend::WorkerPool>,
) -> Result<crate::security::SensitivitySet> {
    if manifest.execution_backend() != AppletBackend::JsonLinesWorker {
        return Ok(crate::security::SensitivitySet::empty());
    }
    let launch = worker_launch_for_call(call, manifest, registry, cwd)?;
    match worker_pool {
        Some(worker_pool) => worker_pool.sensitivity_history_for_launch(&launch),
        None => registry
            .host_state::<worker_backend::WorkerPool>()
            .sensitivity_history_for_launch(&launch),
    }
}

fn append_invocation_sensitivities(
    call: &Invocation,
    registry: &Registry,
    cwd: &Path,
    sensitivities: &mut crate::security::SensitivitySet,
    worker_pool: Option<&worker_backend::WorkerPool>,
) -> Result<()> {
    let manifest = registry.resolve_call(call)?;
    if call.marks_sensitive_output() {
        sensitivities.insert(crate::contracts::Sensitivity::Secret);
    }
    append_manifest_sensitivities(call, manifest, registry, cwd, sensitivities, worker_pool)
}

fn append_manifest_sensitivities(
    call: &Invocation,
    manifest: &Manifest,
    registry: &Registry,
    cwd: &Path,
    sensitivities: &mut crate::security::SensitivitySet,
    worker_pool: Option<&worker_backend::WorkerPool>,
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
    append_worker_sensitivity_history(call, manifest, registry, cwd, sensitivities, worker_pool)
}

fn append_worker_sensitivity_history(
    call: &Invocation,
    manifest: &Manifest,
    registry: &Registry,
    cwd: &Path,
    sensitivities: &mut crate::security::SensitivitySet,
    worker_pool: Option<&worker_backend::WorkerPool>,
) -> Result<()> {
    if manifest.execution_backend() == AppletBackend::JsonLinesWorker {
        let worker_history = worker_sensitivity_history_for_call_with_pool(
            call,
            manifest,
            registry,
            cwd,
            worker_pool,
        )?;
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
    append_manifest_sensitivities(call, manifest, registry, cwd, &mut sensitivities, None)?;
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
    worker_pool: Option<&worker_backend::WorkerPool>,
) -> Result<crate::security::SensitivitySet> {
    let mut sensitivities = crate::security::SensitivitySet::empty();
    if program.inputs.iter().any(crate::ast::is_sensitive)
        || program
            .definitions
            .values()
            .any(|definition| definition.inputs.iter().any(crate::ast::is_sensitive))
    {
        sensitivities.insert(crate::contracts::Sensitivity::Secret);
    }
    for step in &program.steps {
        match &step.kind {
            StepKind::Invoke { call, .. } => {
                if call.marks_sensitive_output() {
                    sensitivities.insert(crate::contracts::Sensitivity::Secret);
                }
                let manifest = registry.resolve_call(call)?;
                if manifest.builtin.is_none() {
                    append_manifest_sensitivities(
                        call,
                        manifest,
                        registry,
                        cwd,
                        &mut sensitivities,
                        worker_pool,
                    )?;
                }
            }
            StepKind::CallProgram { call, .. } => {
                if call.marks_sensitive_output() {
                    sensitivities.insert(crate::contracts::Sensitivity::Secret);
                }
            }
            StepKind::TaskGroup { tasks, .. } => {
                for call in tasks {
                    append_invocation_sensitivities(
                        call,
                        registry,
                        cwd,
                        &mut sensitivities,
                        worker_pool,
                    )?;
                }
            }
            StepKind::SpawnJob { stages, .. } => {
                for stage in stages {
                    append_invocation_sensitivities(
                        &stage.call,
                        registry,
                        cwd,
                        &mut sensitivities,
                        worker_pool,
                    )?;
                }
            }
            StepKind::IfCommand { call, .. }
            | StepKind::Loop {
                kind: crate::ast::LoopKind::WhileCommand { call, .. },
                ..
            } => {
                append_invocation_sensitivities(
                    call,
                    registry,
                    cwd,
                    &mut sensitivities,
                    worker_pool,
                )?;
            }
            // Reusable call bodies are expanded into the sequence; their
            // marked call bindings above add the secret persistence floor.
            StepKind::Loop { .. }
            | StepKind::EndLoop { .. }
            | StepKind::If { .. }
            | StepKind::Otherwise { .. }
            | StepKind::OnInterrupt { .. }
            | StepKind::EndIf
            | StepKind::EndCall
            | StepKind::BreakLoop
            | StepKind::ContinueLoop
            | StepKind::Return
            | StepKind::Exit { .. }
            | StepKind::Fail { .. }
            | StepKind::Try { .. }
            | StepKind::Catch { .. }
            | StepKind::Finally
            | StepKind::EndTry
            | StepKind::UseFolder { .. }
            | StepKind::UseValue { .. }
            | StepKind::BindValue { .. }
            | StepKind::CaptureBytes { .. }
            | StepKind::UseBytes { .. }
            | StepKind::Read { .. }
            | StepKind::ReadStdin { .. }
            | StepKind::InspectJob { .. }
            | StepKind::AwaitJob { .. }
            | StepKind::CancelJob { .. }
            | StepKind::ReadCallInput { .. }
            | StepKind::ParseJson { .. }
            | StepKind::TakeJson { .. }
            | StepKind::TakeJsonDynamic { .. }
            | StepKind::EncodeJson { .. }
            | StepKind::Save { .. }
            | StepKind::Append { .. }
            | StepKind::Redirect { .. }
            | StepKind::Environment { .. }
            | StepKind::Interactive
            | StepKind::HandOver
            | StepKind::LaunchDetached { .. }
            | StepKind::TempScope { .. }
            | StepKind::EndTempScope { .. } => {}
        }
    }
    if let Some(crate::ast::Trigger::Listen {
        source: crate::ast::ListenSource::Applet(call),
        ..
    }) = &program.trigger
    {
        append_invocation_sensitivities(call, registry, cwd, &mut sensitivities, worker_pool)?;
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
        append_invocation_sensitivities(call, scope, cwd, &mut sensitivities, worker_pool)?;
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
        discard_stderr: false,
        parameters,
        binding: None,
        cache: false,
        sensitive_binding: false,
        status_binding: Some("__flow_internal_status".to_owned()),
        result_binding: None,
        retry: None,
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
        discard_stderr: false,
        parameters: parameters.clone(),
        binding: None,
        cache: false,
        sensitive_binding: false,
        status_binding: Some("__flow_internal_status".to_owned()),
        result_binding: None,
        retry: None,
    };
    let manifest = registry.resolve_call(&call)?;
    let cwd = fs::canonicalize(&options.working_directory)?;
    let sensitivities = invocation_sensitivities(&call, manifest, registry, &cwd)?;
    invoke_applet_with_context(
        registry,
        name,
        parameters,
        input,
        options,
        cancelled,
        AppletInvocationContext::new(module_session, sensitivities),
    )
}

/// Invoke one applet with execution state inherited from its enclosing run.
/// Nested modules and task children share module, worker, provenance, observer,
/// and job-budget state while each invocation keeps its own process counter.
pub(crate) fn invoke_applet_with_context(
    registry: &Registry,
    name: &str,
    parameters: BTreeMap<String, Value>,
    input: Vec<u8>,
    options: &RunOptions,
    cancelled: Arc<AtomicBool>,
    context: AppletInvocationContext,
) -> Result<AppletInvocationResult> {
    #[cfg(target_os = "linux")]
    let _sandbox_probe_run = crate::platform::sandbox::begin_probe_run_if_missing();
    invoke_call_with_context(
        AppletInvocation {
            registry,
            name,
            direct: false,
            parameters,
            input,
            options,
        },
        cancelled,
        context,
    )
}

/// One applet invocation: the applet name, its parameters and input, and the
/// registry and options it runs under.
pub(crate) struct AppletInvocation<'a> {
    pub(crate) registry: &'a Registry,
    pub(crate) name: &'a str,
    pub(crate) direct: bool,
    pub(crate) parameters: BTreeMap<String, Value>,
    pub(crate) input: Vec<u8>,
    pub(crate) options: &'a RunOptions,
}

/// [`invoke_applet_with_context`] for either a named applet or, when `direct`
/// is set, a quoted executable resolved through the direct-call binding.
pub(crate) fn invoke_call_with_context(
    request: AppletInvocation<'_>,
    cancelled: Arc<AtomicBool>,
    context: AppletInvocationContext,
) -> Result<AppletInvocationResult> {
    let AppletInvocation {
        registry,
        name,
        direct,
        parameters,
        input,
        options,
    } = request;
    let module_session = Arc::clone(&context.module_session);
    let working_directory = fs::canonicalize(&options.working_directory)?;
    let mut canonicalized_options = None;
    if working_directory != options.working_directory {
        let mut owned = options.clone();
        owned.working_directory = working_directory;
        canonicalized_options = Some(owned);
    }
    // Most runtime callers already carry a canonical working directory. Avoid
    // cloning the run inputs for each child when the only required adjustment
    // is already satisfied.
    let invocation_options = canonicalized_options.as_ref().unwrap_or(options);
    let call = Invocation {
        operation: name.to_owned(),
        direct,
        discard_stderr: false,
        parameters,
        binding: None,
        cache: false,
        sensitive_binding: false,
        status_binding: Some("__flow_internal_status".to_owned()),
        result_binding: None,
        retry: None,
    };
    let manifest = registry.resolve_call(&call)?;
    manifest.validate(&call)?;
    let program = Program {
        definitions: Default::default(),
        inputs: Vec::new(),
        steps: vec![crate::ast::Step {
            line: 1,
            label: None,
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
    let own_deadline = deadline_after(invocation_options.total_timeout, "total timeout")?;
    let deadline = match (own_deadline, context.deadline) {
        (Some(own), Some(outer)) => Some(own.min(outer)),
        (Some(own), None) => Some(own),
        (None, outer) => outer,
    };
    let mut signals = Signals::with_cancellation_and_deadline(cancelled, true, deadline)?;
    signals.inherit_applet_context(&context)?;
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
        let completed = execute_call_group(GroupRequest {
            calls: &[(&call, manifest, 1)],
            input: Input::Bytes(input),
            prepared_first_call: None,
            bindings: &bindings,
            cwd: &crate::working_dir::WorkingDirectory::plain(
                invocation_options.working_directory.clone(),
            ),
            options: invocation_options,
            launched: &mut launched,
            signals: &signals,
            registry,
        })?;
        // Native streams reserve as they are read. Other backends return a
        // completed buffer; account it atomically before publishing the result.
        // Program modules isolate intermediate output and charge their final
        // returned buffer here along with the other non-native backends.
        if manifest.execution_backend() != AppletBackend::Command {
            if let Some((budget, limit)) = &signals.group_diagnostic_budget {
                let captured = completed
                    .bytes
                    .len()
                    .saturating_add(completed.diagnostic.len());
                reserve_group_capture(budget, *limit, captured)?;
            }
        }
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
                        captured_stage_stderr(&call, manifest, &signals)
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
            launched_processes: signals.process_launch_count(),
        }
        .into()
    })
}

enum AppliedControl {
    Continue,
    Jump(usize),
    Rethrow(anyhow::Error),
}

struct ControlContext<'program, 'frames> {
    program: &'program Program,
    state: &'program mut control_flow::RuntimeState,
    call_frames: &'program mut Vec<calls::Frame<'frames>>,
    loop_frames: &'program mut loops::LoopStack,
    bindings: &'program mut BTreeMap<String, Value>,
    cwd: &'program mut crate::working_dir::WorkingDirectory,
    job_supervisor: &'program mut jobs::Supervisor,
}

impl<'program, 'frames> ControlContext<'program, 'frames> {
    fn unwind(&mut self, unwind: control_flow::UnwindTo, collect_loop_jobs: bool) -> Result<()> {
        let control_flow::UnwindTo {
            call_depth,
            loop_depth,
            drop_bindings,
        } = unwind;
        if call_depth > self.call_frames.len() {
            bail!("control transfer targets an inactive program scope");
        }
        while self.call_frames.len() > call_depth {
            let scope = self
                .call_frames
                .last()
                .expect("active frame while above target depth")
                .job_scope;
            self.job_supervisor
                .cancel_scope(scope)
                .context("cancelling background jobs during reusable-program unwind")?;
            let frame = self.call_frames.pop().expect("call depth checked");
            *self.bindings = frame.bindings;
            *self.cwd = frame.cwd;
        }
        let popped = self.loop_frames.unwind_to(loop_depth, self.bindings)?;
        for (parent, scope) in popped {
            if collect_loop_jobs {
                self.job_supervisor.collect_loop_iteration(parent, scope)?;
            } else {
                self.job_supervisor.cancel_scope(scope)?;
            }
        }
        for name in drop_bindings {
            self.bindings.remove(&name);
        }
        self.state
            .apply_unwind(&control_flow::UnwindTo::new(call_depth, loop_depth));
        Ok(())
    }
}

fn apply_control_action(
    action: control_flow::ControlAction,
    mut context: ControlContext<'_, '_>,
) -> Result<AppliedControl> {
    fn remove_bindings(bindings: &mut BTreeMap<String, Value>, names: Vec<String>) {
        for name in names {
            bindings.remove(&name);
        }
    }

    match action {
        control_flow::ControlAction::Continue { drop_bindings } => {
            remove_bindings(context.bindings, drop_bindings);
            Ok(AppliedControl::Continue)
        }
        control_flow::ControlAction::Jump {
            target,
            transfer,
            unwind: target_scope,
        } => {
            let continuing = transfer == Some(control_flow::TransferKind::Continue);
            let collect_loop_jobs = matches!(
                transfer,
                Some(control_flow::TransferKind::Continue | control_flow::TransferKind::Break)
            );
            context.unwind(target_scope, collect_loop_jobs)?;
            if continuing {
                let Some(StepKind::EndLoop { back }) =
                    context.program.steps.get(target).map(|step| &step.kind)
                else {
                    bail!("continue target is not an end-loop marker");
                };
                let header = target
                    .checked_sub(*back)
                    .context("invalid continue loop edge")?;
                if let Some((parent, scope)) = context
                    .loop_frames
                    .end_iteration(header, context.bindings)?
                {
                    context
                        .job_supervisor
                        .collect_loop_iteration(parent, scope)?;
                }
            }
            Ok(AppliedControl::Jump(target))
        }
        control_flow::ControlAction::Catch {
            target,
            binding,
            error,
            job_scope,
            unwind: target_scope,
        } => {
            context
                .job_supervisor
                .cancel_scope_preserving(job_scope)
                .context("cancelling background jobs before entering catch")?;
            context.unwind(target_scope, false)?;
            context
                .bindings
                .insert(binding, execution_result::error_value(&error));
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
    #[cfg(target_os = "linux")]
    let _sandbox_probe_run = crate::platform::sandbox::begin_probe_run();
    if program.trigger.is_some() {
        bail!("triggered programs require run_service");
    }
    // TUI handlers run through this entry point and cannot hand over the terminal.
    if let Some(step) = program.steps.iter().find(|step| {
        matches!(
            step.kind,
            StepKind::Interactive | StepKind::HandOver | StepKind::LaunchDetached { .. }
        )
    }) {
        bail!(
            "line {}: interactively, hand over, and detached launch are not supported in TUI handlers",
            step.line
        );
    }
    validate_discard_stderr_usage(program)?;
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
        preflight_inputs_with_loop_limit(program, registry, &values, options.max_loop_iterations)?;
    }
    let mut signals = Signals::with_cancellation_and_deadline(
        cancelled,
        true,
        deadline_after(options.total_timeout, "total timeout")?,
    )?;
    // TUI handlers have finite executions. Jobs are scoped to the handler and
    // must be awaited or cancelled before it returns.
    signals.background_jobs_allowed = true;
    preload_program_modules(program, registry, &signals, &cwd)?;
    run_finite_state(
        FiniteRun {
            program,
            registry,
            options,
            signals: &signals,
        },
        FiniteStart {
            input: Input::Bytes(input),
            bindings,
            journal: None,
            state_source: FiniteStateSource::Fresh,
            state_out: None,
            job_supervisor: None,
            job_last_uses: None,
        },
    )
}
/// After a process exits, drain already-available diagnostics before taking the
/// stage snapshot. Stop when the pipe would block, even if another process has
/// inherited the writer. Captured bytes remain bounded by the selected stderr
/// policy, including the larger limit used for merged output.
#[cfg(any(unix, windows))]
fn drain_exited_diagnostic(child: &mut ManagedChild, signals: &Signals) -> Result<()> {
    loop {
        if child.stderr.is_none() || child.diagnostic_truncated {
            break;
        }
        check_deadline(signals.deadline)?;
        if signals.is_cancelled() {
            bail!("workflow cancelled while draining diagnostics");
        }
        let before = child.diagnostic.len();
        drain_diagnostic(child, signals)?;
        if child.diagnostic.len() == before {
            break;
        }
    }
    Ok(())
}

fn reserve_group_capture(budget: &AtomicUsize, limit: usize, requested: usize) -> Result<()> {
    let mut retained = budget.load(Ordering::Acquire);
    loop {
        let Some(next) = retained
            .checked_add(requested)
            .filter(|next| *next <= limit)
        else {
            bail!("captured output exceeds the shared group limit of {limit} bytes");
        };
        match budget.compare_exchange_weak(retained, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(observed) => retained = observed,
        }
    }
}

#[cfg(unix)]
fn drain_diagnostic(child: &mut ManagedChild, signals: &Signals) -> Result<()> {
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
                let limit = if FORCE_STDERR_CAPTURE.with(std::cell::Cell::get)
                    || crate::github_actions::enabled()
                {
                    FORCED_STDERR_CAPTURE_LIMIT
                } else {
                    MAX_DIAGNOSTIC_BYTES_PER_STAGE
                };
                let remaining = limit.saturating_sub(child.diagnostic.len());
                let keep = count.min(remaining);
                if let Some((budget, limit)) = &signals.group_diagnostic_budget {
                    reserve_group_capture(budget, *limit, keep)?;
                }
                child.diagnostic.extend_from_slice(&buffer[..keep]);
                child.diagnostic_truncated |= keep < count;
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
        // SAFETY: `observer` owns the kqueue descriptor that raw_fd returns, and it stays open for
        // this call. F_GETFD only reads the descriptor flags.
        let flags = unsafe { libc::fcntl(observer.raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_exited_child_wakes_observer_before_it_is_reaped() {
        let observer = ChildObserver::new().expect("creating test kqueue");
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .expect("spawning child");
        observer.observe(child.id()).expect("observing child");
        // The child is a zombie until waited on, so the kqueue is already readable.
        let mut descriptor = libc::pollfd {
            fd: observer.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` is a live pollfd and the count is 1. Its fd is the kqueue
        // owned by `observer`, which lives until the end of this test.
        let ready = unsafe { libc::poll(&mut descriptor, 1, 10_000) };
        assert_eq!(ready, 1);
        observer.drain().expect("draining exit notification");
        let mut quiet = libc::pollfd {
            fd: observer.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            // SAFETY: `quiet` is a live pollfd on the kqueue owned by `observer`, with a
            // count of 1 and a zero timeout, so the call returns without blocking.
            unsafe { libc::poll(&mut quiet, 1, 0) },
            0,
            "a drained observer must not stay readable"
        );
        // The observer did not reap the zombie. Killing its group is still safe;
        // Child::wait, rather than readiness, remains the exit-status authority.
        assert!(exited_unreaped(child.id()).expect("probing child without reaping"));
        kill_group(child.id());
        assert!(child.wait().expect("reaping child").success());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_repeated_short_lived_children_wake_observer() {
        // Short-lived `sh` children are the hang report's shape: each must wake
        // its own kqueue and reap promptly, with an outer deadline bounding the
        // whole sequence. This covers the wake path, not any proven root cause.
        let deadline = Instant::now() + Duration::from_secs(60);
        for iteration in 0..32 {
            assert!(
                Instant::now() < deadline,
                "short-lived children stalled at iteration {iteration}"
            );
            let observer = ChildObserver::new().expect("creating test kqueue");
            let mut child = Command::new("sh")
                .args(["-c", "printf 'fast\n'"])
                .process_group(0)
                .spawn()
                .expect("spawning child");
            observer.observe(child.id()).expect("observing child");
            let mut descriptor = libc::pollfd {
                fd: observer.raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `descriptor` is a live pollfd and the count is 1. Its fd is the
            // kqueue owned by `observer`, which lives until the end of this iteration.
            let ready = unsafe { libc::poll(&mut descriptor, 1, 10_000) };
            assert_eq!(
                ready, 1,
                "observer missed short-lived child at iteration {iteration}"
            );
            observer.drain().expect("draining exit notification");
            kill_group(child.id());
            assert!(child.wait().expect("reaping child").success());
        }
    }

    struct RestoreSigchld(libc::sigaction);
    impl Drop for RestoreSigchld {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the sigaction saved by this test's SIGCHLD query, a fully
            // initialized struct. It is passed by const pointer, and the null old-action pointer
            // is allowed.
            unsafe {
                libc::sigaction(libc::SIGCHLD, &self.0, std::ptr::null_mut());
            }
        }
    }

    #[test]
    #[ignore = "isolated process-wide SIGCHLD disposition check"]
    fn runtime_observer_preserves_host_sigchld_ignore() {
        // SAFETY: sigaction is a plain C struct of integers, a handler union and a signal mask, so
        // the all-zero bit pattern is valid. The query below overwrites it before any read.
        let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
        // SAFETY: all-zero is a valid sigaction, as above. The handler and mask are set explicitly
        // before the install call uses them.
        let mut ignored = unsafe { std::mem::zeroed::<libc::sigaction>() };
        // SAFETY: `previous` and `ignored` are live locals. The first call only queries SIGCHLD
        // into `previous`, since its new-action pointer is null. The second installs `ignored`,
        // whose mask was emptied by sigemptyset, and passes a null old-action pointer.
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

        // SAFETY: all-zero is a valid sigaction, and the query below overwrites it before use.
        let mut observed = unsafe { std::mem::zeroed::<libc::sigaction>() };
        // SAFETY: `observed` is a live, writable sigaction, and a null new-action pointer makes
        // this call a read-only query of the SIGCHLD disposition.
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
            discard_stderr: false,
            parameters: BTreeMap::from([("text".into(), Value::String("literal".into()))]),
            binding: None,
            cache: false,
            sensitive_binding: false,
            status_binding: None,
            result_binding: None,
            retry: None,
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
        // Empty PATH is a refusal rather than an implicit current-directory lookup.
        if path.is_empty() {
            bail!("executable {binary:?} not found in PATH");
        }
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
        let mut cache = LaunchPathCache::default();
        for _ in 0..2 {
            match (&optimized, cache.lookup(binary, cwd, path)) {
                (Ok(expected), Ok(actual)) => assert_eq!(expected.as_os_str(), actual.as_os_str()),
                (Err(expected), Err(actual)) => {
                    assert_eq!(expected.to_string(), actual.to_string())
                }
                (expected, actual) => {
                    panic!("cached lookup differs for PATH {path:?}: {expected:?}, {actual:?}")
                }
            }
        }
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
            // An empty PATH and one empty component have the same serialized
            // representation. Never interpret that ambiguous value as cwd.
            if entry.as_os_str().is_empty() {
                opt_manifest_assert_path_parity(binary, &cwd, &path);
                assert!(binary_path_from_path(binary, &cwd, &path).is_err());
                continue;
            }
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

    #[test]
    fn opt_manifest_untrusted_first_path_hit_fails_closed_without_fallthrough() {
        // Fail-before-effect for PATH search: the first executable hit is what
        // would run, so an untrusted first hit is refused even when a later
        // entry holds a trusted copy. Falling through would either launch a
        // different binary than the resolved one or mask the untrusted entry.
        let root = tempdir().unwrap();
        let cwd = root.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let binary = "probe";
        let untrusted = PathBuf::from("untrusted");
        let trusted = PathBuf::from("trusted");
        opt_manifest_write_file(&cwd.join(&untrusted).join(binary), 0o777).unwrap();
        opt_manifest_write_file(&cwd.join(&trusted).join(binary), 0o755).unwrap();
        let path = opt_manifest_join_path(&[untrusted, trusted]);
        let error = format!(
            "{:#}",
            binary_path_from_path(binary, &cwd, &path)
                .expect_err("untrusted first PATH hit must fail")
        );
        assert!(
            error.contains("untrusted") && error.contains(binary),
            "{error}"
        );
        let mut cache = LaunchPathCache::default();
        assert!(
            cache.lookup(binary, &cwd, &path).is_err(),
            "cached PATH search must fail closed on the same untrusted hit"
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
        // SAFETY: `fd` is the descriptor of `writer`, which is borrowed for the whole function, so
        // it stays open. F_GETFL only reads the status flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(
            // SAFETY: `fd` is still open, and `flags` was checked not to be -1 above. F_SETFL only
            // adds O_NONBLOCK to the status flags.
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

        // SAFETY: `fd` is still open, and `flags` is the status-flag value read at the start of
        // this function, so F_SETFL restores the pipe to its original mode.
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
    fn cooperative_applet_checkpoints_observe_ancestor_cancellation() {
        let local = Arc::new(AtomicBool::new(false));
        let ancestor = Arc::new(AtomicBool::new(false));
        let mut signals = Signals::with_cancellation_and_deadline(local, true, None).unwrap();
        signals.outer_cancelled.push(Arc::clone(&ancestor));
        ancestor.store(true, Ordering::Release);
        let checkpoint = || {
            if signals.is_cancelled() {
                Err(crate::applets::ExecutionBudgetExceeded::Cancelled)
            } else {
                Ok(())
            }
        };
        let context = crate::applets::ExecutionContext::new(&checkpoint);
        let document = serde_json::json!({
            "items": (0..512).map(|value| serde_json::json!({"value": value})).collect::<Vec<_>>()
        });
        let arguments = serde_json::json!({
            "document": document,
            "pointer": "/items",
            "select": "/value"
        })
        .as_object()
        .unwrap()
        .clone();

        let error = crate::applets::execute_with_context(
            "json.map",
            &arguments,
            &[],
            Path::new("."),
            &context,
        )
        .expect_err("an inherited cancellation must stop cooperative work");
        assert_eq!(error.kind, crate::applets::AppletErrorKind::Cancelled);
    }

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

#[cfg(test)]
mod resolved_json_argument_tests {
    use super::*;

    fn json_manifest() -> Manifest {
        let source = serde_json::json!({
            "name": "test.json_arguments",
            "binary": "unused",
            "parameters": {
                "first": {"type": "json", "required": true},
                "second": {"type": "json", "required": true}
            },
            "arguments": [{"literal": "test.json_arguments"}]
        })
        .to_string();
        Registry::from_sources(&[source])
            .unwrap()
            .resolve("test.json_arguments")
            .unwrap()
            .clone()
    }

    fn invocation(parameters: BTreeMap<String, Value>) -> Invocation {
        Invocation {
            operation: "test.json_arguments".into(),
            direct: false,
            discard_stderr: false,
            parameters,
            binding: None,
            cache: false,
            sensitive_binding: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        }
    }

    #[test]
    fn literal_json_arguments_preserve_nested_typed_values() {
        let manifest = json_manifest();
        let call = invocation(BTreeMap::from([
            (
                "first".into(),
                Value::Record(BTreeMap::from([
                    ("enabled".into(), Value::Bool(true)),
                    (
                        "items".into(),
                        Value::List(vec![
                            Value::Integer(4),
                            Value::Null,
                            Value::Number(serde_json::Number::from_f64(2.5).unwrap()),
                        ]),
                    ),
                ])),
            ),
            (
                "second".into(),
                Value::List(vec![
                    Value::String("literal".into()),
                    Value::Record(BTreeMap::from([(
                        "nested".into(),
                        Value::String("value".into()),
                    )])),
                ]),
            ),
        ]));

        let actual = resolved_json_arguments(&call, &manifest, &BTreeMap::new()).unwrap();
        let expected = serde_json::json!({
            "first": {"enabled": true, "items": [4, null, 2.5]},
            "second": ["literal", {"nested": "value"}]
        });
        assert_eq!(actual, expected.as_object().unwrap().clone());
    }

    #[test]
    fn direct_and_nested_references_resolve_inside_mixed_json_arguments() {
        let manifest = json_manifest();
        let call = invocation(BTreeMap::from([
            ("first".into(), Value::Reference("source".into())),
            (
                "second".into(),
                Value::Record(BTreeMap::from([
                    ("literal".into(), Value::Bool(false)),
                    (
                        "items".into(),
                        Value::List(vec![
                            Value::Reference("count".into()),
                            Value::Record(BTreeMap::from([(
                                "label".into(),
                                Value::Reference("label".into()),
                            )])),
                        ]),
                    ),
                ])),
            ),
        ]));
        let bindings = BTreeMap::from([
            ("source".into(), Value::String("resolved".into())),
            ("count".into(), Value::Integer(7)),
            ("label".into(), Value::String("nested".into())),
        ]);

        let actual = resolved_json_arguments(&call, &manifest, &bindings).unwrap();
        let expected = serde_json::json!({
            "first": "resolved",
            "second": {"literal": false, "items": [7, {"label": "nested"}]}
        });
        assert_eq!(actual, expected.as_object().unwrap().clone());
    }

    #[test]
    fn literal_validation_and_reference_resolution_keep_existing_error_order() {
        let manifest = json_manifest();
        let unknown = invocation(BTreeMap::from([(
            "unknown".into(),
            Value::String("value".into()),
        )]));
        let error = resolved_json_arguments(&unknown, &manifest, &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("has no parameter \"unknown\""));

        let referenced = invocation(BTreeMap::from([
            ("first".into(), Value::Integer(1)),
            (
                "second".into(),
                Value::List(vec![Value::Reference("missing".into())]),
            ),
        ]));
        let error = resolved_json_arguments(&referenced, &manifest, &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("undefined result \"missing\""));
    }
}

#[cfg(all(test, unix))]
mod typed_worker_json_reuse_tests {
    use super::*;

    #[test]
    fn typed_worker_result_reaches_take_without_a_second_json_parse() {
        let directory = tempfile::tempdir().expect("temporary worker directory");
        let worker_fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/json_lines_worker.py");
        let manifest = serde_json::json!({
            "name": "worker.typed_reuse",
            "binary": "python3",
            "parameters": {
                "payload": {
                    "type": "record",
                    "required": true,
                    "fields": {
                        "count": {"type": "integer"},
                        "label": {"type": "string"}
                    }
                }
            },
            "arguments": [
                {"literal": "fixture.echo"},
                {"literal": "--args"},
                {"json_parameter": "payload"}
            ],
            "contract": {
                "version": 1,
                "backend": "json_lines_worker",
                "output": {
                    "type": "record",
                    "encoding": "json",
                    "fields": {
                        "count": {"type": "integer"},
                        "label": {"type": "text"}
                    },
                    "required": ["count", "label"]
                },
                "lifecycle": {
                    "kind": "json_lines",
                    "protocol_version": 2,
                    "startup_arguments": [
                        worker_fixture.to_string_lossy(),
                        "--worker",
                        "echo_value",
                        directory.path().join("launches").to_string_lossy(),
                        directory.path().join("effects").to_string_lossy()
                    ]
                }
            }
        });
        let registry =
            Registry::from_sources(&[manifest.to_string()]).expect("valid v2 worker manifest");
        let program = crate::parser::parse(
            "run worker.typed_reuse with payload {\"count\":7,\"label\":\"typed\"} as result\ntake integer at \"/count\" from result as count\nthen encode count as json\n",
        )
        .expect("valid typed worker program");
        let options = RunOptions {
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(Duration::from_secs(5)),
            ..RunOptions::new(directory.path().to_path_buf())
        };

        worker_backend::reset_strict_json_parse_calls();
        let result = run(&program, &registry, &options)
            .expect("typed worker result should feed the following take");
        assert_eq!(result.output, b"7");
        assert_eq!(
            worker_backend::strict_json_parse_calls(),
            2,
            "the handshake and worker response each require one parse; the following `take` must reuse the retained response value without allocating a third parse tree"
        );
    }

    #[test]
    fn typed_program_module_result_reaches_take_without_a_second_json_parse() {
        let directory = tempfile::tempdir().expect("temporary module directory");
        let catalog = directory.path().join("applets");
        std::fs::create_dir_all(&catalog).expect("creating applet catalog");
        std::fs::write(
            catalog.join("module.program"),
            "define a program named entry\n  accept payload as json\n  then encode payload as json\n",
        )
        .expect("writing typed program module");
        let manifest = serde_json::json!({
            "name": "module.typed_reuse",
            "binary": "@program-module",
            "parameters": {
                "payload": {
                    "type": "record",
                    "required": true,
                    "fields": {"count": {"type": "integer"}}
                }
            },
            "arguments": [],
            "contract": {
                "version": 1,
                "backend": "program_module",
                "program": {
                    "source": "module.program",
                    "entry": "entry",
                    "interface_version": 1
                },
                "lifecycle": {"kind": "oneshot"},
                "output": {
                    "type": "record",
                    "encoding": "json",
                    "fields": {"count": {"type": "integer"}},
                    "required": ["count"]
                }
            }
        });
        std::fs::write(
            catalog.join("module.typed_reuse.json"),
            serde_json::to_vec(&manifest).expect("encoding module manifest"),
        )
        .expect("writing module manifest");
        let registry = Registry::load(&catalog).expect("loading module registry");
        let program = crate::parser::parse(
            "run module.typed_reuse with payload {\"count\":7} as result\ntake integer at \"/count\" from result as count\nthen encode count as json\n",
        )
        .expect("valid typed module program");
        let options = RunOptions {
            max_output_bytes: 1024 * 1024,
            ..RunOptions::new(directory.path().to_path_buf())
        };

        crate::contracts::reset_strict_json_parse_calls();
        let result = run(&program, &registry, &options)
            .expect("typed module result should feed the following take");
        assert_eq!(result.output, b"7");
        assert_eq!(
            crate::contracts::strict_json_parse_calls(),
            1,
            "the module boundary must parse JSON once and the caller must reuse its typed value"
        );
    }
}

#[cfg(test)]
mod output_limit_message_tests {
    use super::*;

    #[test]
    fn the_message_names_the_kind_of_step_that_overflowed() {
        let foreground = output_limit_message(false, 16);
        assert!(foreground.starts_with("foreground command output exceeds the limit of 16 bytes"));
        assert!(!foreground.contains("background"), "{foreground}");
        assert!(
            foreground.contains("streaming final `pipe to`"),
            "{foreground}"
        );
        assert!(foreground.contains("--max-output-bytes"), "{foreground}");

        {
            let _job = OutputScopeGuard::enter(OutputScope::BackgroundJob);
            let job = output_limit_message(false, 16);
            assert!(
                job.starts_with("background job output exceeds the limit"),
                "{job}"
            );
        }
        {
            let _task = OutputScopeGuard::enter(OutputScope::TaskGroup);
            let task = output_limit_message(true, 32);
            assert!(
                task.starts_with("task group output exceeds the shared group limit of 32"),
                "{task}"
            );
        }
        // The guard restores the foreground scope when it is dropped.
        assert!(output_limit_message(false, 1).starts_with("foreground command"));
        assert!(output_limit_message(false, 1).contains("streaming final `pipe to`"));
        assert!(!output_limit_message(false, 1).contains("then save to FILE"));
    }
}

#[cfg(test)]
mod atomic_write_target_tests {
    use super::atomic_write;
    use std::fs;

    #[test]
    fn save_refuses_hard_links_instead_of_splitting_the_link() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::write(&first, b"old").unwrap();
        fs::hard_link(&first, &second).unwrap();

        let error = atomic_write(&first, b"new").unwrap_err();

        assert!(format!("{error:#}").contains("hard-linked"));
        assert_eq!(fs::read(&first).unwrap(), b"old");
        assert_eq!(fs::read(&second).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o754)).unwrap();

        atomic_write(&target, b"new").unwrap();

        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o754
        );
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_symbolic_links_instead_of_replacing_the_link() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::write(&target, b"old").unwrap();
        symlink(&target, &link).unwrap();

        let error = atomic_write(&link, b"new").unwrap_err();

        assert!(format!("{error:#}").contains("symbolic link"));
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;

    fn options(directory: &Path) -> RunOptions {
        RunOptions {
            max_output_bytes: 4096,
            command_timeout: Some(Duration::from_secs(5)),
            total_timeout: Some(Duration::from_secs(10)),
            ..RunOptions::new(directory.to_owned())
        }
    }

    /// Runs `source` in a fresh directory with the drain state preset, without
    /// installing OS signal handlers. The directory keeps any files the run wrote.
    /// Each run owns its interrupt slot, so no process-wide lock or clear is
    /// needed: a concurrent probe cannot move this run off 130.
    fn run_with_drain(
        source: &str,
        state: u8,
        cancel: bool,
    ) -> (tempfile::TempDir, Result<RunResult>) {
        let directory = tempfile::tempdir().unwrap();
        let registry =
            Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
        let program = crate::parser::parse(source).unwrap();
        let signals =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(cancel)), false, None)
                .unwrap();
        signals.drain.store(state, Ordering::SeqCst);
        let result = run_finite_inner(
            FiniteRun {
                program: &program,
                registry: &registry,
                options: &options(directory.path()),
                signals: &signals,
            },
            Input::Empty,
            None,
            None,
        );
        (directory, result)
    }

    /// The interrupt body and the main body each leave a marker file, so the test can
    /// see which ran.
    const INTERRUPTIBLE: &str = "on interrupt\n  run printf with message \"drained\"\n  then save to file \"drained.txt\"\nrun printf with message \"main\"\nthen save to file \"main.txt\"\n";

    #[test]
    fn requested_drain_runs_only_the_interrupt_body() {
        let (directory, result) =
            run_with_drain(INTERRUPTIBLE, crate::platform::DRAIN_REQUESTED, false);
        let result = result.expect("a drain is a clean end, not an error");
        assert_eq!(result.exit.map(|exit| exit.status), Some(130));
        assert!(directory.path().join("drained.txt").exists());
        assert!(!directory.path().join("main.txt").exists());
    }

    #[test]
    fn run_without_an_interrupt_skips_the_interrupt_body() {
        let (directory, result) = run_with_drain(INTERRUPTIBLE, crate::platform::DRAIN_IDLE, false);
        assert!(result.expect("ordinary run").exit.is_none());
        assert!(directory.path().join("main.txt").exists());
        assert!(!directory.path().join("drained.txt").exists());
    }

    #[test]
    fn hard_cancel_skips_the_interrupt_body() {
        let (directory, result) =
            run_with_drain(INTERRUPTIBLE, crate::platform::DRAIN_CANCELLED, true);
        let error = result.err().expect("a second interrupt cancels the run");
        assert!(
            format!("{error:#}").contains("workflow cancelled by signal"),
            "{error:#}"
        );
        assert!(!directory.path().join("drained.txt").exists());
    }

    #[test]
    fn drain_runs_the_interrupt_body_then_the_exit_body() {
        let source = "on interrupt\n  run printf with message \"interrupted\"\n  then save to file \"interrupted.txt\"\non exit\n  run printf with message \"exit\"\n  then save to file \"exit.txt\"\nrun printf with message \"main\"\nthen save to file \"main.txt\"\n";
        let (directory, result) = run_with_drain(source, crate::platform::DRAIN_REQUESTED, false);
        assert_eq!(
            result
                .expect("drain through both cleanups")
                .exit
                .map(|exit| exit.status),
            Some(130)
        );
        assert!(directory.path().join("interrupted.txt").exists());
        assert!(directory.path().join("exit.txt").exists());
        assert!(!directory.path().join("main.txt").exists());
    }

    #[test]
    fn overlapping_signal_owners_keep_their_own_exit_codes() {
        // Two live signal-owning runs record different signals: with the old
        // process-wide slot the second record overwrote the first run's code.
        // The serial lock keeps the installed handlers' fallback writes away
        // from concurrent tests while both runs overlap. This injects per-slot
        // signals to prove slot independence; the real-delivery broadcast proof
        // is `both_overlapping_owners_observe_a_real_broadcast_signal` below.
        let _serial = crate::platform::CANCEL_SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let first = Signals::register_with_cancellation_and_deadline(
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .unwrap();
        let second = Signals::register_with_cancellation_and_deadline(
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .unwrap();
        first.record_cancel_signal(2);
        second.record_cancel_signal(15);
        assert_eq!(first.cancel_exit_code(), Some(130));
        assert_eq!(second.cancel_exit_code(), Some(143));
        assert_eq!(first.drain_exit_status(), 130);
        assert_eq!(second.drain_exit_status(), 143);
    }

    #[test]
    fn a_run_without_signal_handlers_ignores_an_earlier_runs_signal() {
        // A stale process-wide record must not leak into a later run that
        // never owned signal handlers: nested live, service, job, and worker
        // runs all build on `with_cancellation_and_deadline`.
        let _serial = crate::platform::CANCEL_SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::platform::record_cancel_signal(15);
        let signals =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(false)), false, None)
                .unwrap();
        assert_eq!(signals.cancel_exit_code(), None);
        crate::platform::clear_cancel_signal();
    }

    #[cfg(unix)]
    #[test]
    fn installed_handlers_record_into_their_own_run() {
        // End to end through real OS delivery: a SIGINT addressed to this
        // process lands in the owning run's slot and the CLI fallback, while
        // an overlapping run without handlers stays clean. Real delivery runs
        // in a child test process (see the invariant on
        // `CANCEL_SIGNAL_TEST_LOCK`) so the broadcast cannot reach handlers
        // installed by parallel tests in this process; the parent installs no
        // handlers and touches no fallback state.
        const HELPER_ENV: &str = "EXECUTE_TEST_INSTALLED_HANDLERS_CHILD";
        const TEST_NAME: &str =
            "runtime::drain_tests::installed_handlers_record_into_their_own_run";
        if std::env::var_os(HELPER_ENV).is_none() {
            let output = std::process::Command::new(
                std::env::current_exe().expect("current test executable"),
            )
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(HELPER_ENV, "1")
            .output()
            .expect("starting installed-handlers child");
            assert!(
                output.status.success(),
                "installed-handlers child failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Child process: the only place a real signal is raised for this test.
        // The `Drop` guard clears the process-wide fallback even when an
        // assertion below panics; the child then exits, so no fallback state
        // can leak into parallel tests in the parent process.
        struct ClearFallbackOnDrop;
        impl Drop for ClearFallbackOnDrop {
            fn drop(&mut self) {
                crate::platform::clear_cancel_signal();
            }
        }
        let _fallback_guard = ClearFallbackOnDrop;
        crate::platform::clear_cancel_signal();
        let signals = Signals::register_with_deadline(None).unwrap();
        let other =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(false)), false, None)
                .unwrap();
        // SAFETY: the installed handler only performs atomic stores and a
        // nonblocking wake write, so delivering a real SIGINT is safe.
        unsafe {
            libc::raise(libc::SIGINT);
        }
        // `signal_hook` relays through a background thread, so poll briefly.
        let deadline = Instant::now() + Duration::from_secs(5);
        while signals.cancel_exit_code().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(signals.cancel_exit_code(), Some(130));
        assert_eq!(signals.drain_exit_status(), 130);
        assert_eq!(crate::platform::cancel_exit_code(), Some(130));
        assert_eq!(other.cancel_exit_code(), None);
        drop(signals);
    }

    #[cfg(unix)]
    #[test]
    fn both_overlapping_owners_observe_a_real_broadcast_signal() {
        // Real OS delivery is process-wide broadcast: one SIGINT addressed to
        // this process runs every installed owner's handler set, so two
        // overlapping signal-owning runs each record the same signal in their
        // own slot (and the CLI fallback), while an overlapping run without
        // handlers stays clean. Real delivery runs in a child test process
        // (see the invariant on `CANCEL_SIGNAL_TEST_LOCK`) so the broadcast
        // cannot reach handlers installed by parallel tests in this process;
        // the parent installs no handlers and touches no fallback state.
        const HELPER_ENV: &str = "EXECUTE_TEST_BROADCAST_SIGNAL_CHILD";
        const TEST_NAME: &str =
            "runtime::drain_tests::both_overlapping_owners_observe_a_real_broadcast_signal";
        if std::env::var_os(HELPER_ENV).is_none() {
            let output = std::process::Command::new(
                std::env::current_exe().expect("current test executable"),
            )
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(HELPER_ENV, "1")
            .output()
            .expect("starting broadcast-signal child");
            assert!(
                output.status.success(),
                "broadcast-signal child failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Child process: the only place a real signal is raised for #554.
        // The `Drop` guard clears the process-wide fallback even when an
        // assertion below panics; the child then exits, so no fallback state
        // can leak into parallel tests in the parent process.
        struct ClearFallbackOnDrop;
        impl Drop for ClearFallbackOnDrop {
            fn drop(&mut self) {
                crate::platform::clear_cancel_signal();
            }
        }
        let _fallback_guard = ClearFallbackOnDrop;
        crate::platform::clear_cancel_signal();
        let first = Signals::register_with_cancellation_and_deadline(
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .unwrap();
        let second = Signals::register_with_cancellation_and_deadline(
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .unwrap();
        let other =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(false)), false, None)
                .unwrap();
        // SAFETY: both owners installed signal-safe handlers (atomic stores
        // plus a nonblocking wake write), so delivering a real SIGINT is safe.
        unsafe {
            libc::raise(libc::SIGINT);
        }
        // `signal_hook` relays through a background thread, so poll briefly.
        let deadline = Instant::now() + Duration::from_secs(5);
        while (first.cancel_exit_code().is_none() || second.cancel_exit_code().is_none())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(first.cancel_exit_code(), Some(130));
        assert_eq!(second.cancel_exit_code(), Some(130));
        assert_eq!(first.drain_exit_status(), 130);
        assert_eq!(second.drain_exit_status(), 130);
        assert_eq!(crate::platform::cancel_exit_code(), Some(130));
        assert_eq!(other.cancel_exit_code(), None);
        drop(first);
        drop(second);
    }

    #[test]
    fn restored_parallel_elements_are_not_started_again() {
        use crate::runtime::checkpoint::elements::ElementOutcome;
        use crate::runtime::tasks::{ElementPlan, GroupSink};

        #[derive(Default)]
        struct Recorder {
            begins: Vec<usize>,
            commits: Vec<usize>,
        }
        impl GroupSink for Recorder {
            fn begin(&mut self, index: usize) -> Result<()> {
                self.begins.push(index);
                Ok(())
            }
            fn commit(&mut self, index: usize, _outcome: &ElementOutcome) -> Result<()> {
                self.commits.push(index);
                Ok(())
            }
        }

        let source = "parallel for each word in [\"a\", \"b\", \"c\"] at most 2 as results\n  run printf with message word\n";
        let program = crate::parser::parse(source).unwrap();
        let registry =
            Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let signals =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(false)), false, None)
                .unwrap();
        let session =
            crate::modules::ProgramModuleSession::preload_program(&program, &registry).unwrap();
        *signals.program_modules.lock().unwrap() = Some(Arc::new(session));
        let (mode, limit, tasks, each) = program
            .steps
            .iter()
            .find_map(|step| match &step.kind {
                StepKind::TaskGroup {
                    mode,
                    limit,
                    tasks,
                    each,
                    ..
                } => Some((*mode, *limit, tasks.clone(), each.clone())),
                _ => None,
            })
            .expect("a task group step");
        let each = each.expect("a parallel for each");

        // Element 0 committed in an earlier run: its recorded result is restored.
        let restored = ElementOutcome {
            output: b"restored-a".to_vec(),
            stderr: Vec::new(),
            status: 0,
            outcome: crate::contracts::Outcome {
                status: 0,
                name: "printf".into(),
                kind: crate::contracts::OutcomeKind::Success,
                effect_certainty: crate::contracts::EffectCertainty::KnownResponse,
            },
            value: None,
            uncertain: false,
            launched_processes: 1,
        };
        let mut skip = std::collections::BTreeMap::new();
        skip.insert(0usize, restored);
        let mut recorder = Recorder::default();
        let result = super::tasks::run_each(
            super::tasks::TaskGroupRequest {
                config: super::tasks::TaskGroupConfig {
                    mode,
                    limit,
                    outer_deadline: None,
                },
                registry: &registry,
                bindings: &BTreeMap::new(),
                environment: None,
                sensitive_items: false,
                sensitive_environment: false,
                options: &options(directory.path()),
                cancelled: Arc::clone(&signals.cancelled),
                draining: Arc::new(AtomicU8::new(crate::platform::DRAIN_IDLE)),
                context: AppletInvocationContext::from_signals(&signals).unwrap(),
            },
            &each,
            &tasks[0],
            ElementPlan {
                skip,
                sink: Some(&mut recorder),
            },
        )
        .expect("a group with restored elements runs the rest");

        // Only the elements that did not commit begin, and only they commit.
        assert_eq!(recorder.begins, vec![1, 2]);
        // Commits follow completion order, which concurrent elements do not fix.
        let mut commits = recorder.commits.clone();
        commits.sort_unstable();
        assert_eq!(commits, vec![1, 2]);
        let value = crate::structured::to_json(&result.value).unwrap();
        let items = value["items"].as_array().unwrap();
        assert_eq!(items[0]["output_encoding"], "utf8");
        assert_eq!(items[0]["output"], "restored-a");
        assert_eq!(items[1]["output_encoding"], "utf8");
        assert_eq!(items[1]["output"], "b\n");
    }

    #[test]
    fn drained_parallel_for_each_admits_no_elements() {
        let source = "parallel for each word in [\"a\", \"b\", \"c\"] at most 2 as results\n  run printf with message word\n";
        let program = crate::parser::parse(source).unwrap();
        let registry =
            Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let signals =
            Signals::with_cancellation_and_deadline(Arc::new(AtomicBool::new(false)), false, None)
                .unwrap();
        let session =
            crate::modules::ProgramModuleSession::preload_program(&program, &registry).unwrap();
        *signals.program_modules.lock().unwrap() = Some(Arc::new(session));
        let (mode, limit, tasks, each) = program
            .steps
            .iter()
            .find_map(|step| match &step.kind {
                StepKind::TaskGroup {
                    mode,
                    limit,
                    tasks,
                    each,
                    ..
                } => Some((*mode, *limit, tasks.clone(), each.clone())),
                _ => None,
            })
            .expect("a task group step");
        let each = each.expect("a parallel for each");
        let result = super::tasks::run_each(
            super::tasks::TaskGroupRequest {
                config: super::tasks::TaskGroupConfig {
                    mode,
                    limit,
                    outer_deadline: None,
                },
                registry: &registry,
                bindings: &BTreeMap::new(),
                environment: None,
                sensitive_items: false,
                sensitive_environment: false,
                options: &options(directory.path()),
                cancelled: Arc::clone(&signals.cancelled),
                draining: Arc::new(AtomicU8::new(crate::platform::DRAIN_REQUESTED)),
                context: AppletInvocationContext::from_signals(&signals).unwrap(),
            },
            &each,
            &tasks[0],
            super::tasks::ElementPlan::none(),
        )
        .expect("a drained group reports its elements, not an error");
        let value = crate::structured::to_json(&result.value).unwrap();
        assert_eq!(result.launched_processes, 0);
        assert_eq!(value["not_started"], 3, "{value}");
    }

    #[test]
    fn drain_clock_expires_only_after_its_budget_and_restarts_with_a_new_drain() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let mut clock = DrainClock::new(ms(100));
        assert!(!clock.expired(crate::platform::DRAIN_IDLE, start));
        assert!(!clock.expired(crate::platform::DRAIN_REQUESTED, start));
        assert!(!clock.expired(crate::platform::DRAIN_REQUESTED, start + ms(99)));
        assert!(clock.expired(crate::platform::DRAIN_STARTED, start + ms(100)));
        // A cancelled or idle state resets the clock, so the next drain gets a full budget.
        assert!(!clock.expired(crate::platform::DRAIN_CANCELLED, start + ms(500)));
        assert!(!clock.expired(crate::platform::DRAIN_REQUESTED, start + ms(500)));
        assert!(!clock.expired(crate::platform::DRAIN_REQUESTED, start + ms(599)));
        assert!(clock.expired(crate::platform::DRAIN_REQUESTED, start + ms(600)));
    }

    #[test]
    fn watchdog_turns_an_expired_drain_into_a_cancel() {
        let drain = Arc::new(AtomicU8::new(crate::platform::DRAIN_REQUESTED));
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        spawn_drain_watchdog(
            Arc::clone(&drain),
            Arc::clone(&cancelled),
            Arc::new(AtomicBool::new(false)),
            Duration::from_millis(50),
            Arc::clone(&stop),
            None,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cancelled.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            cancelled.load(Ordering::SeqCst),
            "the drain never cancelled"
        );
        assert_eq!(
            drain.load(Ordering::SeqCst),
            crate::platform::DRAIN_CANCELLED
        );
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn watchdog_leaves_an_idle_run_alone() {
        let drain = Arc::new(AtomicU8::new(crate::platform::DRAIN_IDLE));
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        spawn_drain_watchdog(
            Arc::clone(&drain),
            Arc::clone(&cancelled),
            Arc::new(AtomicBool::new(false)),
            Duration::from_millis(10),
            Arc::clone(&stop),
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!cancelled.load(Ordering::SeqCst));
        assert_eq!(drain.load(Ordering::SeqCst), crate::platform::DRAIN_IDLE);
        stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod lane_v2_path_tests {
    use super::*;

    #[test]
    fn lane_v2_path_components_reused_but_filesystem_rechecked() {
        let root = tempfile::tempdir().unwrap();
        let early = root.path().join("early");
        let late = root.path().join("late");
        fs::create_dir(&early).unwrap();
        fs::create_dir(&late).unwrap();
        let name = if cfg!(windows) { "probe.exe" } else { "probe" };
        let write = |path: &Path| {
            fs::write(path, b"fixture").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        };
        write(&late.join(name));
        let path = std::env::join_paths([&early, &late]).unwrap();
        let mut cache = LaunchPathCache::default();
        assert_eq!(
            cache.lookup(name, root.path(), &path).unwrap(),
            late.join(name)
        );
        let storage = cache.entries[0].as_os_str().as_encoded_bytes().as_ptr();
        assert_eq!(
            cache.lookup(name, root.path(), &path).unwrap(),
            late.join(name)
        );
        assert_eq!(
            storage,
            cache.entries[0].as_os_str().as_encoded_bytes().as_ptr()
        );
        write(&early.join(name));
        assert_eq!(
            cache.lookup(name, root.path(), &path).unwrap(),
            early.join(name)
        );
        fs::remove_file(early.join(name)).unwrap();
        assert_eq!(
            cache.lookup(name, root.path(), &path).unwrap(),
            late.join(name)
        );
        fs::remove_file(late.join(name)).unwrap();
        assert_eq!(
            cache
                .lookup(name, root.path(), &path)
                .unwrap_err()
                .to_string(),
            format!("executable {name:?} not found in PATH")
        );
    }

    #[test]
    fn lane_v2_path_cache_invalidates_path_and_cwd() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = LaunchPathCache::default();
        for (cwd, path) in [
            (root.path().join("one"), OsStr::new("relative")),
            (root.path().join("two"), OsStr::new("relative")),
            (root.path().join("two"), OsStr::new("changed")),
            (root.path().join("two").join("."), OsStr::new("changed")),
        ] {
            assert!(cache.lookup("absent", &cwd, path).is_err());
            assert_eq!(cache.entries[0].as_os_str(), cwd.join(path).as_os_str());
        }
    }

    #[cfg(unix)]
    #[test]
    fn lane_v2_path_cache_preserves_symlink_alias_and_permission_changes() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"fixture").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let alias = root.path().join("alias");
        symlink(&target, &alias).unwrap();
        let mut cache = LaunchPathCache::default();
        let path = root.path().as_os_str();
        assert_eq!(cache.lookup("alias", root.path(), path).unwrap(), alias);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(cache.lookup("alias", root.path(), path).is_err());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(cache.lookup("alias", root.path(), path).unwrap(), alias);
    }

    #[cfg(windows)]
    #[test]
    fn lane_v2_path_cache_preserves_non_unicode_components() {
        use std::os::windows::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let entry = OsString::from_wide(&[b'x' as u16, 0xd800, b'y' as u16]);
        let mut cache = LaunchPathCache::default();
        assert!(cache.lookup("absent", root.path(), &entry).is_err());
        assert_eq!(
            cache.entries[0].as_os_str(),
            root.path().join(&entry).as_os_str()
        );
        assert!(cache.lookup("absent", root.path(), &entry).is_err());
        assert_eq!(
            cache.entries[0].as_os_str(),
            root.path().join(&entry).as_os_str()
        );
    }

    #[test]
    #[ignore = "cached PATH lookup microbenchmark; run in isolation"]
    fn lane_v2_path_lookup_benchmark() {
        let root = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { "probe.exe" } else { "probe" };
        fs::write(root.path().join(name), b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.path().join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        for prefix in [0, 16] {
            let path = std::env::join_paths(
                (0..prefix)
                    .map(|i| root.path().join(format!("missing-{i}")))
                    .chain(std::iter::once(root.path().to_path_buf())),
            )
            .unwrap();
            let registry = Registry::default();
            let mut samples = [Vec::new(), Vec::new()];
            for batch in 0..7 {
                for mode in if batch % 2 == 0 { [0, 1] } else { [1, 0] } {
                    let started = Instant::now();
                    for _ in 0..2000 {
                        let selected = if mode == 0 {
                            binary_path_from_path(name, root.path(), &path).unwrap()
                        } else {
                            binary_path_from_cached_path(name, root.path(), &path, &registry)
                                .unwrap()
                        };
                        std::hint::black_box(selected);
                    }
                    samples[mode].push(started.elapsed());
                }
            }
            for sample in &mut samples {
                sample.sort_unstable();
            }
            eprintln!(
                "PATH misses={prefix}: parsed {:?}; cached {:?}; 2000 lookups",
                samples[0][3], samples[1][3]
            );
        }
    }
}
