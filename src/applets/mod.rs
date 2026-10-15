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
    match operation {
        "path.dirname" => path::dirname(args),
        "path.basename" => path::basename(args),
        "path.remove_suffix" => path::remove_suffix(args),
        "text.trim_final_lf" | "text.normalize" => text::trim_final_lf(args, input),
        "text.concat" => text::concat(args),
        "text.matches" => text::matches(args, input),
        "number.add" | "number.subtract" | "number.multiply" | "number.divide"
        | "number.compare" => arithmetic::run(operation, args),
        "number.parse_integer" | "number.parse_integer.result" => {
            arithmetic::parse_integer(args, input)
        }
        "number.to_text" => arithmetic::to_text(args),
        "color.rgb_to_hex" => arithmetic::rgb_to_hex(args),
        "color.parse_rgb" => arithmetic::parse_rgb(args, input),
        "color.parse_rgb.result" => arithmetic::parse_rgb_result(args, input),
        "json.parse" => structured::parse(args, input),
        "json.stringify" => structured::stringify(args),
        "json.get" => structured::get(args, input),
        "json.set" => structured::set(args, input),
        "json.map" => structured::map(args, input),
        "json.filter" => structured::filter(args, input),
        "jsonc.edit_content" => jsonc::edit_content(args, input),
        "fs.transaction" => transaction::apply(args, cwd),
        "fs.transaction.recover" => transaction::recover(args, cwd),
        "fs.read" | "fs.snapshot" => filesystem::read(args, cwd),
        "fs.write_atomic" => filesystem::write_atomic(args, input, cwd),
        "fs.unlink_checked" => filesystem::unlink_checked(args, cwd),
        "jsonc.set" => jsonc::set(args, cwd),
        "jsonc.remove" => jsonc::remove(args, cwd),
        _ => Err(AppletError::failed(
            "unknown_operation",
            format!("unknown operation {operation:?}"),
        )),
    }
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
