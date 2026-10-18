//! Preserve validation order and typed interfaces while indexing large schemas.
use flow::{
    ast::{InputDeclaration, InputType, Value},
    inputs,
};
use std::collections::BTreeMap;

fn declarations(count: usize) -> Vec<InputDeclaration> {
    (0..count)
        .map(|n| InputDeclaration {
            name: format!("p_{n:04}"),
            kind: InputType::Integer,
            default: Some(Value::Integer(42)),
            source: None,
            line: n + 1,
            description: None,
            choices: None,
        })
        .collect()
}

#[test]
fn schema_errors_precede_supplied_errors_for_single_and_indexed_interfaces() {
    for count in [1, 8, 64] {
        let mut schema = declarations(count);
        schema[0].name = "bad-name".into();
        let text = BTreeMap::from([("unknown".into(), "bad".into())]);
        let typed = BTreeMap::from([("unknown".into(), Value::Bool(false))]);
        let expected = "line 1: invalid input name \"bad-name\"";
        for error in [
            inputs::validate(&schema).unwrap_err(),
            inputs::parse_cli(&schema, &text).unwrap_err(),
            inputs::resolve(&schema, &typed, false).unwrap_err(),
            inputs::resolve(&schema, &typed, true).unwrap_err(),
            inputs::describe(&schema, &typed).unwrap_err(),
        ] {
            assert_eq!(error.to_string(), expected);
        }
    }
    let mut schema = declarations(64);
    schema[63].name = "p_0000".into();
    schema[63].default = Some(Value::Bool(false));
    assert_eq!(
        inputs::parse_cli(&schema, &BTreeMap::new())
            .unwrap_err()
            .to_string(),
        "line 64: duplicate input \"p_0000\""
    );
    schema[0].default = Some(Value::Bool(false));
    assert_eq!(
        inputs::validate(&schema).unwrap_err().to_string(),
        "line 1: input \"p_0000\" expects integer, got boolean"
    );
}

#[test]
fn supplied_error_order_and_allow_extra_policy_remain_distinct() {
    for count in [1, 8, 64, 1000] {
        let schema = declarations(count);
        let typed = BTreeMap::from([
            ("p_0000".into(), Value::String("wrong type".into())),
            ("zzz_unknown".into(), Value::Bool(false)),
        ]);
        assert_eq!(
            inputs::resolve(&schema, &typed, false)
                .unwrap_err()
                .to_string(),
            "unknown program input \"zzz_unknown\""
        );
        for error in [
            inputs::resolve(&schema, &typed, true).unwrap_err(),
            inputs::describe(&schema, &typed).unwrap_err(),
        ] {
            assert_eq!(
                error.to_string(),
                "line 1: input \"p_0000\" expects integer, got text"
            );
        }
        let text = BTreeMap::from([
            ("p_0000".into(), "bad integer".into()),
            ("zzz_unknown".into(), "ignored".into()),
        ]);
        assert_eq!(
            inputs::parse_cli(&schema, &text).unwrap_err().to_string(),
            "input \"p_0000\" requires an integer"
        );
    }
}

#[test]
fn indexed_interfaces_keep_literal_values_types_defaults_and_local_binding_policy() {
    let mut schema = declarations(64);
    let mut typed = BTreeMap::new();
    let mut text = BTreeMap::new();
    let mut expected = BTreeMap::new();
    for (n, decl) in schema.iter_mut().enumerate() {
        let (kind, value) = match n % 5 {
            0 => (
                InputType::Text,
                Value::String("spaces 雪; $(literal) *".into()),
            ),
            1 => (InputType::Integer, Value::Integer(-7)),
            2 => (InputType::Boolean, Value::Bool(false)),
            3 => (
                InputType::TextList,
                Value::List(vec![
                    Value::String("one space".into()),
                    Value::String("*".into()),
                ]),
            ),
            _ => (
                InputType::Json,
                Value::Record(BTreeMap::from([("answer".into(), Value::Integer(42))])),
            ),
        };
        let literal = match n % 5 {
            0 => serde_json::json!("spaces 雪; $(literal) *"),
            1 => serde_json::json!(-7),
            2 => serde_json::json!(false),
            3 => serde_json::json!(["one space", "*"]),
            _ => serde_json::json!({"answer":42}),
        };
        expected.insert(decl.name.clone(), literal.clone());
        decl.kind = kind;
        decl.default = Some(value.clone());
        text.insert(
            decl.name.clone(),
            if let Value::String(v) = &value {
                v.clone()
            } else {
                literal.to_string()
            },
        );
        typed.insert(decl.name.clone(), value);
    }
    assert_eq!(inputs::parse_cli(&schema, &text).unwrap(), typed);
    assert_eq!(
        inputs::resolve(&schema, &BTreeMap::new(), false).unwrap(),
        typed
    );
    assert_eq!(inputs::resolve(&schema, &typed, false).unwrap(), typed);
    let mut locals = typed.clone();
    locals.insert("external_local".into(), Value::Bool(true));
    assert_eq!(inputs::resolve(&schema, &locals, true).unwrap(), typed);
    assert!(inputs::resolve(&schema, &locals, false)
        .unwrap_err()
        .to_string()
        .contains("external_local"));
    let description = inputs::describe(&schema, &typed).unwrap();
    assert_eq!(description["inputs_complete"], true);
    for spec in description["inputs"].as_array().unwrap() {
        assert_eq!(spec["value"], expected[spec["name"].as_str().unwrap()]);
    }
}
