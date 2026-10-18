use flow::applets;
use serde_json::{json, Value};

fn run(name: &str, args: Value, input: &[u8]) -> Result<Value, applets::AppletError> {
    applets::execute(name, args.as_object().expect("object args"), input)
}

#[test]
fn uppercase_keeps_unicode_expansions_and_ascii_mapping() {
    assert_eq!(
        run("text.upper", json!({"text":"ascii Straße ﬃ"}), &[]).unwrap(),
        json!("ASCII STRASSE FFI")
    );
    assert_eq!(
        run("text.upper", json!({"text":""}), &[]).unwrap(),
        json!("")
    );
}

#[test]
fn count_lines_by_preserves_field_and_first_seen_semantics() {
    assert_eq!(
        run(
            "text.count_lines_by",
            json!({"delimiter":"::","field":1}),
            b"left::alpha::x\nleft::beta::y\nright::alpha::z\n"
        )
        .unwrap(),
        json!([{"key":"alpha","count":2},{"key":"beta","count":1}])
    );
    assert_eq!(
        run(
            "text.count_lines_by",
            json!({"delimiter":":","field":1}),
            b"a::b\na:c\n"
        )
        .unwrap(),
        json!([{"key":"","count":1},{"key":"c","count":1}])
    );
}

#[test]
fn borrowed_count_keys_preserve_unicode_empty_fields_and_missing_field_errors() {
    for delimiter in [":", "::", "雪"] {
        let input = format!("a{delimiter}é\r\nb{delimiter}\nc{delimiter}é\n");
        assert_eq!(
            run(
                "text.count_lines_by",
                json!({"delimiter": delimiter, "field": 1}),
                input.as_bytes()
            )
            .unwrap(),
            json!([{"key": "é", "count": 2}, {"key": "", "count": 1}])
        );
        let error = run(
            "text.count_lines_by",
            json!({"delimiter": delimiter, "field": 2}),
            input.as_bytes(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("line has no field 2"));
    }
}

#[test]
fn url_format_percent_encodes_utf8_bytes_with_exact_output() {
    assert_eq!(
        run(
            "text.format",
            json!({"template":"{{ value }}","values":{"value":"a /é~"},"escape":"url"}),
            &[]
        )
        .unwrap(),
        json!("a%20%2F%C3%A9~")
    );
}

#[test]
fn case_mapping_matches_unicode_at_ascii_block_boundaries() {
    const ASCII: &str = "aZ[{{09\0\n";
    for length in 0..=257 {
        let prefix = ASCII.repeat(length / ASCII.len() + 1);
        for suffix in ["", "Straße ﬃ", "İΟΣ", "é漢字"] {
            let text = format!("{}{suffix}", &prefix[..length]);
            for (operation, expected) in [
                ("text.upper", text.to_uppercase()),
                ("text.lower", text.to_lowercase()),
            ] {
                assert_eq!(
                    run(operation, json!({"text": text}), &[]).unwrap(),
                    json!(expected),
                    "operation={operation}, length={length}, suffix={suffix:?}"
                );
                assert_eq!(
                    run(operation, json!({}), text.as_bytes()).unwrap(),
                    json!(expected)
                );
            }
        }
    }
}

#[test]
fn case_mapping_rejects_invalid_utf8_stdin() {
    for operation in ["text.upper", "text.lower"] {
        for bytes in [&b"ascii\xff"[..], &b"\xc3"[..], &b"\xf0\x80\x80\x80"[..]] {
            let error = run(operation, json!({}), bytes).unwrap_err();
            assert!(error.to_string().contains("stdin must be UTF-8"));
        }
    }
}
