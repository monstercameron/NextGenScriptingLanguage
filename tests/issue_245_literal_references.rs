//! docs/grammar.md ("Literal lists and objects") states that object and list literals
//! accept bound names. These tests hold the documented forms to the real parser.
use flow::ast::{StepKind, Value};
use std::collections::BTreeMap;

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
