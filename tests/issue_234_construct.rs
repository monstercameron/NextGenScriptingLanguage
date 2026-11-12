//! Issue #234: record and list construction. The source is included directly so the
//! applet logic and its unit tests compile without the `flow` crate wiring.

#[path = "../src/applets/construct.rs"]
mod construct;

use serde_json::{Map, Value};

fn args(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .cloned()
        .expect("test arguments are an object")
}

#[test]
fn report_record_is_built_from_bindings_and_encodes_as_valid_json() {
    let failures = construct::list_make(&args(serde_json::json!({
        "items": ["timeout \"retry\"", "bad\nline"]
    })))
    .expect("list builds");
    let summary = construct::record_make(&args(serde_json::json!({
        "fields": {"total": 2, "name": "run \"7\"\n", "failures": failures}
    })))
    .expect("record builds");
    let summary = construct::record_set(&args(serde_json::json!({
        "record": summary, "key": "ok", "value": false
    })))
    .expect("field is set");
    let summary = construct::list_append(&args(serde_json::json!({
        "list": [summary], "item": {"note": "x\"y"}
    })))
    .expect("item is appended");
    let encoded = serde_json::to_string(&summary).expect("encodes");
    let decoded: Value = serde_json::from_str(&encoded).expect("valid JSON");
    assert_eq!(decoded, summary);
    assert_eq!(
        encoded,
        r#"[{"failures":["timeout \"retry\"","bad\nline"],"name":"run \"7\"\n","ok":false,"total":2},{"note":"x\"y"}]"#
    );
}

#[test]
fn unknown_argument_shapes_are_refused_by_name() {
    let error = construct::list_make(&args(serde_json::json!({"items": [], "flatten": true})))
        .expect_err("flatten is not implemented");
    assert_eq!(error.to_string(), "args.flatten is not implemented");
}
