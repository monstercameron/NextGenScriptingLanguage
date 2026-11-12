//! Issue #403: the comparison report is pure, and its JSON names the differing field.

use flow::compare::{compare, Outcome};
use serde_json::Value;
use std::collections::BTreeMap;

fn captured() -> Outcome {
    let mut files = BTreeMap::new();
    files.insert("out/result.txt".to_string(), b"done\n".to_vec());
    Outcome {
        status: Some(0),
        stdout: b"hello\n".to_vec(),
        stderr: Vec::new(),
        fake_calls: vec!["git status".to_string()],
        files,
    }
}

#[test]
fn identical_outcomes_produce_a_valid_report_with_no_differences() {
    let text = compare(&captured(), &captured()).to_json().to_string();
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["identical"], Value::Bool(true));
    assert_eq!(value["differences"], Value::Array(Vec::new()));
}

#[test]
fn one_changed_stdout_byte_produces_one_stdout_difference_in_valid_json() {
    let original = captured();
    let mut port = captured();
    port.stdout[0] = b'H';

    let text = compare(&original, &port).to_json().to_string();
    let value: Value = serde_json::from_str(&text).unwrap();

    assert_eq!(value["identical"], Value::Bool(false));
    let differences = value["differences"].as_array().unwrap();
    assert_eq!(differences.len(), 1);
    assert_eq!(differences[0]["field"], Value::from("stdout"));
    assert_eq!(differences[0]["path"], Value::Null);
}

#[test]
fn non_utf8_output_still_yields_valid_json() {
    let original = captured();
    let mut port = captured();
    port.stderr = vec![0xff, 0xfe, 0x00];

    let text = compare(&original, &port).to_json().to_string();
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["differences"][0]["field"], Value::from("stderr"));
}
