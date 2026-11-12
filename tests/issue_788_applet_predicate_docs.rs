use flow::manifest::Registry;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// Every `test` name that `ParameterPredicate` accepts in src/manifest.rs.
const PREDICATE_TESTS: [&str; 11] = [
    "present",
    "absent",
    "nonempty",
    "positive_integer",
    "nonnegative_integer",
    "nonzero_integer",
    "negative_integer",
    "boolean",
    "string_not_equals",
    "number_range",
    "number_range_by_representation",
];

fn applet_contracts_doc() -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/applet-contracts.md"))
        .unwrap()
}

fn manifest_with(predicate: Value) -> String {
    json!({
        "name": "fixture",
        "binary": "fixture",
        "parameters": {
            "value": {"type": "integer"},
            "text": {"type": "string"},
            "flag": {"type": "boolean"},
            "source": {"type": "json"}
        },
        "parameter_alternatives": [{"message": "choose", "any_of": [[predicate]]}],
        "arguments": []
    })
    .to_string()
}

fn predicate_for(test: &str) -> Value {
    match test {
        "present" => json!({"test": "present", "parameter": "value"}),
        "absent" => json!({"test": "absent", "parameter": "value"}),
        "nonempty" => json!({"test": "nonempty", "parameter": "text"}),
        "positive_integer" | "nonnegative_integer" | "nonzero_integer" | "negative_integer" => {
            json!({"test": test, "parameter": "value"})
        }
        "boolean" => json!({"test": "boolean", "parameter": "flag", "value": true}),
        "string_not_equals" => {
            json!({"test": "string_not_equals", "parameter": "text", "value": "stdout"})
        }
        "number_range" => {
            json!({"test": "number_range", "parameter": "value", "minimum": 0, "maximum": 10})
        }
        "number_range_by_representation" => json!({
            "test": "number_range_by_representation",
            "parameter": "value",
            "based_on": "source",
            "representation": "integer",
            "minimum": 0,
            "maximum": 10
        }),
        other => panic!("no fixture for predicate {other}"),
    }
}

#[test]
fn every_accepted_predicate_is_documented() {
    let doc = applet_contracts_doc();
    for test in PREDICATE_TESTS {
        assert!(
            doc.contains(&format!("`{test}`")),
            "predicate `{test}` is not documented in docs/applet-contracts.md"
        );
    }
}

#[test]
fn every_documented_predicate_is_accepted_by_manifest_parsing() {
    for test in PREDICATE_TESTS {
        let source = manifest_with(predicate_for(test));
        assert!(
            Registry::from_sources(&[source]).is_ok(),
            "manifest parsing refused documented predicate `{test}`"
        );
    }
}

#[test]
fn representation_predicate_projection_is_marked_as_an_extension() {
    let source = manifest_with(predicate_for("number_range_by_representation"));
    let registry = Registry::from_sources(&[source]).unwrap();
    let projection = registry.resolve("fixture").unwrap().contract().to_string();
    assert!(projection.contains("x-number-range-by-representation"));
    assert!(applet_contracts_doc().contains("x-number-range-by-representation"));
}
