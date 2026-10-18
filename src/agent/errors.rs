//! `--json-errors`: a structured JSON object as the final stderr line of a
//! failed run.
//!
//! The flag is recognised only before the first `--`. It is removed from the
//! option arguments before parsing, and changes only how the final error is
//! written. Exit statuses are unchanged. After `--`, the same spelling is a
//! literal program argument.
//!
//! Child stderr is forwarded unchanged, in both modes. A failing command's own
//! stderr is written first, as usual. Only Execute's `execute: …` diagnostic
//! line is replaced by the JSON object. Machines should therefore read the
//! final stderr line, not the whole stream. When no child wrote stderr, the
//! object is the only line.
//!
//! Stable codes come from the typed error that caused the failure. Errors
//! without a typed source, such as parse and validation errors, use the
//! catch-all `execute_error` code in category `runtime`.

use std::ffi::{OsStr, OsString};
use std::sync::atomic::{AtomicBool, Ordering};

/// Protocol identifier of the stderr object.
pub const PROTOCOL: &str = "execute.error/1";
/// The global flag, matched as a whole argument.
pub const FLAG: &str = "--json-errors";

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Remove every `--json-errors` argument and record the request. The remaining
/// arguments keep their order.
pub fn strip_flag<I>(arguments: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let flag = OsStr::new(FLAG);
    let mut requested = false;
    let kept = arguments
        .into_iter()
        .filter(|argument| {
            if argument.as_os_str() == flag {
                requested = true;
                false
            } else {
                true
            }
        })
        .collect();
    if requested {
        REQUESTED.store(true, Ordering::Relaxed);
    }
    kept
}

/// Whether `--json-errors` was given to this process.
pub fn enabled() -> bool {
    REQUESTED.load(Ordering::Relaxed)
}

/// The stable `(code, category)` pair for an error chain.
pub fn classify(error: &anyhow::Error) -> (&'static str, &'static str) {
    use crate::runtime::{AppletInvocationError, CommandFailed};
    if error.downcast_ref::<CommandFailed>().is_some() {
        return ("command_failed", "process");
    }
    if error.downcast_ref::<AppletInvocationError>().is_some() {
        return ("applet_invocation_failed", "applet");
    }
    if error
        .downcast_ref::<crate::applets::AppletError>()
        .is_some()
    {
        return ("applet_failed", "applet");
    }
    if error
        .downcast_ref::<crate::security::PolicyError>()
        .is_some()
    {
        return ("policy_denied", "policy");
    }
    // RemoteApplicationError has no Display, so anyhow cannot downcast to it.
    // Remote application failures are reported under the generic code.
    if error
        .downcast_ref::<crate::contracts::RemoteError>()
        .is_some()
    {
        return ("remote_failed", "remote");
    }
    #[cfg(feature = "live")]
    if error
        .downcast_ref::<crate::live::auth::AuthError>()
        .is_some()
    {
        return ("auth_failed", "auth");
    }
    if error
        .downcast_ref::<crate::runtime::execution_result::ExecutionFailure>()
        .is_some()
    {
        return ("execution_failed", "execution");
    }
    if error
        .downcast_ref::<crate::runtime::tasks::TaskGroupFailure>()
        .is_some()
    {
        return ("task_failed", "execution");
    }
    ("execute_error", "runtime")
}

/// Render one stderr object. `interrupted` marks a run stopped by a signal, and
/// `exit_code` is the status the process is about to exit with.
pub fn render(error: &anyhow::Error, interrupted: bool, exit_code: i32) -> String {
    let (code, category) = if interrupted {
        ("interrupted", "signal")
    } else {
        classify(error)
    };
    let mut line = serde_json::json!({
        "protocol": PROTOCOL,
        "code": code,
        "category": category,
        "message": format!("{error:#}"),
        "exit_code": exit_code,
    })
    .to_string();
    line.push('\n');
    line
}

/// Write the stderr object through the runtime's diagnostic writer.
pub fn write_stderr(error: &anyhow::Error, interrupted: bool, exit_code: i32) {
    crate::runtime::write_diagnostic(render(error, interrupted, exit_code).as_bytes());
}
