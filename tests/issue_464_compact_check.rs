//! `render_check_compact` must lose no information: expanding its output back (re-adding
//! the omitted path bytes and edit spans) gives the same value as the full check result.

use flow::diagnostics::{analyze_source_at, render_check_compact};
use flow::manifest::Registry;
use serde_json::{json, Value};
use std::path::Path;

fn fixture_registry() -> Registry {
    Registry::from_sources(&[json!({
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

/// The full check result, built the way `check --json` builds it. The program file must
/// exist at `path`, because analysis resolves the file it is checked from.
fn full_check_result(path: &Path, source: &str, registry: &Registry) -> Value {
    std::fs::write(path, source).unwrap();
    let diagnostics: Vec<Value> = analyze_source_at(source, Some(path), registry)
        .iter()
        .map(|diagnostic| serde_json::to_value(diagnostic).unwrap())
        .collect();
    let ok = diagnostics
        .iter()
        .all(|diagnostic| diagnostic["severity"] != "error");
    json!({
        "protocol": "execute.check/1",
        "validation": "static",
        "ok": ok,
        "path": path.to_string_lossy(),
        "path_bytes": path.as_os_str().as_encoded_bytes(),
        "diagnostics": diagnostics,
    })
}

/// Each diagnostic as `code/fix-count`, so a failing assertion shows what analysis returned.
fn codes_and_fix_counts(full: &Value) -> Vec<String> {
    full["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| {
            let fixes = diagnostic["fixes"].as_array().map_or(0, Vec::len);
            format!("{}/{fixes}", diagnostic["code"].as_str().unwrap_or("?"))
        })
        .collect()
}

/// Re-adds what the compact form omits: the byte array of a UTF-8 path, and the span of an
/// edit that equals its diagnostic's span.
fn expand(compact: &Value) -> Value {
    let mut full = compact.clone();
    restore_path_bytes(&mut full);
    let Some(diagnostics) = full.get_mut("diagnostics").and_then(Value::as_array_mut) else {
        return full;
    };
    for diagnostic in diagnostics {
        if let Some(span) = diagnostic.get_mut("span") {
            restore_path_bytes(span);
        }
        let diagnostic_span = diagnostic.get("span").cloned();
        let Some(fixes) = diagnostic.get_mut("fixes").and_then(Value::as_array_mut) else {
            continue;
        };
        for fix in fixes {
            let Some(edits) = fix.get_mut("edits").and_then(Value::as_array_mut) else {
                continue;
            };
            for edit in edits {
                match edit.get_mut("span") {
                    Some(span) => restore_path_bytes(span),
                    None => edit["span"] = diagnostic_span.clone().unwrap_or(Value::Null),
                }
            }
        }
    }
    full
}

fn restore_path_bytes(object: &mut Value) {
    if object.get("path_bytes").is_some() {
        return;
    }
    if let Some(path) = object
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        object["path_bytes"] = json!(path.as_bytes());
    }
}

/// Checks the round trip for one check result and reports both sizes on failure only.
fn assert_round_trip(label: &str, full: &Value) -> Value {
    let compact = render_check_compact(full);
    let full_text = serde_json::to_string_pretty(full).unwrap();
    assert!(
        !compact.contains('\n'),
        "{label}: compact form is not one line (compact {} bytes, full {} bytes)",
        compact.len(),
        full_text.len()
    );
    let parsed: Value = serde_json::from_str(&compact).unwrap();
    assert_eq!(
        expand(&parsed),
        *full,
        "{label}: expanded compact form differs (compact {} bytes, full {} bytes)",
        compact.len(),
        full_text.len()
    );
    parsed
}

#[test]
fn a_clean_program_keeps_its_required_keys_and_drops_the_utf8_path_bytes() {
    let registry = fixture_registry();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("clean.program");
    let full = full_check_result(
        &path,
        "run fixture.reader with arguments \"x\"\n",
        &registry,
    );
    let parsed = assert_round_trip("clean", &full);
    assert_eq!(parsed["ok"], json!(true), "FULL {full}");
    assert_eq!(parsed["diagnostics"], json!([]), "FULL {full}");
    assert_eq!(parsed["protocol"], json!("execute.check/1"));
    assert!(parsed.get("path_bytes").is_none(), "{parsed}");
}

#[test]
fn one_error_with_a_fix_expands_back_to_the_full_form() {
    let registry = fixture_registry();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("one-error.program");
    let full = full_check_result(&path, "run fixture.reader with argumens \"x\"\n", &registry);
    assert_eq!(
        codes_and_fix_counts(&full),
        ["applet.parameter.required/0", "applet.parameter.unknown/1"]
    );
    let parsed = assert_round_trip("one error", &full);
    assert_eq!(parsed["ok"], json!(false));
    let fixed = parsed["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| diagnostic["code"] == "applet.parameter.unknown")
        .unwrap();
    // The edit covers the diagnostic's own span, so the compact edit writes no span.
    assert_eq!(fixed["fixes"][0]["edits"][0].get("span"), None);
}

#[test]
fn several_diagnostics_expand_back_to_the_full_form() {
    let registry = fixture_registry();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("several.program");
    let source = "run fixture.reader with argumens \"x\"\n\
                  run fixture.reader with argumens \"y\"\n";
    let full = full_check_result(&path, source, &registry);
    assert_eq!(
        codes_and_fix_counts(&full),
        [
            "applet.parameter.required/0",
            "applet.parameter.unknown/1",
            "applet.parameter.required/0",
            "applet.parameter.unknown/1"
        ]
    );
    assert_round_trip("several", &full);
}

#[cfg(unix)]
#[test]
fn a_path_that_is_not_utf8_keeps_its_byte_array_and_round_trips() {
    use std::os::unix::ffi::OsStrExt;

    let registry = fixture_registry();
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join(std::ffi::OsStr::from_bytes(b"bad-\xff.program"));
    let full = full_check_result(&path, "run fixture.reader with argumens \"x\"\n", &registry);
    let parsed = assert_round_trip("non-utf8 path", &full);
    assert_eq!(parsed["path_bytes"], full["path_bytes"]);
}

#[test]
fn omission_keeps_required_keys_and_drops_empty_optional_ones() {
    let full = json!({
        "protocol": "execute.check/1",
        "validation": "static",
        "ok": false,
        "path": "a.program",
        "path_bytes": [97, 46, 112, 114, 111, 103, 114, 97, 109],
        "diagnostics": [{
            "severity": "error",
            "code": "x.y",
            "message": "m",
            "span": {
                "start_byte": 0, "end_byte": 1, "line": 1, "column": 1,
                "end_line": 1, "end_column": 2, "precision": "token"
            },
            "causes": [],
            "related": []
        }]
    });
    let compact: Value = serde_json::from_str(&render_check_compact(&full)).unwrap();
    assert_eq!(compact["ok"], json!(false));
    assert_eq!(compact["diagnostics"].as_array().unwrap().len(), 1);
    assert!(compact.get("path_bytes").is_none(), "{compact}");
    let diagnostic = &compact["diagnostics"][0];
    assert!(diagnostic.get("causes").is_none(), "{diagnostic}");
    assert!(diagnostic.get("related").is_none(), "{diagnostic}");
    assert_eq!(diagnostic["code"], json!("x.y"));
    assert_eq!(diagnostic["span"]["precision"], json!("token"));
}
