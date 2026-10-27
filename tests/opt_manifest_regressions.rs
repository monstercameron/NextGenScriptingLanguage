use flow::ast::{Invocation, Value};
use flow::manifest::Registry;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn opt_manifest_from_json(json: &str) -> flow::manifest::Manifest {
    let dir = tempdir().unwrap();
    let path = dir.path().join("manifest.json");
    fs::write(&path, json).unwrap();
    Registry::load(Path::new(&path))
        .unwrap()
        .resolve("check")
        .unwrap()
        .clone()
}

fn opt_manifest_load_error(json: &str) -> String {
    let dir = tempdir().unwrap();
    let path = dir.path().join("manifest.json");
    fs::write(&path, json).unwrap();
    match Registry::load(Path::new(&path)) {
        Ok(_) => panic!("invalid manifest unexpectedly loaded"),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn opt_manifest_list_type_errors_keep_precedence_over_nul_errors() {
    let manifest = opt_manifest_from_json(
        r#"{"name":"check","binary":"tool","parameters":{"items":{"type":"list","items":"string","required":true}},"arguments":[{"spread":"items"}]}"#,
    );
    let invalid = Value::List(vec![Value::String("\0".into()), Value::Integer(1)]);
    let invocation = Invocation {
        operation: "check".into(),
        direct: false,
        discard_stderr: false,
        parameters: BTreeMap::from([("items".into(), invalid.clone())]),
        binding: None,
        cache: false,
        status_binding: None,
        result_binding: None,
        retry: None,
    };

    assert_eq!(
        manifest.validate(&invocation).unwrap_err().to_string(),
        "parameter \"items\" contains an item that is not string"
    );
    assert_eq!(
        manifest
            .build_arguments(&BTreeMap::from([("items".into(), invalid)]))
            .unwrap_err()
            .to_string(),
        "parameter \"items\" contains an item that is not string"
    );
    assert_eq!(
        manifest
            .build_arguments(&BTreeMap::from([(
                "items".into(),
                Value::List(vec![Value::String("\0".into())]),
            )]))
            .unwrap_err()
            .to_string(),
        "parameter \"items\" contains a NUL byte, which cannot be passed in argv"
    );
}

#[test]
fn opt_manifest_json_lists_require_json_arguments_not_spread() {
    let manifest = opt_manifest_from_json(
        r#"{"name":"check","binary":"tool","parameters":{"items":{"type":"list","items":"json","required":true},"flags":{"type":"list","items":"string","required":true}},"arguments":[{"literal":"--flags"},{"spread":"flags"},{"literal":"--args"},{"json_arguments":true}]}"#,
    );
    let values = BTreeMap::from([
        (
            "items".into(),
            Value::List(vec![Value::Record(BTreeMap::from([(
                "name".into(),
                Value::String("Ada".into()),
            )]))]),
        ),
        (
            "flags".into(),
            Value::List(vec![Value::String("--quiet".into())]),
        ),
    ]);
    assert_eq!(
        manifest.build_arguments(&values).unwrap(),
        [
            "--flags",
            "--quiet",
            "--args",
            r#"{"flags":["--quiet"],"items":[{"name":"Ada"}]}"#
        ]
    );

    let spread_only = opt_manifest_load_error(
        r#"{"name":"check","binary":"tool","parameters":{"items":{"type":"list","items":"json","required":true}},"arguments":[{"spread":"items"}]}"#,
    );
    assert!(spread_only
        .contains("json and record list items require json_arguments rather than spread"));
}

#[test]
fn opt_manifest_unknown_and_required_errors_keep_their_order() {
    let manifest = opt_manifest_from_json(
        r#"{"name":"check","binary":"tool","parameters":{"a":{"type":"string","required":true},"z":{"type":"string"}},"arguments":[]}"#,
    );
    let with_unknown = BTreeMap::from([("unknown".into(), Value::Integer(1))]);
    let invocation = Invocation {
        operation: "check".into(),
        direct: false,
        discard_stderr: false,
        parameters: with_unknown.clone(),
        binding: None,
        cache: false,
        status_binding: None,
        result_binding: None,
        retry: None,
    };
    let expected_unknown = "applet \"check\" has no parameter \"unknown\"";
    assert_eq!(
        manifest.validate(&invocation).unwrap_err().to_string(),
        expected_unknown
    );
    assert_eq!(
        manifest
            .build_arguments(&with_unknown)
            .unwrap_err()
            .to_string(),
        expected_unknown
    );

    let wrong_later_value = BTreeMap::from([("z".into(), Value::Integer(1))]);
    let invocation = Invocation {
        operation: "check".into(),
        direct: false,
        discard_stderr: false,
        parameters: wrong_later_value.clone(),
        binding: None,
        cache: false,
        status_binding: None,
        result_binding: None,
        retry: None,
    };
    let expected_required = "applet \"check\" requires parameter \"a\"";
    assert_eq!(
        manifest.validate(&invocation).unwrap_err().to_string(),
        expected_required
    );
    assert_eq!(
        manifest
            .build_arguments(&wrong_later_value)
            .unwrap_err()
            .to_string(),
        expected_required
    );
}

#[test]
fn opt_manifest_parameter_deserialization_stays_strict_and_keeps_serialize_shape() {
    let manifest = opt_manifest_from_json(
        r#"{"name":"check","binary":"tool","parameters":{"items":{"type":"list","items":"integer","required":true,"min_items":2},"text":{"type":"string"}},"arguments":[]}"#,
    );
    assert_eq!(
        serde_json::to_string(&manifest).unwrap(),
        r#"{"name":"check","binary":"tool","builtin":null,"accepts_stdin":false,"cacheable":false,"phrases":[],"parameters":{"items":{"kind":"list","item_type":"integer","required":true,"min_items":2},"text":{"kind":"string","item_type":null,"required":false,"min_items":null}},"arguments":[]}"#
    );

    let unknown_field = opt_manifest_load_error(
        r#"{"name":"check","binary":"tool","parameters":{"text":{"type":"string","surprise":true}},"arguments":[]}"#,
    );
    assert!(unknown_field.contains("unknown field `surprise`"));
    assert!(unknown_field.contains("expected one of `type`, `required`, `items`, `min_items`"));

    let invalid_type = opt_manifest_load_error(
        r#"{"name":"check","binary":"tool","parameters":{"text":{"type":"textual"}},"arguments":[]}"#,
    );
    assert!(invalid_type.contains("unknown variant `textual`"));

    let missing_items = opt_manifest_load_error(
        r#"{"name":"check","binary":"tool","parameters":{"items":{"type":"list"}},"arguments":[]}"#,
    );
    assert!(missing_items.contains("list requires an items type"));
}

#[test]
fn opt_manifest_large_spread_with_optional_nested_spread_preserves_argv_order() {
    let manifest = opt_manifest_from_json(
        r#"{"name":"check","binary":"tool","parameters":{"label":{"type":"string","required":true},"items":{"type":"list","items":"string","required":true},"extra":{"type":"list","items":"string"}},"arguments":[{"literal":"begin"},{"parameter":"label"},{"spread":"items"},{"when_present":"extra","append":[{"literal":"extra"},{"spread":"extra"}]}]}"#,
    );
    let mut values = BTreeMap::from([
        ("label".into(), Value::String("batch".into())),
        (
            "items".into(),
            Value::List(
                (0..256)
                    .map(|index| Value::String(format!("item-{index}")))
                    .collect(),
            ),
        ),
        (
            "extra".into(),
            Value::List(vec![
                Value::String("tail-a".into()),
                Value::String("tail-b".into()),
            ]),
        ),
    ]);
    let mut expected = vec!["begin".to_owned(), "batch".to_owned()];
    expected.extend((0..256).map(|index| format!("item-{index}")));
    expected.extend(["extra".to_owned(), "tail-a".to_owned(), "tail-b".to_owned()]);
    assert_eq!(manifest.build_arguments(&values).unwrap(), expected);

    values.remove("extra");
    expected.truncate(258);
    assert_eq!(manifest.build_arguments(&values).unwrap(), expected);
}
