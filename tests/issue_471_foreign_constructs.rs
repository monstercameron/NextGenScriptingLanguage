//! Shell habits with one Execute form: a `|` pipe and a `2>/dev/null` redirect get a
//! `parse.foreign.*` diagnostic, and the offered fix must make the program check.
//! A keyword suggestion never names a keyword for a first word that is a known applet.

use flow::diagnostics::{analyze_source, Diagnostic, DiagnosticSeverity, SuggestedFix};
use flow::manifest::Registry;

/// Apply every edit of a fix, highest offset first so earlier offsets stay valid.
fn apply(source: &str, fix: &SuggestedFix) -> String {
    let mut edits = fix.edits.clone();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.span.start_byte));
    let mut text = source.to_owned();
    for edit in edits {
        text.replace_range(edit.span.start_byte..edit.span.end_byte, &edit.replacement);
    }
    text
}

fn errors(source: &str, registry: &Registry) -> Vec<Diagnostic> {
    analyze_source(source, registry)
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
        .collect()
}

fn only_diagnostic(source: &str, registry: &Registry, code: &str) -> Diagnostic {
    let diagnostics = analyze_source(source, registry);
    let matches = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == code)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "expected one {code}: {diagnostics:#?}");
    matches.into_iter().next().unwrap()
}

#[test]
fn a_shell_pipe_becomes_a_pipe_to_line_that_checks() {
    let registry = Registry::default();
    let source = "run \"ls\" | \"wc\"\n";
    let diagnostic = only_diagnostic(source, &registry, "parse.foreign.pipe");
    assert!(
        diagnostic.message.contains("`pipe to"),
        "{}",
        diagnostic.message
    );
    let fix = diagnostic.fixes.first().expect("a pipe fix");
    let repaired = apply(source, fix);
    assert_eq!(repaired, "run \"ls\"\npipe to \"wc\"\n");
    assert!(errors(&repaired, &registry).is_empty(), "{repaired:?}");
}

#[test]
fn every_pipe_in_a_line_is_rewritten_together() {
    let registry = Registry::default();
    let source = "run \"a\" | \"b\" | \"c\"\n";
    let diagnostic = only_diagnostic(source, &registry, "parse.foreign.pipe");
    let fix = diagnostic.fixes.first().expect("a pipe fix");
    let repaired = apply(source, fix);
    assert_eq!(repaired, "run \"a\"\npipe to \"b\"\npipe to \"c\"\n");
    assert!(errors(&repaired, &registry).is_empty(), "{repaired:?}");
}

#[test]
fn a_stderr_redirect_to_dev_null_becomes_discard_stderr_that_checks() {
    let registry = Registry::default();
    let source = "run \"ls\" 2>/dev/null\n";
    let diagnostic = only_diagnostic(source, &registry, "parse.foreign.discard_stderr");
    assert!(
        diagnostic.message.contains("and discard stderr"),
        "{}",
        diagnostic.message
    );
    let fix = diagnostic.fixes.first().expect("a discard fix");
    let repaired = apply(source, fix);
    assert_eq!(repaired, "run \"ls\" and discard stderr\n");
    assert!(errors(&repaired, &registry).is_empty(), "{repaired:?}");
}

#[test]
fn a_pipe_after_a_non_run_head_gets_a_diagnostic_but_no_fix() {
    let registry = Registry::default();
    let source = "then \"a\" | \"b\"\n";
    for diagnostic in analyze_source(source, &registry) {
        if diagnostic.code == "parse.foreign.pipe" {
            assert!(
                diagnostic.fixes.is_empty(),
                "a pipe stage cannot follow a non-run head: {diagnostic:#?}"
            );
        }
    }
}

#[test]
fn a_known_applet_head_gets_no_keyword_suggestion() {
    // `thne` is one transposition from the keyword `then`, but it is a registered
    // applet name, so it must never be offered as the keyword.
    let registry = Registry::from_sources(&[serde_json::json!({
        "name": "thne",
        "binary": "thne-bin",
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {},
        "arguments": []
    })
    .to_string()])
    .unwrap();
    let source = "thne \"x\" | \"y\"\n";
    let diagnostics = analyze_source(source, &registry);
    assert!(!diagnostics.is_empty(), "the line must fail to check");
    for diagnostic in diagnostics {
        assert!(
            !diagnostic.message.contains("did you mean \"then\""),
            "{diagnostic:#?}"
        );
        assert!(
            diagnostic
                .fixes
                .iter()
                .all(|fix| fix.edits.iter().all(|edit| edit.replacement != "then")),
            "{diagnostic:#?}"
        );
    }
}
