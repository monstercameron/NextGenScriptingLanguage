use flow::applets;
use serde_json::{json, Map, Value};

fn run(operation: &str, args: Value, input: &[u8]) -> Result<Value, applets::AppletError> {
    applets::execute(
        operation,
        &args.as_object().cloned().unwrap_or_default(),
        input,
    )
}

#[test]
fn leading_bom_is_stripped_by_text_parsers() {
    for (operation, text, expected) in [
        ("yaml.parse", "\u{feff}a: 1\n", json!({"a": 1})),
        ("ini.parse", "\u{feff}a=1\n", json!({"a": "1"})),
        ("toml.parse", "\u{feff}a=1\n", json!({"a": 1})),
    ] {
        assert_eq!(
            run(operation, json!({}), text.as_bytes()).unwrap(),
            expected
        );
    }
    assert_eq!(
        run("env.load", json!({}), b"\xef\xbb\xbfA=1\n").unwrap(),
        json!({"A": "1"})
    );
    assert_eq!(
        run("json.parse", json!({}), b"\xef\xbb\xbf{\"a\":1}").unwrap(),
        json!({"a": 1})
    );
    assert_eq!(
        run(
            "jsonc.edit_content",
            json!({"content":"\u{feff}{\"a\":1}","key":"a","action":"set","value":1}),
            b""
        )
        .unwrap(),
        json!("{\"a\":1}")
    );
}

#[test]
fn csv_accepts_tabs_ignores_final_blank_record_and_quotes_empty_cells() {
    let parsed = run("csv.parse", json!({}), b"a,b\n1,x\ty\n\n").unwrap();
    assert_eq!(parsed, json!({"headers":["a","b"],"rows":[["1","x\ty"]]}));
    let output = run(
        "csv.stringify",
        json!({"table":{"headers":["a"],"rows":[[""]]} }),
        b"",
    )
    .unwrap();
    assert_eq!(output, json!("a\r\n\"\"\r\n"));
}

#[test]
fn yaml_writer_quotes_common_implicit_values_and_parser_fails_closed() {
    let values = json!(["yes", "on", "NO", "0x1F", "12:30", "2020-01-01", "=", "<<"]);
    let output = run("yaml.stringify", json!({"value":values}), b"").unwrap();
    let text = output.as_str().unwrap();
    for value in ["yes", "on", "NO", "0x1F", "12:30", "2020-01-01", "=", "<<"] {
        assert!(text.contains(&format!("\"{value}\"")), "{text}");
    }
    assert!(run("yaml.parse", json!({}), b"a: [1,,2]\n").is_err());
    assert!(run("yaml.parse", json!({}), b"a: [x: 1]\n").is_err());
    assert_eq!(
        run("yaml.parse", json!({}), b"k: |+\n  a\n").unwrap(),
        json!({"k":"a\n"})
    );
    assert_eq!(
        run("yaml.parse", json!({}), b"k: |+\n  a\n\n\n").unwrap(),
        json!({"k":"a\n\n\n"})
    );
    assert!(run("yaml.parse", json!({}), b"a: [,]\n").is_err());
    assert!(run("yaml.parse", json!({}), b"k: >\n  a\n    b\n  c\n").is_err());
}

#[test]
fn toml_del_roundtrips_and_invalid_inline_extension_and_comment_controls_fail() {
    let output = run(
        "toml.stringify",
        json!({"value":{"k":"\u{7f}","\u{7f}":"v"}}),
        b"",
    )
    .unwrap();
    let text = output.as_str().unwrap();
    assert!(text.contains("\\u007f"));
    assert_eq!(
        run("toml.parse", json!({"text":text}), b"").unwrap(),
        json!({"k":"\u{7f}","\u{7f}":"v"})
    );
    assert!(run("toml.parse", json!({}), b"a = {x=1}\na.y = 2\n").is_err());
    assert!(run("toml.parse", json!({}), b"k=1 # bad \x01 comment\n").is_err());
}

#[test]
fn heterogeneous_json_pointer_filter_treats_non_index_as_missing_and_numbers_by_value() {
    let filtered = run(
        "json.filter",
        json!({
            "document":[{"x":{"name":1}},{"x":[1]},{"x":"s"},{}],
            "where":[{"pointer":"/x/name","exists":false}]
        }),
        b"",
    )
    .unwrap();
    assert_eq!(filtered.as_array().unwrap().len(), 3);
    let zeroes = run(
        "json.filter",
        json!({"document":[{"n":0},{"n":-0.0}],"where":[{"pointer":"/n","equals":0}]}),
        b"",
    )
    .unwrap();
    assert_eq!(zeroes.as_array().unwrap().len(), 2);
    let filtered = run(
        "json.filter",
        json!({"document":[{"n":1},{"n":1.0},{"n":1e0}],"where":[{"pointer":"/n","equals":1}]}),
        b"",
    )
    .unwrap();
    assert_eq!(filtered.as_array().unwrap().len(), 3);
    let missing = run(
        "json.get",
        json!({"document":{"x":[1]},"pointer":"/x/name"}),
        b"",
    );
    assert!(missing.is_err());
    let missing_projection = run(
        "json.map",
        json!({"document":[{"x":[1]}],"select":"/x/name"}),
        b"",
    )
    .expect_err("a non-index array segment is absent");
    assert_eq!(missing_projection.kind, applets::AppletErrorKind::Absence);
    let nested = run(
        "json.filter",
        json!({"document":[{"n":[1]},{"n":[1.0]}],"where":[{"pointer":"/n","equals":[1]}]}),
        b"",
    )
    .unwrap();
    assert_eq!(nested.as_array().unwrap().len(), 2);
}

