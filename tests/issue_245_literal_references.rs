//! docs/grammar.md ("Literal lists and objects") states that object and list literals
//! accept bound names. These tests hold the documented forms to the real parser.
use flow::ast::{StepKind, Value};
use flow::{manifest::Registry, parser, runtime};
use std::collections::BTreeMap;
use std::time::Duration;

#[test]
fn object_literal_with_bound_names_parses_names_as_references() {
    let program = flow::parser::parse(
        "run json.stringify with value {\"path\": path, \"count\": count} as report\n",
    )
    .unwrap();
    let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
        panic!("expected an invocation step");
    };
    let expected = Value::Record(BTreeMap::from([
        ("count".to_owned(), Value::Reference("count".into())),
        ("path".to_owned(), Value::Reference("path".into())),
    ]));
    assert_eq!(call.parameters.get("value"), Some(&expected));
}

#[test]
fn list_literal_with_bound_names_parses_names_as_references() {
    let program = flow::parser::parse("run printf with values [\"two words\", name]\n").unwrap();
    let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
        panic!("expected an invocation step");
    };
    let expected = Value::List(vec![
        Value::String("two words".into()),
        Value::Reference("name".into()),
    ]);
    assert_eq!(call.parameters.get("values"), Some(&expected));
}

fn run_source_output(source: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let options = runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        ..runtime::RunOptions::new(dir.path().into())
    };
    let program = parser::parse(source).unwrap();
    runtime::run(&program, &Registry::default(), &options)
        .unwrap()
        .output
}

#[test]
fn object_literal_with_defaulted_bound_names_encodes_its_values() {
    let source = "accept path as text with default \"notes.txt\"\n\
                  accept count as integer with default 3\n\
                  use value {\"path\": path, \"count\": count} as report\n\
                  then encode report as json\n";
    let output = run_source_output(source);
    let json: serde_json::Value = serde_json::from_slice(&output)
        .unwrap_or_else(|error| panic!("output is not JSON ({error}): {output:?}"));
    assert_eq!(
        json,
        serde_json::json!({"path": "notes.txt", "count": 3}),
        "output: {}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn list_literal_with_defaulted_bound_names_encodes_its_values() {
    let source = "accept path as text with default \"notes.txt\"\n\
                  accept count as integer with default 3\n\
                  use value [path, count] as items\n\
                  then encode items as json\n";
    let output = run_source_output(source);
    let json: serde_json::Value = serde_json::from_slice(&output)
        .unwrap_or_else(|error| panic!("output is not JSON ({error}): {output:?}"));
    assert_eq!(
        json,
        serde_json::json!(["notes.txt", 3]),
        "output: {}",
        String::from_utf8_lossy(&output)
    );
}
