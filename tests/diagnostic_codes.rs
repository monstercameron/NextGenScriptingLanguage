//! Stable diagnostic codes: every diagnostic `execute check --json` emits carries a
//! published code, and every published code is documented.

use flow::diagnostics::{analyze_source, code_info, CODES};
use flow::manifest::Registry;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

fn fixture_registry() -> Registry {
    Registry::from_sources(&[serde_json::json!({
        "name": "fixture.count",
        "binary": "fixture-count",
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {
            "count": { "type": "integer", "required": true }
        },
        "arguments": [{ "parameter": "count" }]
    })
    .to_string()])
    .unwrap()
}

/// Sources that each trigger a static diagnostic, with the code they must produce.
fn corpus() -> Vec<(&'static str, &'static str)> {
    vec![
        ("read file\n", "parse.syntax"),
        ("run \"unterminated\n", "parse.lexical"),
        ("\trun \"true\"\n", "parse.indentation"),
        ("run nosuchapplet with x \"y\"\n", "applet.unknown"),
        (
            "run fixture.count with bogus 1\n",
            "applet.parameter.unknown",
        ),
        (
            "run fixture.count with count \"text\"\n",
            "applet.parameter.type",
        ),
        ("run fixture.count\n", "applet.parameter.required"),
    ]
}

#[test]
fn every_static_diagnostic_has_a_published_code() {
    let registry = fixture_registry();
    let mut seen = BTreeSet::new();
    for (source, expected) in corpus() {
        let diagnostics = analyze_source(source, &registry);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == expected),
            "{source:?} should produce {expected}: {diagnostics:#?}"
        );
        for diagnostic in &diagnostics {
            assert!(
                !diagnostic.code.is_empty(),
                "{source:?} emitted an empty code"
            );
            assert!(
                code_info(&diagnostic.code).is_some(),
                "{source:?} emitted the unpublished code {:?}",
                diagnostic.code
            );
            seen.insert(diagnostic.code.clone());
        }
    }
    assert!(
        seen.len() >= 6,
        "the corpus should exercise the registry: {seen:?}"
    );
}

#[test]
fn published_codes_are_unique_and_grouped_by_known_families() {
    let mut codes = BTreeSet::new();
    for info in CODES {
        assert!(codes.insert(info.code), "duplicate code {}", info.code);
        assert!(
            ["parse", "resolve", "type", "policy", "validate"].contains(&info.family),
            "{} has an unknown family {}",
            info.code,
            info.family
        );
        assert!(!info.meaning.is_empty(), "{} has no meaning", info.code);
    }
}

#[test]
fn every_published_code_is_documented() {
    let docs = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/language-server.md"),
    )
    .unwrap();
    for info in CODES {
        assert!(
            docs.contains(&format!("| `{}` |", info.code)),
            "docs/language-server.md has no row for {}",
            info.code
        );
    }
}

#[test]
fn check_json_never_emits_a_diagnostic_without_a_published_code() {
    let work = tempfile::tempdir().unwrap();
    let program = work.path().join("bad.program");
    std::fs::write(&program, "read file\nrun \"unterminated\n").unwrap();
    let policy = work.path().join("policy.json");
    std::fs::write(&policy, r#"{"version":1,"applets":{}}"#).unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");

    // One run with a static error, and one with a capability policy and no data-flow
    // policy, which adds the policy warning.
    for extra in [
        vec![],
        vec!["--capability-policy", policy.to_str().unwrap()],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("check")
            .arg(&program)
            .arg("--manifest-dir")
            .arg(&manifests)
            .arg("--json")
            .args(&extra)
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let diagnostics = report["diagnostics"].as_array().unwrap();
        assert!(!diagnostics.is_empty(), "{report}");
        for diagnostic in diagnostics {
            let code = diagnostic["code"].as_str().unwrap_or_default();
            assert!(
                code_info(code).is_some(),
                "a diagnostic has no published code: {diagnostic}"
            );
        }
    }
}
