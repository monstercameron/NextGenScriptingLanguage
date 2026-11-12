//! Issue #239: cross-format round trip. Each format applet that has a stringify
//! operation must encode a sample value into text that its own parse operation
//! reads back as an equal value. There is no tsv or jsonl applet and env has no
//! stringify operation, so those are covered only by parse checks or not at all.
//! Portable: no process is spawned and no file is read.

use flow::applets::{execute, AppletError};
use serde_json::{json, Map, Value};

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn call(operation: &str, arguments: Value, input: &[u8]) -> Result<Value, AppletError> {
    execute(operation, &args(arguments), input)
}

/// Encode `sample` with `<format>.stringify`, then parse the text back with
/// `<format>.parse`, returning the parsed value.
fn round_trip(format: &str, sample: &Value) -> Result<Value, String> {
    // csv.stringify takes its input as `table`; every other format takes `value`.
    let key = if format == "csv" { "table" } else { "value" };
    let text = call(&format!("{format}.stringify"), json!({ key: sample }), &[])
        .map_err(|error| format!("{format}.stringify failed: {error:?}"))?;
    let text = text
        .as_str()
        .ok_or_else(|| format!("{format}.stringify did not return text"))?;
    call(&format!("{format}.parse"), json!({"text": text}), &[])
        .map_err(|error| format!("{format}.parse failed on its own output: {error:?}"))
}

/// Return the path of the first place where `actual` differs from `expected`.
fn first_difference(path: &str, expected: &Value, actual: &Value) -> Option<String> {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in left {
                let child = format!("{path}.{key}");
                let found = match right.get(key) {
                    Some(other) => first_difference(&child, value, other),
                    None => Some(format!("{child} is missing")),
                };
                if found.is_some() {
                    return found;
                }
            }
            right
                .keys()
                .find(|key| !left.contains_key(*key))
                .map(|key| format!("{path}.{key} is unexpected"))
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => left
            .iter()
            .zip(right)
            .enumerate()
            .find_map(|(index, (l, r))| first_difference(&format!("{path}[{index}]"), l, r)),
        _ if expected == actual => None,
        _ => Some(format!("{path}: expected {expected}, got {actual}")),
    }
}

fn assert_round_trip(format: &str, sample: &Value) {
    let parsed = round_trip(format, sample).unwrap_or_else(|error| panic!("{format}: {error}"));
    if let Some(difference) = first_difference("$", sample, &parsed) {
        panic!("{format} round trip changed the value; first difference at {difference}");
    }
}

fn typed_sample() -> Value {
    json!({
        "name": "execute",
        "count": 3,
        "enabled": true,
        "ratio": 0.5,
        "tags": ["a", "b"],
        "nested": {"port": 8080, "host": "localhost"}
    })
}

#[test]
fn json_round_trips_a_typed_sample() {
    assert_round_trip("json", &typed_sample());
}

#[test]
fn yaml_round_trips_a_typed_sample() {
    assert_round_trip("yaml", &typed_sample());
}

#[test]
fn toml_round_trips_a_typed_sample() {
    assert_round_trip("toml", &typed_sample());
}

#[test]
fn ini_round_trips_a_string_sample() {
    assert_round_trip(
        "ini",
        &json!({"name": "execute", "section": {"key": "value", "other": "1"}}),
    );
}

#[test]
fn csv_round_trips_a_table() {
    assert_round_trip(
        "csv",
        &json!({
            "headers": ["name", "count"],
            "rows": [["execute", "3"], ["flow", "4"]]
        }),
    );
}

#[test]
fn env_parses_a_plain_sample_to_the_expected_value() {
    let parsed = call("env.load", json!({}), b"NAME=execute\nCOUNT=3\n")
        .expect("env.load should parse a plain environment file");
    assert_eq!(parsed, json!({"NAME": "execute", "COUNT": "3"}));
}

#[test]
fn first_difference_names_the_first_mismatched_key() {
    let expected = json!({"a": 1, "b": {"c": "x"}});
    let actual = json!({"a": 1, "b": {"c": "y"}});
    assert_eq!(
        first_difference("$", &expected, &actual).as_deref(),
        Some("$.b.c: expected \"x\", got \"y\"")
    );
    assert_eq!(
        first_difference("$", &expected, &json!({"a": 1})).as_deref(),
        Some("$.b is missing")
    );
}
