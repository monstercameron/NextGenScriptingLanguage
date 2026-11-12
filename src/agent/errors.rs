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
//! catch-all `execute_error` code in category `runtime`. A program that fails
//! static validation keeps its diagnostic: category `static`, the registry code
//! and the span.

use crate::ast::Program;
use crate::diagnostics::{Diagnostic, DiagnosticSeverity};
use crate::manifest::Registry;
use std::ffi::{OsStr, OsString};
use std::path::Path;
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

/// The first error diagnostic of a program that fails static validation. The
/// object reports its registry code, category `static` and its span.
#[derive(Debug)]
pub struct StaticCheckFailure {
    pub diagnostic: Diagnostic,
}

impl std::fmt::Display for StaticCheckFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "line {}: {}",
            self.diagnostic.span.line, self.diagnostic.message
        )
    }
}

impl std::error::Error for StaticCheckFailure {}

/// Static validation shared by `explain` and `inspect`. The first error
/// diagnostic fails the command before any output or effect.
pub fn static_gate(
    source: &str,
    path: &Path,
    program: &Program,
    registry: &Registry,
) -> anyhow::Result<()> {
    gate(source, path, program, registry, |_| true)
}

/// Static validation for `help`, which loads no manifests. Diagnostics that
/// need the registry or program files (`applet.*`, `check.program*`) are not
/// reported, so a program that only lacks manifests still gets its help.
pub fn static_gate_without_manifests(
    source: &str,
    path: &Path,
    program: &Program,
    registry: &Registry,
) -> anyhow::Result<()> {
    gate(source, path, program, registry, |diagnostic| {
        !(diagnostic.code.starts_with("applet.") || diagnostic.code.starts_with("check.program"))
    })
}

fn gate(
    source: &str,
    path: &Path,
    program: &Program,
    registry: &Registry,
    keep: impl Fn(&Diagnostic) -> bool,
) -> anyhow::Result<()> {
    let first_error = crate::diagnostics::analyze_source_at(source, Some(path), registry)
        .into_iter()
        .find(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error && keep(diagnostic));
    if let Some(diagnostic) = first_error {
        return Err(StaticCheckFailure { diagnostic }.into());
    }
    // The analyzer does not type every program call's result, but the validator
    // does. Its condition type error is reported as the static error it is, with
    // the span of its line. Other validation errors keep their own path.
    if let Err(error) = crate::runtime::validate(program, registry) {
        if let Some(diagnostic) = condition_type_error(source, &error) {
            return Err(StaticCheckFailure { diagnostic }.into());
        }
    }
    Ok(())
}

/// The `check.type_mismatch` diagnostic for a validator error of the form
/// `line N: condition compares ...`.
fn condition_type_error(source: &str, error: &anyhow::Error) -> Option<Diagnostic> {
    let text = format!("{error:#}");
    let (line, message) = text.strip_prefix("line ")?.split_once(": ")?;
    let line: usize = line.parse().ok()?;
    if !message.starts_with("condition compares ") {
        return None;
    }
    Some(Diagnostic {
        severity: DiagnosticSeverity::Error,
        code: "check.type_mismatch".to_owned(),
        message: message.to_owned(),
        span: crate::diagnostics::source_line_span(source, line),
        fixes: Vec::new(),
        related: Vec::new(),
    })
}

/// Render one stderr object. `interrupted` marks a run stopped by a signal, and
/// `exit_code` is the status the process is about to exit with.
pub fn render(error: &anyhow::Error, interrupted: bool, exit_code: i32) -> String {
    let static_failure = error
        .downcast_ref::<StaticCheckFailure>()
        .filter(|_| !interrupted);
    let (code, category) = match static_failure {
        Some(failure) => (failure.diagnostic.code.clone(), "static"),
        None if interrupted => ("interrupted".to_owned(), "signal"),
        None => {
            let (code, category) = classify(error);
            (code.to_owned(), category)
        }
    };
    let mut object = serde_json::json!({
        "protocol": PROTOCOL,
        "code": code,
        "category": category,
        "message": format!("{error:#}"),
        "exit_code": exit_code,
    });
    if let Some(failure) = static_failure {
        object["span"] =
            serde_json::to_value(&failure.diagnostic.span).unwrap_or(serde_json::Value::Null);
    }
    let mut line = crate::terminal_text::escape_json_for_terminal(&object.to_string());
    line.push('\n');
    line
}

/// Write the stderr object through the runtime's exit-diagnostic writer: this
/// object is rendered at process exit, so no more stderr follows and a
/// trailing held prefix is flushed rather than truncated.
pub fn write_stderr(error: &anyhow::Error, interrupted: bool, exit_code: i32) {
    crate::runtime::write_exit_diagnostic(render(error, interrupted, exit_code).as_bytes());
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_diagnostic_escapes_bidi_and_c1_without_changing_decoded_message() {
        let message = "bad \u{202e}override \u{2066}isolate \u{009b}csi \u{0085}nel";
        let error = anyhow::anyhow!("{message}");
        let rendered = super::render(&error, false, 1);
        for raw in ['\u{202e}', '\u{2066}', '\u{009b}', '\u{0085}'] {
            assert!(!rendered.contains(raw), "raw {raw:?} in JSON: {rendered:?}");
        }
        assert!(rendered.contains(r"\u202e"), "{rendered:?}");
        assert!(rendered.contains(r"\u009b"), "{rendered:?}");
        let parsed: serde_json::Value = serde_json::from_str(rendered.trim_end()).unwrap();
        assert_eq!(parsed["message"].as_str().unwrap(), message);
        assert_eq!(parsed["protocol"], super::PROTOCOL);
    }
}
