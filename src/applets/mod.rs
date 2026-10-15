//! Small, reusable operations for `.program` workflows.
//!
//! These functions intentionally contain no application-specific policy. The
//! `.program` workflow owns sequencing, retries, and decisions about which
//! operation to call.

mod arithmetic;
mod filesystem;
mod jsonc;
mod path;
mod structured;
mod text;
mod transaction;

use serde_json::{Map, Value};
use std::fmt;
use std::path::Path;

/// The execution budget stopped an in-process applet at a cooperative
/// checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionBudgetExceeded {
    Timeout,
    Cancelled,
}

/// Scoped checkpoint policy for an in-process applet execution.
///
/// The callback is borrowed for one synchronous invocation and is never
/// installed in process-global state. Applets call it periodically while
/// traversing bounded CPU work. It cannot interrupt an operation that is
/// blocked inside an operating-system call or a library routine that does not
/// expose checkpoints.
pub struct ExecutionContext<'a> {
    checkpoint: &'a dyn Fn() -> Result<(), ExecutionBudgetExceeded>,
}

impl<'a> ExecutionContext<'a> {
    /// Create a context whose callback checks the caller's deadline or
    /// cancellation state.
    pub fn new(checkpoint: &'a dyn Fn() -> Result<(), ExecutionBudgetExceeded>) -> Self {
        Self { checkpoint }
    }

    /// Create a context for trusted direct calls that have no runtime budget.
    pub fn unrestricted() -> ExecutionContext<'static> {
        ExecutionContext {
            checkpoint: &always_continue,
        }
    }

    fn checkpoint(&self) -> Result<(), AppletError> {
        (self.checkpoint)().map_err(AppletError::budget_exceeded)
    }
}

fn always_continue() -> Result<(), ExecutionBudgetExceeded> {
    Ok(())
}

const COOPERATIVE_CHECK_INTERVAL: usize = 256;

pub(crate) fn checkpoint_work(
    context: &ExecutionContext<'_>,
    work: usize,
) -> Result<(), AppletError> {
    if work.is_multiple_of(COOPERATIVE_CHECK_INTERVAL) {
        context.checkpoint()?;
    }
    Ok(())
}

/// A stable error envelope for the standalone helper protocol.
#[derive(Debug)]
pub struct AppletError {
    pub code: &'static str,
    pub kind: AppletErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppletErrorKind {
    Absence,
    Permanent,
    Retryable,
    Uncertain,
    Timeout,
    Cancelled,
}

impl AppletError {
    pub fn exit_status(&self) -> i32 {
        match self.kind {
            AppletErrorKind::Absence => 1,
            AppletErrorKind::Permanent => 2,
            AppletErrorKind::Retryable => 75,
            // Keep uncertain effects distinct from retryable conflicts so
            // contracts never imply that replay is safe.
            AppletErrorKind::Uncertain => 76,
            AppletErrorKind::Timeout => 124,
            AppletErrorKind::Cancelled => 130,
        }
    }
}

impl AppletError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            kind: AppletErrorKind::Permanent,
            message: message.into(),
        }
    }

    fn failed(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind: match code {
                "conflict" | "lock_timeout" => AppletErrorKind::Retryable,
                "uncertain_effect" | "manual_recovery" => AppletErrorKind::Uncertain,
                _ => AppletErrorKind::Permanent,
            },
            message: message.into(),
        }
    }

    fn absence(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind: AppletErrorKind::Absence,
            message: message.into(),
        }
    }

    fn budget_exceeded(exceeded: ExecutionBudgetExceeded) -> Self {
        match exceeded {
            ExecutionBudgetExceeded::Timeout => Self {
                code: "timeout",
                kind: AppletErrorKind::Timeout,
                message: "execution deadline exceeded; applet effects may be incomplete".into(),
            },
            ExecutionBudgetExceeded::Cancelled => Self {
                code: "cancelled",
                kind: AppletErrorKind::Cancelled,
                message: "workflow cancelled; applet effects may be incomplete".into(),
            },
        }
    }
}

