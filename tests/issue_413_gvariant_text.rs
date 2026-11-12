use flow::gsettings_value::parse_gvariant_text;
use serde_json::json;

#[test]
fn gsettings_scalars_map_to_json() {
    assert_eq!(parse_gvariant_text("'dark'").unwrap(), json!("dark"));
    assert_eq!(parse_gvariant_text("true\n").unwrap(), json!(true));
    assert_eq!(parse_gvariant_text("1.5").unwrap(), json!(1.5));
    assert_eq!(parse_gvariant_text("uint32 7").unwrap(), json!(7));
    assert_eq!(parse_gvariant_text("int32 -3").unwrap(), json!(-3));
    assert_eq!(parse_gvariant_text("double 2").unwrap(), json!(2.0));
}

#[test]
fn gsettings_containers_map_to_json() {
    assert_eq!(
        parse_gvariant_text("['a', 'b']").unwrap(),
        json!(["a", "b"])
    );
    assert_eq!(parse_gvariant_text("@as []").unwrap(), json!([]));
    assert_eq!(parse_gvariant_text("(1, 'x')").unwrap(), json!([1, "x"]));
    assert_eq!(
        parse_gvariant_text("@a{sv} {'theme': <'dark'>, 'scale': <1.25>}").unwrap(),
        json!({"theme": "dark", "scale": 1.25})
    );
    assert_eq!(parse_gvariant_text("@a{sv} {}").unwrap(), json!({}));
}

#[test]
fn malformed_gsettings_text_returns_a_typed_error() {
    for bad in [
        "",
        "'unterminated",
        "[1, 2",
        "uint32 -1",
        "byte 256",
        "{1: 'a'}",
        "nothing",
        "inf",
        "1.",
        "'a' 'b'",
        "{'k': 1, 'k': 2}",
        "'\\q'",
    ] {
        let err = parse_gvariant_text(bad).expect_err(bad);
        assert!(
            !err.message.is_empty(),
            "{bad:?} should explain the failure"
        );
    }
}
