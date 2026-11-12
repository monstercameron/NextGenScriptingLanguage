//! Issue #442: the Lua config subset encodes and decodes data only. Round trips must be
//! exact, and anything that is code, a variable, a long bracket or an ambiguous table
//! must be refused with a line and column.

use flow::lua_config::{decode, decode_call, encode, encode_call, MAX_DEPTH};
use serde_json::{json, Number, Value};

fn round_trip(value: &Value) {
    let text = encode(value).expect("encode");
    let back = decode(&text).unwrap_or_else(|e| panic!("decode of {text} failed: {e}"));
    assert_eq!(&back, value, "encoded as {text}");
}

#[test]
fn tricky_strings_round_trip() {
    let samples = [
        "plain",
        "say \"hi\"",
        "back\\slash",
        "line\nbreak\r\n",
        "tab\tand\u{0b}\u{0c}",
        "nul\0byte",
        "]] close bracket",
        "[[open long",
        "--[[ looks like a comment",
        "'single' and \"double\"",
        "héllo wörld ✓ 日本語 🦀",
        "\u{7f}\u{1b}[0m",
        "",
    ];
    for s in samples {
        round_trip(&Value::String(s.to_string()));
    }
}

#[test]
fn string_encoding_uses_decimal_escapes() {
    assert_eq!(encode(&json!("a\"b")).unwrap(), "\"a\\034b\"");
    assert_eq!(encode(&json!("a\\b")).unwrap(), "\"a\\092b\"");
    assert_eq!(encode(&json!("x\ny")).unwrap(), "\"x\\010y\"");
    assert_eq!(encode(&json!("\0")).unwrap(), "\"\\000\"");
    assert_eq!(encode(&json!("é")).unwrap(), "\"\\195\\169\"");
    assert_eq!(encode(&json!("]]")).unwrap(), "\"]]\"");
}

#[test]
fn scalars_and_numbers_round_trip() {
    for v in [
        Value::Null,
        json!(true),
        json!(false),
        json!(0),
        json!(-1),
        json!(i64::MIN),
        json!(i64::MAX),
        json!(1.0),
        json!(-0.5),
        json!(1e300),
        json!(1e-7),
        json!(f64::MIN_POSITIVE),
        json!(123456.789),
    ] {
        round_trip(&v);
    }
    assert_eq!(encode(&json!(1.0)).unwrap(), "1.0");
    assert_eq!(decode("1e2").unwrap(), json!(100.0));
    assert!(decode("1e2").unwrap().as_f64().is_some());
}

#[test]
fn integer_out_of_range_is_refused_on_encode_and_decode() {
    assert!(encode(&json!(u64::MAX)).is_err());
    assert!(decode("9223372036854775808").is_err());
    assert!(decode("1e400").is_err());
}

#[test]
fn nested_values_round_trip() {
    round_trip(&json!({
        "list": [1, 2.5, -3, true, null, "x"],
        "nested": { "deep": [{ "k": "v" }, [[1]]] },
        "end": { "function": 1, "and": 2, "nil": 3 },
        "odd key ]]\"\\": "value",
        "": "empty key",
        "emoji 🦀": ["ok"]
    }));
}

#[test]
fn encoding_is_deterministic_and_bracketed() {
    assert_eq!(
        encode(&json!({ "b": 1, "a": 2 })).unwrap(),
        "{ [\"a\"] = 2, [\"b\"] = 1 }"
    );
    assert_eq!(encode(&json!([1, "x", null])).unwrap(), "{ 1, \"x\", nil }");
    assert_eq!(encode(&json!({})).unwrap(), "{}");
}

#[test]
fn empty_array_documents_its_limit() {
    // `{}` is one form for both; it decodes as an empty record.
    assert_eq!(encode(&json!([])).unwrap(), "{}");
    assert_eq!(decode("{}").unwrap(), json!({}));
}

#[test]
fn generated_values_round_trip() {
    // Deterministic xorshift generator; no external dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }
    const CHARS: &[char] = &[
        'a', 'Z', '0', '"', '\\', ' ', '\n', '\r', '\t', '\0', '\u{7f}', 'é', '日', ']', '[', '=',
        '-', ',', ';', '{', '}', '\'', '✓', '🦀',
    ];
    fn text(rng: &mut Rng) -> String {
        let len = rng.below(9);
        (0..len)
            .map(|_| CHARS[rng.below(CHARS.len() as u64) as usize])
            .collect()
    }
    fn gen(rng: &mut Rng, depth: usize) -> Value {
        let kind = if depth >= 3 {
            rng.below(5)
        } else {
            rng.below(8)
        };
        match kind {
            0 => Value::Null,
            1 => Value::Bool(rng.below(2) == 0),
            2 => json!(rng.next() as i64),
            3 => {
                let f = f64::from_bits(rng.next());
                if f.is_finite() {
                    Value::Number(Number::from_f64(f).unwrap())
                } else {
                    json!(0.5)
                }
            }
            4 => Value::String(text(rng)),
            5 => Value::Array((0..=rng.below(3)).map(|_| gen(rng, depth + 1)).collect()),
            _ => {
                let mut map = serde_json::Map::new();
                for _ in 0..rng.below(4) {
                    map.insert(text(rng), gen(rng, depth + 1));
                }
                Value::Object(map)
            }
        }
    }
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..2000 {
        let v = gen(&mut rng, 0);
        round_trip(&v);
    }
}