impl fmt::Display for AppletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppletError {}

/// Run one generic operation. Workflow control and operation ordering belong
/// to the calling `.program` or runtime.
pub fn execute(
    operation: &str,
    args: &Map<String, Value>,
    input: &[u8],
) -> Result<Value, AppletError> {
    let cwd = std::env::current_dir()
        .map_err(|error| io_error(error, "getting the process working directory"))?;
    execute_in_cwd(operation, args, input, &cwd)
}

/// Run one generic operation with an explicit base directory for relative
/// filesystem paths. This is safe for parallel runtimes because it never
/// changes process-global current-directory state.
pub fn execute_in_cwd(
    operation: &str,
    args: &Map<String, Value>,
    input: &[u8],
    cwd: &Path,
) -> Result<Value, AppletError> {
    execute_with_context(
        operation,
        args,
        input,
        cwd,
        &ExecutionContext::unrestricted(),
    )
}

/// Run one generic operation with an explicit working directory and scoped
/// cooperative execution context.
pub fn execute_with_context(
    operation: &str,
    args: &Map<String, Value>,
    input: &[u8],
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let result = match operation {
        "path.dirname" => path::dirname(args),
        "path.basename" => path::basename(args),
        "path.remove_suffix" => path::remove_suffix(args),
        "text.trim_final_lf" | "text.normalize" => text::trim_final_lf(args, input),
        "text.concat" => text::concat(args, context),
        "text.matches" => text::matches(args, input, context),
        "number.add" | "number.subtract" | "number.multiply" | "number.divide"
        | "number.compare" => arithmetic::run(operation, args),
        "number.parse_integer" | "number.parse_integer.result" => {
            arithmetic::parse_integer(args, input)
        }
        "number.to_text" => arithmetic::to_text(args),
        "color.rgb_to_hex" => arithmetic::rgb_to_hex(args),
        "color.parse_rgb" => arithmetic::parse_rgb(args, input),
        "color.parse_rgb.result" => arithmetic::parse_rgb_result(args, input),
        "json.parse" => structured::parse(args, input, context),
        "json.stringify" => structured::stringify(args, context),
        "json.get" => structured::get(args, input, context),
        "json.set" => structured::set(args, input, context),
        "json.map" => structured::map(args, input, context),
        "json.filter" => structured::filter(args, input, context),
        "jsonc.edit_content" => jsonc::edit_content(args, input, context),
        "fs.transaction" => transaction::apply(args, cwd),
        "fs.transaction.recover" => transaction::recover(args, cwd),
        "fs.read" | "fs.snapshot" => filesystem::read(args, cwd),
        "fs.write_atomic" => filesystem::write_atomic(args, input, cwd),
        "fs.unlink_checked" => filesystem::unlink_checked(args, cwd),
        "jsonc.set" => jsonc::set(args, cwd, context),
        "jsonc.remove" => jsonc::remove(args, cwd, context),
        _ => Err(AppletError::failed(
            "unknown_operation",
            format!("unknown operation {operation:?}"),
        )),
    };
    if result.is_ok() {
        context.checkpoint()?;
    }
    result
}

pub(crate) fn required_string<'a>(
    args: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, AppletError> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| AppletError::invalid(format!("args.{name} must be a string")))
}

pub(crate) fn io_error(error: std::io::Error, context: impl fmt::Display) -> AppletError {
    if error.kind() == std::io::ErrorKind::NotFound {
        AppletError::absence("not_found", format!("{context}: {error}"))
    } else if matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::StorageFull
    ) {
        AppletError {
            code: "io_error",
            kind: AppletErrorKind::Retryable,
            message: format!("{context}: {error}"),
        }
    } else {
        AppletError::failed("io_error", format!("{context}: {error}"))
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> AppletError {
    AppletError::invalid(message)
}

pub(crate) fn failed(code: &'static str, message: impl Into<String>) -> AppletError {
    AppletError::failed(code, message)
}

pub(crate) fn absence(code: &'static str, message: impl Into<String>) -> AppletError {
    AppletError::absence(code, message)
}