#[test]
fn json_applet_rejects_duplicate_keys() {
    let error = run("json.parse", json!({"text":"{\"a\":1,\"a\":2}"}), b"")
        .expect_err("duplicate JSON object keys must fail closed");
    assert!(error.message.contains("duplicate key \"a\""));
}

#[test]
fn jsonc_remove_removes_all_duplicate_root_keys_from_one_source() {
    let result = run(
        "jsonc.edit_content",
        json!({"content":"{\"a\":1,\"a\":2,\"b\":3}","key":"a","action":"remove"}),
        b"",
    )
    .unwrap();
    assert_eq!(result, json!("{\"b\":3}"));
}

#[test]
fn jsonc_set_reports_unchanged_when_replacement_bytes_match() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("same.jsonc");
    let original = "{\n  \"a\": 1\n}\n";
    std::fs::write(&path, original).unwrap();
    let read = applets::execute_in_cwd(
        "fs.read",
        &json!({"path":path}).as_object().unwrap().clone(),
        b"",
        directory.path(),
    )
    .unwrap();
    let args = json!({
        "path":path,
        "key":"a",
        "value":1,
        "expected_content_hash":read["content_hash"],
        "symlink_policy":"reject",
        "permissions_policy":"preserve"
    });
    let result = applets::execute_in_cwd(
        "jsonc.set",
        args.as_object().unwrap(),
        b"",
        directory.path(),
    )
    .unwrap();
    assert_eq!(result["changed"], false);
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn yaml_and_toml_stringifiers_refuse_values_past_parser_depth_limit() {
    let mut nested = json!(1);
    for _ in 0..65 {
        nested = json!({"k":nested});
    }
    for operation in ["yaml.stringify", "toml.stringify"] {
        let result = run(operation, json!({"value":nested}), b"");
        assert!(result.is_err(), "{operation} wrote an unreadable value");
    }
}

#[test]
fn csv_text_argument_bom_stripping_is_shared() {
    let mut args = Map::new();
    args.insert("text".into(), Value::String("\u{feff}a\n1\n".into()));
    assert_eq!(
        run("csv.parse", Value::Object(args), b"").unwrap(),
        json!({"headers":["a"],"rows":[["1"]]})
    );
}

#[test]
fn hostile_format_corpus_roundtrips_or_is_refused() {
    let strings = [
        "",
        "yes",
        "no",
        "on",
        "off",
        "2020-01-01",
        "12:30",
        "0x1F",
        "1_000",
        "\u{7f}",
        "\u{1}",
        "\u{feff}",
    ];
    let toml_value = Value::Object(
        strings
            .iter()
            .enumerate()
            .map(|(index, text)| (format!("k{index}"), Value::String((*text).to_owned())))
            .collect(),
    );
    let toml = run("toml.stringify", json!({"value":toml_value}), b"");
    if let Ok(output) = toml {
        assert_eq!(
            run("toml.parse", json!({"text":output}), b"").unwrap(),
            toml_value
        );
    }
    let yaml_value = json!(strings);
    let yaml = run("yaml.stringify", json!({"value":yaml_value}), b"");
    if let Ok(output) = yaml {
        assert_eq!(
            run("yaml.parse", json!({"text":output}), b"").unwrap(),
            yaml_value
        );
    }
    let ini_value = json!({"empty":"", "word":"yes", "del":"\u{7f}"});
    let ini = run("ini.stringify", json!({"value":ini_value}), b"");
    if let Ok(output) = ini {
        assert_eq!(
            run("ini.parse", json!({"text":output}), b"").unwrap(),
            ini_value
        );
    }
    let csv_table = json!({
        "headers":["empty", "tab", "date", "number"],
        "rows":[["", "x\ty", "2020-01-01", "001"]]
    });
    let csv = run("csv.stringify", json!({"table":csv_table}), b"");
    let output = csv.expect("ordinary tabs are valid CSV field content");
    assert_eq!(
        run("csv.parse", json!({"text":output}), b"").unwrap(),
        csv_table
    );
}

#[test]
fn toml_array_of_tables_does_not_forget_unrelated_table_paths() {
    let mut document = String::new();
    for index in 0..40_000 {
        document.push_str(&format!("[t{index}]\nx = 1\n"));
    }
    for _ in 0..40_000 {
        document.push_str("[[a]]\nx = 1\n");
    }
    let parsed = run("toml.parse", json!({"text":document}), b"").unwrap();
    assert_eq!(parsed["a"].as_array().unwrap().len(), 40_000);
}
