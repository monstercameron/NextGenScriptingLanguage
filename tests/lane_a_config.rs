//! Strict YAML, TOML and INI subsets: parse to JSON and stringify from JSON.
//! Portable: no process is spawned and no file is read.

use flow::applets::{execute, AppletError};
use serde_json::{json, Map, Value};

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
