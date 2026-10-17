//! GitHub Actions workflow-command annotations for `execute check --format github`.
//!
//! The converter reads the same diagnostic values that `--json` prints, so the two
//! outputs cannot disagree. Each diagnostic becomes one `::error`, `::warning` or
//! `::notice` line carrying the file and, when the diagnostic has a span, its line
//! and column. Property and message text is escaped as the workflow-command format
//! requires, so a diagnostic message cannot inject another command.

use anyhow::Result;
use serde_json::Value;
use std::io::{self, Write};

/// Write one annotation per diagnostic to standard output.
pub fn write(path: &str, diagnostics: &[Value]) -> Result<()> {
    let file = repository_path(path);
    let mut out = io::stdout().lock();
    for diagnostic in diagnostics {
        writeln!(out, "{}", annotation(&file, diagnostic))?;
    }
    Ok(())
}

/// GitHub maps annotations to repository files by relative path with forward
/// slashes, so a file under the working directory is named relative to it.
pub fn repository_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = cwd.to_string_lossy().replace('\\', "/");
        let prefix = format!("{}/", cwd.trim_end_matches('/'));
        if let Some(relative) = normalized.strip_prefix(&prefix) {
            return relative.to_owned();
        }
    }
    normalized
}

/// Format one diagnostic as a workflow command.
pub fn annotation(path: &str, diagnostic: &Value) -> String {
    let level = match diagnostic["severity"].as_str() {
        Some("warning") => "warning",
        Some("information" | "hint") => "notice",
        _ => "error",
    };
    let code = diagnostic["code"].as_str().unwrap_or("diagnostic");
    let message = diagnostic["message"].as_str().unwrap_or("");
    let mut properties = vec![format!("file={}", escape_property(path))];
    let span = &diagnostic["span"];
    for (key, field) in [
        ("line", "line"),
        ("col", "column"),
        ("endLine", "end_line"),
        ("endColumn", "end_column"),
    ] {
        if let Some(value) = span[field].as_u64() {
            properties.push(format!("{key}={value}"));
        }
    }
    properties.push(format!("title={}", escape_property(code)));
    format!(
        "::{level} {}::{}",
        properties.join(","),
        escape_message(message)
    )
}

/// Escape message data: `%`, carriage return and line feed.
pub fn escape_message(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escape a property value: message escaping plus the separators `:` and `,`.
pub fn escape_property(text: &str) -> String {
    escape_message(text).replace(':', "%3A").replace(',', "%2C")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn severity_maps_to_workflow_levels_and_span_becomes_properties() {
        let diagnostic = json!({
            "severity": "error",
            "code": "unknown_applet",
            "message": "no applet named x",
            "span": {"line": 3, "column": 5, "end_line": 3, "end_column": 9}
        });
        assert_eq!(
            annotation("flow.program", &diagnostic),
            "::error file=flow.program,line=3,col=5,endLine=3,endColumn=9,title=unknown_applet::no applet named x"
        );
        let warning = json!({"severity": "warning", "code": "w", "message": "careful"});
        assert_eq!(
            annotation("f", &warning),
            "::warning file=f,title=w::careful"
        );
        let hint = json!({"severity": "hint", "code": "h", "message": "tip"});
        assert!(annotation("f", &hint).starts_with("::notice "));
    }

    #[test]
    fn message_and_property_text_cannot_inject_commands() {
        let diagnostic = json!({
            "severity": "error",
            "code": "bad,code:x",
            "message": "first\n::error::injected 100%"
        });
        let line = annotation("dir:one,two.program", &diagnostic);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("file=dir%3Aone%2Ctwo.program"), "{line}");
        assert!(line.contains("title=bad%2Ccode%3Ax"), "{line}");
        assert!(
            line.ends_with("::first%0A::error::injected 100%25"),
            "{line}"
        );
    }

    #[test]
    fn repository_paths_are_relative_with_forward_slashes() {
        let cwd = std::env::current_dir().unwrap();
        let inside = format!(
            "{}/examples/a.program",
            cwd.to_string_lossy().replace('\\', "/")
        );
        assert_eq!(repository_path(&inside), "examples/a.program");
        assert_eq!(repository_path("examples\\b.program"), "examples/b.program");
        assert_eq!(
            repository_path("/elsewhere/c.program"),
            "/elsewhere/c.program"
        );
    }

    #[test]
    fn check_failed_without_a_span_still_annotates_the_file() {
        let diagnostic = json!({"severity": "error", "code": "check_failed", "message": "boom"});
        assert_eq!(
            annotation("p.program", &diagnostic),
            "::error file=p.program,title=check_failed::boom"
        );
    }
}