#[test]
fn decodes_plain_identifiers_quotes_escapes_and_separators() {
    assert_eq!(
        decode("{ name = \"x\", _under = 1; other = 'it\\'s' }").unwrap(),
        json!({ "name": "x", "_under": 1, "other": "it's" })
    );
    assert_eq!(
        decode("'\\a\\b\\f\\n\\r\\t\\v\\\\\\\"\\''").unwrap(),
        Value::String("\u{7}\u{8}\u{c}\n\r\t\u{b}\\\"'".to_string())
    );
    assert_eq!(decode("{1;2;}").unwrap(), json!([1, 2]));
    assert_eq!(decode("{ 1, 2, }").unwrap(), json!([1, 2]));
    assert_eq!(decode("  nil  ").unwrap(), Value::Null);
}

#[test]
fn comments_are_skipped() {
    assert_eq!(
        decode("-- header\n--[[ block\n]] { a = 1, -- trailing\n b = 2; }").unwrap(),
        json!({ "a": 1, "b": 2 })
    );
}

#[test]
fn hostile_inputs_are_refused() {
    let refused = [
        "os.execute(\"x\")",
        "a = b",
        "{ 1 + 1 }",
        "{ 1 - 1 }",
        "{ 1 } .. \"x\"",
        "\"unterminated",
        "\"a\nb\"",
        "\"a\rb\"",
        "--[[ never closed",
        "1 --[==[ level comment ]==]",
        "[[long]]",
        "{ [[long]] }",
        "{ [==[x]==] = 1 }",
        "{ f() }",
        "function() end",
        "{ a = 1, a = 2 }",
        "{ 1, a = 2 }",
        "{ a = 1, 2 }",
        "{ 1 2 }",
        "{ , }",
        "{ 1,, 2 }",
        "{ [1] = \"x\" }",
        "{ [true] = 1 }",
        "{ end = 1 }",
        "{ nil = 1 }",
        "{ [\"a\" = 1 }",
        "0x10",
        "1.",
        ".5",
        "1..2",
        "\"\\x41\"",
        "\"\\300\"",
        "\"\\q\"",
        "\"\\u{41}\"",
        "x",
        "and",
        "\"a\" \"b\"",
        "{ 1 ",
        "{ \"a\" ",
        "\"\\ddd\"",
    ];
    for src in refused {
        assert!(decode(src).is_err(), "should refuse {src:?}");
    }
}

#[test]
fn refusals_report_line_and_column() {
    let err = decode("{\n  1,\n  a = 2\n}").unwrap_err();
    assert_eq!((err.line, err.column), (3, 3), "{err}");
    assert!(err.message.contains("mix"), "{err}");

    let err = decode("{ 1 + 1 }").unwrap_err();
    assert_eq!((err.line, err.column), (1, 5), "{err}");

    let err = decode("{ a = 1, a = 2 }").unwrap_err();
    assert!(err.message.contains("duplicate"), "{err}");
}

#[test]
fn mixed_tables_are_refused_not_guessed() {
    assert!(decode("{ 1, [\"a\"] = 2 }").is_err());
    assert!(decode("{ [\"a\"] = 2, 1 }").is_err());
}

#[test]
fn nesting_limit_is_enforced_on_both_sides() {
    let ok_depth = "{".repeat(MAX_DEPTH) + &"}".repeat(MAX_DEPTH);
    assert!(decode(&ok_depth).is_ok());
    let too_deep = "{".repeat(MAX_DEPTH + 1) + &"}".repeat(MAX_DEPTH + 1);
    assert!(decode(&too_deep).is_err());

    let mut v = json!(1);
    for _ in 0..MAX_DEPTH {
        v = Value::Array(vec![v]);
    }
    assert!(encode(&v).is_ok());
    v = Value::Array(vec![v]);
    assert!(encode(&v).is_err());
}

#[test]
fn call_file_round_trips_with_tricky_arguments() {
    let args = json!({
        "workspace": "name with \"quote\" and \\backslash",
        "mode": "tile",
        "note": "line\nbreak ]] \u{1f980}"
    });
    let text = encode_call("o.workspace_mode", &args).unwrap();
    assert!(text.starts_with("o.workspace_mode({ "), "{text}");
    let call = decode_call(&text).unwrap();
    assert_eq!(call.function, "o.workspace_mode");
    assert_eq!(call.args, args);
}

#[test]
fn call_file_reads_omarchy_shape_and_refuses_other_calls() {
    let call = decode_call("o.workspace_mode({ workspace = \"1\", mode = \"tile\" })\n").unwrap();
    assert_eq!(call.function, "o.workspace_mode");
    assert_eq!(call.args, json!({ "workspace": "1", "mode": "tile" }));

    let call = decode_call("-- note\no.f({}); ").unwrap();
    assert_eq!(call.args, json!({}));

    let refused = [
        "os.execute(\"x\")",
        "os.execute({ a = 1 }) os.execute({ b = 2 })",
        "a = b",
        "o:method({})",
        "o.f({1, 2})",
        "o.f({ a = 1 }) .. \"x\"",
        "o.f{ a = 1 }",
        "o..f({})",
        "o.end({})",
        "{ a = 1 }",
    ];
    for src in refused {
        assert!(decode_call(src).is_err(), "should refuse {src:?}");
    }
}

#[test]
fn call_encoder_refuses_bad_names_and_non_records() {
    assert!(encode_call("o.end", &json!({})).is_err());
    assert!(encode_call("o:f", &json!({})).is_err());
    assert!(encode_call("o.1x", &json!({})).is_err());
    assert!(encode_call("o.f", &json!([1])).is_err());
    assert!(encode_call("o.f", &json!("x")).is_err());
}

#[test]
fn encode_error_names_the_path() {
    let err = encode(&json!({ "hosts": [1, u64::MAX] })).unwrap_err();
    assert!(err.path.contains("hosts"), "{err}");
}
