//! Strict YAML, TOML and INI subsets: parse to JSON and stringify from JSON.
//! Portable: no process is spawned and no file is read.

use flow::applets::{
    execute, execute_with_context, AppletError, AppletErrorKind, ExecutionBudgetExceeded,
    ExecutionContext,
};
use serde_json::{json, Map, Value};
use std::cell::Cell;
use std::time::Instant;

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn parse(operation: &str, text: &str) -> Result<Value, AppletError> {
    execute(operation, &args(json!({"text": text})), &[])
}

fn stringify(operation: &str, value: Value) -> Result<Value, AppletError> {
    execute(operation, &args(json!({"value": value})), &[])
}

fn yaml(text: &str) -> Result<Value, AppletError> {
    parse("yaml.parse", text)
}

fn toml(text: &str) -> Result<Value, AppletError> {
    parse("toml.parse", text)
}

fn ini(text: &str) -> Result<Value, AppletError> {
    parse("ini.parse", text)
}

#[test]
fn yaml_block_mappings_sequences_and_scalar_resolution() {
    let document = r#"name: eth0
addresses:
  - 192.168.1.2/24
  - "10.0.0.1/8"
dhcp4: false
mtu: 1500
ratio: 0.5
exp: 1e3
hex: 0x1F
underscored: 1_000
upper: True
empty:
nothing: ~
"#;
    assert_eq!(
        yaml(document).unwrap(),
        json!({
            "name": "eth0",
            "addresses": ["192.168.1.2/24", "10.0.0.1/8"],
            "dhcp4": false,
            "mtu": 1500,
            "ratio": 0.5,
            "exp": 1000.0,
            "hex": "0x1F",
            "underscored": "1_000",
            "upper": true,
            "empty": null,
            "nothing": null
        })
    );
}

#[test]
fn yaml_netplan_shape_with_flow_sequences_and_quoted_keys() {
    let document = r#"network:
  version: 2
  ethernets:
    eno1:
      dhcp4: true
      addresses: [10.0.0.5/24, 10.0.0.6/24]
      nameservers:
        addresses: [1.1.1.1, "8.8.8.8"]
        search:
          - example.com
    wlan0:
      access-points:
        "My Net":
          password: "p@ss: word # not a comment"
"#;
    let value = yaml(document).unwrap();
    assert_eq!(value["network"]["version"], json!(2));
    assert_eq!(
        value["network"]["ethernets"]["eno1"]["addresses"],
        json!(["10.0.0.5/24", "10.0.0.6/24"])
    );
    assert_eq!(
        value["network"]["ethernets"]["eno1"]["nameservers"]["search"],
        json!(["example.com"])
    );
    assert_eq!(
        value["network"]["ethernets"]["wlan0"]["access-points"]["My Net"]["password"],
        json!("p@ss: word # not a comment")
    );
}

#[test]
fn yaml_compact_mappings_and_nested_sequences() {
    assert_eq!(
        yaml("- name: a\n  value: 1\n- name: b\n  value: 2\n").unwrap(),
        json!([{"name": "a", "value": 1}, {"name": "b", "value": 2}])
    );
    assert_eq!(
        yaml("- - a\n  - b\n- c\n").unwrap(),
        json!([["a", "b"], "c"])
    );
    assert_eq!(
        yaml("key:\n- one\n- two\nother: 3\n").unwrap(),
        json!({"key": ["one", "two"], "other": 3})
    );
}

#[test]
fn yaml_block_scalars_follow_their_chomping_rules() {
    let document = "script: |\n  line one\n    indented\n  line three\nfolded: >\n  one\n  two\n\n  three\nkeep: |+\n  x\n\nstrip: |-\n  y\n";
    assert_eq!(
        yaml(document).unwrap(),
        json!({
            "script": "line one\n  indented\nline three\n",
            "folded": "one two\nthree\n",
            "keep": "x\n\n",
            "strip": "y"
        })
    );
}

