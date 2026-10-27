//! Repair-grade diagnostics: "did you mean" suggestions and machine-applicable
//! fixes. Each fix is applied to the source and the result is checked again, so a
//! suggestion that does not actually repair the program fails the test.

use flow::diagnostics::{Diagnostic, SuggestedFix, analyze_source};
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

#[test]
fn semantic_parser_errors_have_specific_codes_and_token_spans() {
    for (source, code, token) in [
        ("break\n", "check.scope", "break"),
        ("await job1\n", "check.scope", "job1"),
        (
            "step: run \"printf\" with arguments [\"x\"]\nstep: run \"printf\" with arguments [\"y\"]\n",
            "check.duplicate_name",
            "step",
        ),
        (
            "accept input as text\naccept input as text\n",
            "check.duplicate_name",
            "input",
        ),
        (
            "define worker\n  run \"true\"\ndefine worker\n  run \"true\"\n",
            "check.duplicate_name",
            "worker",
        ),
        (
            "run \"true\" as result\nrun \"true\" as result\n",
            "check.duplicate_name",
            "result",
        ),
        ("exit with status 300\n", "check.range", "status"),
        (
            "parallel at most 0 as group\n  run fixture.count with count 1 as task\n",
            "check.range",
            "0",
        ),
        (
            "accept color as text one of [\"red\"] with default \"blue\"\n",
            "check.value.constraint",
            "\"blue\"",
        ),
        (
            "run \"true\" with item \"a\" and item \"b\"\n",
            "check.duplicate_name",
            "item",
        ),
        (
            "define worker\n  run program worker\n",
            "check.program.cycle",
            "worker",
        ),
        (
            "define worker\n  run \"true\"\n\nrun program workre\n",
            "check.program.unknown",
            "workre",
        ),
    ] {
        let diagnostics = analyze_source(source, &Registry::default());
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == code)
            .unwrap_or_else(|| panic!("expected {code}: {diagnostics:#?}"));
        assert_eq!(
            diagnostic.span.precision,
            flow::diagnostics::SpanPrecision::Token
        );
        assert_eq!(
            &source[diagnostic.span.start_byte..diagnostic.span.end_byte],
            token
        );
        if code == "check.program.unknown" {
            let repaired = apply(source, diagnostic.fixes.first().expect("program name fix"));
            assert!(repaired.contains("run program worker\n"));
            assert!(analyze_source(&repaired, &Registry::default()).is_empty());
        }
    }
}

#[test]
fn direct_executable_args_typo_has_a_machine_applicable_fix() {
    let source = "run \"grep\" with args [\"needle\"]\n";
    let diagnostic = only_diagnostic(source, &Registry::default(), "check.parameter.direct");
    assert!(diagnostic.message.contains("did you mean \"arguments\""));
    let fixed = apply(source, diagnostic.fixes.first().expect("rename fix"));
    assert_eq!(fixed, "run \"grep\" with arguments [\"needle\"]\n");
    assert!(analyze_source(&fixed, &Registry::default()).is_empty());
}

#[test]
fn reusable_program_input_errors_have_specific_codes_and_token_spans() {
    let registry = Registry::default();
    let definition = "define worker\n  accept input as text\n  use value input\n\n";
    for (call, code, token) in [
        (
            "run program worker with typo \"value\"\n",
            "check.program.input.unknown",
            "typo",
        ),
        (
            "run program worker\n",
            "check.program.input.required",
            "worker",
        ),
    ] {
        let source = format!("{definition}{call}");
        let diagnostics = analyze_source(&source, &registry);
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == code)
            .unwrap_or_else(|| panic!("expected {code}: {diagnostics:#?}"));
        assert_eq!(
            diagnostic.span.precision,
            flow::diagnostics::SpanPrecision::Token
        );
        assert_eq!(
            &source[diagnostic.span.start_byte..diagnostic.span.end_byte],
            token
        );
        if code == "check.program.input.unknown" {
            let repaired = apply(&source, diagnostic.fixes.first().expect("input rename fix"));
            assert!(repaired.contains("with input \"value\"\n"));
            assert!(
                analyze_source(&repaired, &registry)
                    .iter()
                    .all(|item| item.code != code)
            );
        }
        if code == "check.program.input.required" {
            let repaired = apply(
                &source,
                diagnostic.fixes.first().expect("input insertion fix"),
            );
            assert!(repaired.contains("run program worker with input \"\"\n"));
            assert!(
                analyze_source(&repaired, &registry)
                    .iter()
                    .all(|item| item.code != code)
            );
        }
    }
}

#[test]
fn one_hundred_invalid_sources_never_fall_back_to_check_failed() {
    let registry = Registry::default();
    for index in 0..100 {
        let source = format!("use value missing_{index}\n");
        let diagnostics = analyze_source(&source, &registry);
        assert!(
            !diagnostics.is_empty(),
            "source unexpectedly checked clean: {source}"
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code != "check_failed"),
            "generic failure code for {source}: {diagnostics:#?}"
        );
    }
}
