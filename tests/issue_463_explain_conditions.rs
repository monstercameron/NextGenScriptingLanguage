//! Issue #463, first open criterion: the `explain` plan names the condition
//! that guards an `if` row. The plan comes from the parsed program through
//! `flow::agent::explain::explain_json_with_structure`; nothing runs. The
//! published shape (`explain_json_with_inputs`) must stay unchanged.

use flow::agent::explain;
use flow::manifest::Registry;
use flow::parser;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const SAMPLE: &str = "accept items as list of text with default [\"a\"]
accept mode as text with default \"x\"
for each item in items
  if mode is \"x\"
    run \"rm\" with arguments [\"-f\", item]
  otherwise
    run \"touch\" with arguments [item]
";

fn rows_of_kind<'a>(plan: &'a Value, kind: &str) -> Vec<&'a Value> {
    plan["steps"]
        .as_array()
        .expect("steps array")
        .iter()
        .filter(|row| row["kind"] == kind)
        .collect()
}

#[test]
fn if_row_names_its_guard_with_reference_and_literal_operands() {
    let program = parser::parse(SAMPLE).expect("sample parses");
    let plan = explain::explain_json_with_structure(
        &program,
        &Registry::default(),
        "guard.program",
        &BTreeMap::new(),
    )
    .expect("structure plan");

    let ifs = rows_of_kind(&plan, "if");
    assert_eq!(ifs.len(), 1, "one if row for the sample program");
    assert_eq!(
        ifs[0]["condition"],
        json!({
            "left": {"ref": "mode"},
            "op": "equals",
            "negated": false,
            "right": {"literal": "x"}
        })
    );
}

#[test]
fn loop_and_otherwise_rows_keep_the_existing_fields() {
    let program = parser::parse(SAMPLE).expect("sample parses");
    let plan = explain::explain_json_with_structure(
        &program,
        &Registry::default(),
        "guard.program",
        &BTreeMap::new(),
    )
    .expect("structure plan");

    // Only `if` gains a condition in this slice. The loop source is a later criterion.
    assert_eq!(rows_of_kind(&plan, "loop_for_each").len(), 1);
    assert!(rows_of_kind(&plan, "loop_for_each")[0]
        .get("condition")
        .is_none());
    assert!(rows_of_kind(&plan, "otherwise")[0]
        .get("condition")
        .is_none());
}

#[test]
fn condition_shows_the_reference_name_not_the_supplied_input_value() {
    let program = parser::parse(SAMPLE).expect("sample parses");
    let mut inputs = BTreeMap::new();
    inputs.insert(
        "mode".to_string(),
        flow::ast::Value::String("supplied".to_string()),
    );
    let plan = explain::explain_json_with_structure(
        &program,
        &Registry::default(),
        "guard.program",
        &inputs,
    )
    .expect("structure plan");

    let ifs = rows_of_kind(&plan, "if");
    assert_eq!(ifs[0]["condition"]["left"], json!({"ref": "mode"}));
}

#[test]
fn negated_condition_is_marked_negated() {
    let source = "accept mode as text with default \"x\"
if mode is not \"x\"
  run \"touch\" with arguments [\"a\"]
";
    let program = parser::parse(source).expect("negated sample parses");
    let plan = explain::explain_json_with_structure(
        &program,
        &Registry::default(),
        "negated.program",
        &BTreeMap::new(),
    )
    .expect("structure plan");

    let ifs = rows_of_kind(&plan, "if");
    assert_eq!(ifs[0]["condition"]["negated"], json!(true));
    assert_eq!(ifs[0]["condition"]["op"], json!("equals"));
}

#[test]
fn published_shape_has_no_condition_key() {
    let program = parser::parse(SAMPLE).expect("sample parses");
    let plan = explain::explain_json_with_inputs(
        &program,
        &Registry::default(),
        "guard.program",
        &BTreeMap::new(),
    )
    .expect("published plan");

    for row in plan["steps"].as_array().expect("steps array") {
        assert!(
            row.get("condition").is_none(),
            "published rows must not gain a condition key: {row}"
        );
    }
    assert_eq!(rows_of_kind(&plan, "if").len(), 1);
}