#[test]
fn yaml_quoted_flow_comments_and_document_markers() {
    assert_eq!(yaml(r#"a: 'it''s'"#).unwrap(), json!({"a": "it's"}));
    assert_eq!(
        yaml(r#"a: "tab\there\x41""#).unwrap(),
        json!({"a": "tab\thereA"})
    );
    assert_eq!(
        yaml("x: [a, 'b c', {k: v, \"q\": 2}]\n").unwrap(),
        json!({"x": ["a", "b c", {"k": "v", "q": 2}]})
    );
    assert_eq!(
        yaml("---\na: 1 # trailing\n# full line\nb: 2\n").unwrap(),
        json!({"a": 1, "b": 2})
    );
    assert_eq!(yaml("").unwrap(), json!(null));
    assert_eq!(yaml("just a scalar\n").unwrap(), json!("just a scalar"));
}

#[test]
fn yaml_rejects_unsupported_and_malformed_input_with_line_numbers() {
    let cases = [
        ("a: 1\na: 2\n", "duplicate key"),
        ("a:\n\tb: 1\n", "tabs"),
        ("a: &x 1\n", "anchors"),
        ("b: *x\n", "aliases"),
        ("c: !!str 1\n", "tags"),
        ("a: 1\n---\nb: 2\n", "multiple documents"),
        ("a:\n  b: 1\n c: 2\n", "indentation"),
        ("a: \"unclosed\n", "not closed"),
        ("a: .inf\n", "infinite"),
        ("- a\nb: 1\n", "unexpected content"),
        ("a: [1, 2\n", "not closed"),
        ("a: key: value\n", "mapping values"),
        ("a: \"x\" y\n", "unexpected text"),
    ];
    for (document, expected) in cases {
        let error = yaml(document).unwrap_err();
        assert_eq!(error.exit_status(), 2, "{document:?}");
        assert!(
            error.message.contains(expected),
            "{document:?}: {}",
            error.message
        );
        assert!(
            error.message.contains("line"),
            "{document:?}: {}",
            error.message
        );
    }
}

#[test]
fn yaml_stringify_quotes_only_when_needed_and_round_trips() {
    assert_eq!(
        stringify(
            "yaml.stringify",
            json!({"name": "eth0", "addresses": ["10.0.0.5/24"]})
        )
        .unwrap(),
        json!("addresses:\n  - 10.0.0.5/24\nname: eth0\n")
    );
    let values = json!({
        "numeric_text": "1.0",
        "boolean_text": "yes",
        "empty": "",
        "dash": "- item",
        "padded": "  pad  ",
        "colon": "a: b",
        "multiline": "line\nbreak",
        "nested": {"list": [1, 2.5, null, true, {"k": "v"}], "empty_map": {}, "empty_list": []},
        "unicode": "雪 é"
    });
    let text = stringify("yaml.stringify", values.clone()).unwrap();
    let round_trip = yaml(text.as_str().unwrap()).unwrap();
    assert_eq!(round_trip, values, "{text}");
}

#[test]
fn toml_tables_arrays_of_tables_and_inline_values() {
    let document = r#"title = "TOML Example"

[owner]
name = "Tom"

[database]
enabled = true
ports = [ 8000, 8001, 8002, ]
data = [ ["delta", "phi"], [3.14] ]
temp_targets = { cpu = 79.5, case = 72.0 }

[servers.alpha]
ip = "10.0.0.1"

[[products]]
name = "Hammer"
sku = 738594937

[[products]]
name = "Nail"
color = "gray"
"#;
    assert_eq!(
        toml(document).unwrap(),
        json!({
            "title": "TOML Example",
            "owner": {"name": "Tom"},
            "database": {
                "enabled": true,
                "ports": [8000, 8001, 8002],
                "data": [["delta", "phi"], [3.14]],
                "temp_targets": {"cpu": 79.5, "case": 72.0}
            },
            "servers": {"alpha": {"ip": "10.0.0.1"}},
            "products": [
                {"name": "Hammer", "sku": 738594937},
                {"name": "Nail", "color": "gray"}
            ]
        })
    );
}

#[test]
fn toml_dotted_keys_implicit_tables_and_numbers() {
    assert_eq!(
        toml("a.b.c = 1\na.d = 2\n").unwrap(),
        json!({"a": {"b": {"c": 1}, "d": 2}})
    );
    assert_eq!(
        toml("[a.b]\nx = 1\n[a]\ny = 2\n").unwrap(),
        json!({"a": {"b": {"x": 1}, "y": 2}})
    );
    assert_eq!(
        toml("hex = 0xff\noct = 0o17\nbin = 0b101\nbig = 1_000\nneg = -42\nplus = +3\nexp = 3.0e2\nhalf = -0.5\n").unwrap(),
        json!({"hex": 255, "oct": 15, "bin": 5, "big": 1000, "neg": -42, "plus": 3, "exp": 300.0, "half": -0.5})
    );
}

#[test]
fn toml_strings_including_multi_line_and_escapes() {
    let basic = concat!("s = \"", "\\", "u00e9", "\\t\\\"", "\"\n");
    assert_eq!(toml(basic).unwrap(), json!({"s": "é\t\""}));
    assert_eq!(
        toml("s = \"\"\"\nline1\nline2\"\"\"\n").unwrap(),
        json!({"s": "line1\nline2"})
    );
    assert_eq!(
        toml(
            r#"l = '''raw \n'''
"#
        )
        .unwrap(),
        json!({"l": r"raw \n"})
    );
    assert_eq!(
        toml("arr = [\n  1, # one\n  2,\n]\n").unwrap(),
        json!({"arr": [1, 2]})
    );
}

#[test]
fn toml_rejects_unsupported_and_inconsistent_documents() {
    let cases = [
        ("[a]\nx = 1\n[a]\n", "repeats"),
        ("a = 1\n[a]\n", "repeats"),
        ("a = { b = 1 }\n[a.c]\n", "extends a value"),
        ("[[a]]\n[a]\n", "conflicts"),
        ("x = 1\nx = 2\n", "duplicate key"),
        ("d = 1979-05-27\n", "dates"),
        ("t = 07:32:00\n", "dates"),
        ("v = inf\n", "inf"),
        ("n = 01\n", "leading zeros"),
        ("s = \"bad\\q\"\n", "escape"),
        ("s = \"unterminated\n", "unterminated"),
        ("x = [1, 2\n", "expected"),
        ("x = 1 2\n", "unexpected"),
        ("x = 1_\n", "invalid"),
    ];
    for (document, expected) in cases {
        let error = toml(document).unwrap_err();
        assert_eq!(error.exit_status(), 2, "{document:?}");
        assert!(
            error.message.contains(expected),
            "{document:?}: {}",
            error.message
        );
    }
}

#[test]
fn toml_stringify_round_trips_tables_and_arrays_of_tables() {
    let value = json!({
        "title": "x",
        "owner": {"name": "Tom", "tags": ["a", "b"]},
        "products": [{"name": "Hammer", "sku": 1}, {"name": "Nail"}],
        "ports": [1, 2],
        "ratio": 1.0,
        "odd key": "value \"quoted\"",
        "empty": {}
    });
    let text = stringify("toml.stringify", value.clone()).unwrap();
    let round_trip = toml(text.as_str().unwrap()).unwrap();
    assert_eq!(round_trip, value, "{text}");
    assert!(stringify("toml.stringify", json!({"n": null})).is_err());
    assert!(stringify("toml.stringify", json!([1, 2])).is_err());
}

#[test]
fn ini_sections_keys_and_text_values() {
    let document = "; comment\ntop = 1\n[core]\neditor = vim   \n# note\n[net]\nip: 10.0.0.1\nmask = 255.0.0.0\n";
    assert_eq!(
        ini(document).unwrap(),
        json!({
            "top": "1",
            "core": {"editor": "vim"},
            "net": {"ip": "10.0.0.1", "mask": "255.0.0.0"}
        })
    );
}

#[test]
fn ini_rejects_duplicates_continuations_and_bad_lines() {
    let cases = [
        ("a = 1\na = 2\n", "duplicate key"),
        ("[s]\n[s]\n", "duplicate key"),
        ("[s]\na = 1\n  continued\n", "continuation"),
        ("junk\n", "expected key"),
        ("[]\n", "section name is empty"),
        ("[open\n", "not closed"),
        ("a = 1\n[a]\nb = 2\n", "same name"),
        ("= value\n", "key is empty"),
    ];
    for (document, expected) in cases {
        let error = ini(document).unwrap_err();
        assert_eq!(error.exit_status(), 2, "{document:?}");
        assert!(
            error.message.contains(expected),
            "{document:?}: {}",
            error.message
        );
    }
}

#[test]
fn ini_stringify_writes_top_level_then_sections() {
    assert_eq!(
        stringify(
            "ini.stringify",
            json!({"top": "1", "core": {"editor": "vim", "count": 2, "on": true}})
        )
        .unwrap(),
        json!("top = 1\n\n[core]\ncount = 2\neditor = vim\non = true\n")
    );
    assert!(stringify("ini.stringify", json!({"core": {"deep": {"x": 1}}})).is_err());
    assert!(stringify("ini.stringify", json!({"n": null})).is_err());
    assert!(stringify("ini.stringify", json!({"list": [1]})).is_err());
    assert!(stringify("ini.stringify", json!({"v": "two\nlines"})).is_err());
    assert!(stringify("ini.stringify", json!({"v": " padded "})).is_err());
}

#[test]
fn stdin_is_used_when_text_is_absent() {
    let value = execute("yaml.parse", &Map::new(), b"a: 1\n").unwrap();
    assert_eq!(value, json!({"a": 1}));
    let value = execute("toml.parse", &Map::new(), b"a = 1\n").unwrap();
    assert_eq!(value, json!({"a": 1}));
    let value = execute("ini.parse", &Map::new(), b"a = 1\n").unwrap();
    assert_eq!(value, json!({"a": "1"}));
}

// Resource limits: nesting, dotted-key components, value counts, input size,
// line-linear error reporting and cooperative cancellation.

fn parse_timed(operation: &str, text: &str) -> (Result<Value, AppletError>, std::time::Duration) {
    let started = Instant::now();
    let result = parse(operation, text);
    (result, started.elapsed())
}

#[test]
fn toml_dotted_keys_and_headers_are_bounded_by_the_nesting_limit() {
    let sixty_four = vec!["a"; 64].join(".");
    assert!(toml(&format!("{sixty_four} = 1\n")).is_ok());
    let sixty_five = vec!["a"; 65].join(".");
    assert!(toml(&format!("{sixty_five} = 1\n")).is_err());

    let huge_key = vec!["a"; 10_000].join(".");
    let (result, elapsed) = parse_timed("toml.parse", &format!("{huge_key} = 1\n"));
    let error = result.unwrap_err();
    assert!(error.message.contains("exceeds"), "{}", error.message);
    assert!(elapsed.as_secs_f64() < 5.0, "dotted key took {elapsed:?}");

    let (result, elapsed) = parse_timed("toml.parse", &format!("[{huge_key}]\nx = 1\n"));
    assert!(result.unwrap_err().message.contains("exceeds"));
    assert!(elapsed.as_secs_f64() < 5.0, "deep header took {elapsed:?}");
}

#[test]
fn toml_inline_table_dotted_keys_share_the_nesting_budget() {
    let deep_inline = format!("x = {{ {} = 1 }}\n", vec!["a"; 64].join("."));
    assert!(toml(&deep_inline).is_err());
    let at_limit = format!("x = {{ {} = 1 }}\n", vec!["a"; 63].join("."));
    assert!(toml(&at_limit).is_ok());
}

#[test]
fn oversized_inputs_fail_before_parsing_in_every_format() {
    let big = "a = 1\n".repeat(3_000_000);
    assert!(big.len() > 16 * 1024 * 1024);
    for operation in ["toml.parse", "yaml.parse", "ini.parse"] {
        let (result, elapsed) = parse_timed(operation, &big);
        let error = result.unwrap_err();
        assert!(
            error.message.contains("byte limit"),
            "{operation}: {}",
            error.message
        );
        assert!(elapsed.as_secs_f64() < 2.0, "{operation} took {elapsed:?}");
    }
}

#[test]
fn toml_line_numbers_are_correct_without_quadratic_rescans() {
    let mut document = String::new();
    for index in 0..50_000 {
        document.push_str(&format!("k{index} = 1\n"));
    }
    document.push_str("bad line\n");
    let (result, elapsed) = parse_timed("toml.parse", &document);
    assert!(result.unwrap_err().message.contains("toml line 50001"));
    assert!(
        elapsed.as_secs_f64() < 5.0,
        "line-numbered error took {elapsed:?}"
    );
}

#[test]
fn toml_large_flat_array_parses_in_linear_time() {
    let mut document = String::from("a = [");
    for _ in 0..400_000 {
        document.push_str("1,");
    }
    document.push_str("1]\n");
    assert!(document.len() > 800_000 && document.len() < 1_100_000);
    let (result, elapsed) = parse_timed("toml.parse", &document);
    let value = result.unwrap();
    assert_eq!(value["a"].as_array().unwrap().len(), 400_001);
    assert!(elapsed.as_secs_f64() < 10.0, "1 MB array took {elapsed:?}");
}

#[test]
fn value_cap_applies_to_toml_arrays_and_yaml_flow_collections() {
    let mut toml_document = String::from("a = [");
    for _ in 0..1_100_000 {
        toml_document.push_str("1,");
    }
    toml_document.push_str("1]\n");
    let error = toml(&toml_document).unwrap_err();
    assert!(error.message.contains("value limit"), "{}", error.message);

    let flow = format!("[{}1]", "1,".repeat(1_100_000));
    let error = yaml(&flow).unwrap_err();
    assert!(error.message.contains("value limit"), "{}", error.message);
}

#[test]
fn deep_yaml_flow_nesting_is_bounded() {
    let deep = format!("{}{}", "[".repeat(100), "]".repeat(100));
    let error = yaml(&deep).unwrap_err();
    assert!(error.message.contains("nesting"), "{}", error.message);
    let deep_map = format!("{}1{}", "{a: ".repeat(100), "}".repeat(100));
    assert!(yaml(&deep_map).is_err());
}

#[test]
fn ini_value_cap_bounds_many_distinct_keys() {
    let mut document = String::new();
    for index in 0..1_100_000 {
        document.push_str(&format!("k{index}=1\n"));
    }
    assert!(document.len() < 16 * 1024 * 1024);
    let error = ini(&document).unwrap_err();
    assert!(error.message.contains("value limit"), "{}", error.message);
}

/// A context that reports cancellation after a few checkpoints. Checkpoints run
/// every 256 parser steps, so reaching cancellation proves the parse was
/// interrupted in the middle of the input rather than only at entry.
fn cancel_after(checks: usize) -> impl Fn() -> Result<(), ExecutionBudgetExceeded> {
    let calls = Cell::new(0usize);
    move || {
        let next = calls.get() + 1;
        calls.set(next);
        if next > checks {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    }
}

fn run_with_checkpoint(
    operation: &str,
    text: &str,
    checkpoint: &dyn Fn() -> Result<(), ExecutionBudgetExceeded>,
) -> Result<Value, AppletError> {
    let context = ExecutionContext::new(checkpoint);
    let arguments = json!({ "text": text });
    let arguments = arguments.as_object().cloned().unwrap_or_default();
    execute_with_context(
        operation,
        &arguments,
        &[],
        std::path::Path::new("."),
        &context,
    )
}

#[test]
fn cancelled_context_stops_long_toml_yaml_and_ini_parses() {
    let mut toml_document = String::from("a = [");
    for _ in 0..400_000 {
        toml_document.push_str("1,");
    }
    toml_document.push_str("1]\n");
    let checkpoint = cancel_after(3);
    let error = run_with_checkpoint("toml.parse", &toml_document, &checkpoint).unwrap_err();
    assert_eq!(error.kind, AppletErrorKind::Cancelled);

    let yaml_flow = format!("[{}1]", "1,".repeat(400_000));
    let checkpoint = cancel_after(3);
    let error = run_with_checkpoint("yaml.parse", &yaml_flow, &checkpoint).unwrap_err();
    assert_eq!(error.kind, AppletErrorKind::Cancelled);

    let mut ini_document = String::new();
    for index in 0..100_000 {
        ini_document.push_str(&format!("k{index}=1\n"));
    }
    let checkpoint = cancel_after(3);
    let error = run_with_checkpoint("ini.parse", &ini_document, &checkpoint).unwrap_err();
    assert_eq!(error.kind, AppletErrorKind::Cancelled);
}
