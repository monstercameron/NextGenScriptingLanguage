use flow::applets;
use serde_json::{json, Map, Value};

fn format(mode: &str, value: &str) -> Result<Value, applets::AppletError> {
    applets::execute(
        "text.format",
        &Map::from_iter([
            ("template".to_owned(), json!("{{x}}")),
            ("values".to_owned(), json!({"x": value})),
            ("escape".to_owned(), json!(mode)),
        ]),
        &[],
    )
}

#[test]
fn text_format_requires_an_explicit_mode_and_escapes_hostile_values() {
    let hostile = "quote'\" & < >,\n\0 bidi\u{202e} \u{e9}";
    assert_eq!(format("plain", hostile).unwrap(), json!(hostile));
    let json_escaped = format("json", hostile)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    assert!(json_escaped.contains("\\n") && json_escaped.contains("\\u0000"));
    assert_eq!(
        format("json", "ends with \"").unwrap(),
        json!("ends with \\\"")
    );
    assert_eq!(
        format("url", "a b&\u{e9}").unwrap(),
        json!("a%20b%26%C3%A9")
    );
    assert_eq!(format("csv", "a,\"b\"").unwrap(), json!("\"a,\"\"b\"\"\""));
    assert_eq!(
        format("html", "<a&\"' >").unwrap(),
        json!("&lt;a&amp;&quot;&#39; &gt;")
    );
    assert_eq!(format("regex", "a.b[0]").unwrap(), json!("a\\.b\\[0\\]"));
    assert_eq!(format("shell", "a'b").unwrap(), json!("'a'\\''b'"));
    assert!(format("shell", "\0").unwrap_err().message.contains("NUL"));
    let args = Map::from_iter([
        ("template".to_owned(), json!("{{x}}")),
        ("values".to_owned(), json!({"x":"v"})),
    ]);
    assert!(applets::execute("text.format", &args, &[]).is_err());
}
