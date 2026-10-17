//! Repair-grade diagnostics: "did you mean" suggestions and machine-applicable
//! fixes. Each fix is applied to the source and the result is checked again, so a
//! suggestion that does not actually repair the program fails the test.

use flow::diagnostics::{analyze_source, Diagnostic, SuggestedFix};
use flow::manifest::Registry;
use std::path::Path;
use std::process::Command;

fn fixture_registry() -> Registry {
    Registry::from_sources(&[serde_json::json!({
        "name": "fixture.reader",
        "binary": "fixture-reader",
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {
            "arguments": { "type": "string", "required": true }
        },
        "arguments": [{ "parameter": "arguments" }]
    })
    .to_string()])
    .unwrap()
}

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
fn a_misspelled_parameter_is_renamed_and_the_fix_checks_clean() {
    let registry = fixture_registry();
    let source = "run fixture.reader with argumens \"x\"\n";
    let diagnostic = only_diagnostic(source, &registry, "applet.parameter.unknown");
    assert!(
        diagnostic.message.contains("did you mean \"arguments\""),
        "{}",
        diagnostic.message
    );
    let fix = diagnostic.fixes.first().expect("a rename fix");
    let repaired = apply(source, fix);
    assert_eq!(repaired, "run fixture.reader with arguments \"x\"\n");
    assert!(analyze_source(&repaired, &registry).is_empty());
}

#[test]
fn a_misspelled_applet_name_is_corrected() {
    let registry = fixture_registry();
    let source = "run fixture.readr with arguments \"x\"\n";
    let diagnostic = only_diagnostic(source, &registry, "applet.unknown");
    assert!(
        diagnostic
            .message
            .contains("did you mean \"fixture.reader\""),
        "{}",
        diagnostic.message
    );
    let repaired = apply(source, diagnostic.fixes.first().expect("a name fix"));
    assert_eq!(repaired, "run fixture.reader with arguments \"x\"\n");
    assert!(analyze_source(&repaired, &registry).is_empty());
}

#[test]
fn an_unquoted_executable_gets_a_quoting_fix() {
    let source = "echo hello\n";
    let diagnostic = only_diagnostic(source, &Registry::default(), "applet.unknown");
    assert!(
        diagnostic.message.contains("quote its name"),
        "{}",
        diagnostic.message
    );
    let fix = diagnostic.fixes.first().expect("a quoting fix");
    let repaired = apply(source, fix);
    assert_eq!(repaired, "run \"echo\" with arguments [\"hello\"]\n");
    assert!(
        analyze_source(&repaired, &Registry::default()).is_empty(),
        "the quoted executable should check clean"
    );
}

#[test]
fn a_misspelled_statement_keyword_is_corrected_on_a_parse_error() {
    let source = "rnu \"printf\" with arguments [\"a\"]\n";
    let diagnostic = only_diagnostic(source, &Registry::default(), "parse.syntax");
    assert!(
        diagnostic.message.contains("did you mean \"run\""),
        "{}",
        diagnostic.message
    );
    let repaired = apply(source, diagnostic.fixes.first().expect("a keyword fix"));
    assert_eq!(repaired, "run \"printf\" with arguments [\"a\"]\n");
    assert!(analyze_source(&repaired, &Registry::default()).is_empty());
}

#[test]
fn a_misspelled_keyword_before_a_bare_applet_phrase_is_corrected() {
    let source = "rnu printf with arguments [\"a\"]\n";
    let diagnostic = only_diagnostic(source, &Registry::default(), "applet.unknown");
    assert!(
        diagnostic
            .message
            .contains("did you mean the statement keyword \"run\""),
        "{}",
        diagnostic.message
    );
    let repaired = apply(source, diagnostic.fixes.first().expect("a keyword fix"));
    assert_eq!(repaired, "run printf with arguments [\"a\"]\n");
}

#[test]
fn check_json_carries_the_fixes_with_spans() {
    let directory = std::env::temp_dir().join(format!("diagnostic-fixes-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let program = directory.join("typo.program");
    std::fs::write(&program, "run text.trim_finall_lf with text \"x\"\n").unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("check")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(&manifests)
        .arg("--json")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&directory);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostic = &report["diagnostics"][0];
    assert_eq!(diagnostic["code"], "applet.unknown");
    let fix = &diagnostic["fixes"][0];
    assert_eq!(fix["edits"][0]["replacement"], "text.trim_final_lf");
    assert_eq!(fix["edits"][0]["span"]["start_byte"], 4);
    assert_eq!(
        fix["edits"][0]["span"]["end_byte"],
        4 + "text.trim_finall_lf".len()
    );
}

#[test]
fn diagnostics_without_a_fix_keep_their_json_shape() {
    let diagnostics = analyze_source("run\n", &Registry::default());
    let value = serde_json::to_value(&diagnostics[0]).unwrap();
    assert!(
        value.get("fixes").is_none(),
        "empty fixes are omitted: {value}"
    );
}
