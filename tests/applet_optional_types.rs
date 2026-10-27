//! Direct applet calls must distinguish omitted options from invalid supplied types.
use flow::applets::{self, AppletErrorKind};
use serde_json::{json, Value};
use std::process::Command;

fn call(name: &str, args: &Value) -> Result<Value, applets::AppletError> {
    applets::execute(name, args.as_object().unwrap(), b"")
}

fn cases() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        ("time.format", json!({"epoch":0,"format":"%H:%M"}), "zone"),
        ("text.pad", json!({"text":"x","width":3}), "side"),
        ("text.pad", json!({"text":"x","width":3}), "fill"),
        ("text.pad", json!({"text":"x","width":3}), "display_width"),
        (
            "text.truncate",
            json!({"text":"abcd","width":2}),
            "ellipsis",
        ),
        ("number.format_size", json!({"value":1000}), "system"),
        ("number.format", json!({"value":1000}), "grouping"),
        ("number.format", json!({"value":1000}), "percent"),
        ("list.count", json!({"items":[2,1]}), "key"),
        ("list.count", json!({"items":[2,1]}), "collect"),
        ("list.sum", json!({"items":[2,1]}), "key"),
        ("list.sum", json!({"items":[2,1]}), "collect"),
        ("list.min", json!({"items":[2,1]}), "key"),
        ("list.min", json!({"items":[2,1]}), "collect"),
        ("list.max", json!({"items":[2,1]}), "key"),
        ("list.max", json!({"items":[2,1]}), "collect"),
        ("list.unique", json!({"items":[2,1]}), "key"),
        ("list.unique", json!({"items":[2,1]}), "collect"),
        ("list.sort", json!({"items":[2,1]}), "key"),
        ("list.sort", json!({"items":[2,1]}), "collect"),
        ("list.top", json!({"items":[2,1],"n":1}), "collect"),
        ("list.top", json!({"items":[2,1],"n":1}), "key"),
        ("list.group", json!({"items":[1,1],"key":""}), "collect"),
        ("json.sort_by", json!({"document":["b","a"]}), "numeric"),
        ("encode.base64", json!({"text":"a"}), "url_safe"),
        ("encode.base64", json!({"text":"a"}), "padding"),
        ("decode.base64", json!({"text":"YQ=="}), "url_safe"),
        ("decode.base64", json!({"text":"YQ=="}), "padding"),
        ("encode.url", json!({"text":"a b"}), "form"),
        ("decode.url", json!({"text":"a+b"}), "form"),
        (
            "wait.until",
            json!({"probe":"file_exists","path":".","timeout_ms":0}),
            "interval_ms",
        ),
    ]
}

#[test]
fn supplied_options_with_wrong_types_are_invalid_requests() {
    for (name, mut args, field) in cases() {
        assert!(
            call(name, &args).is_ok(),
            "omitted {name}.{field} must retain its default"
        );
        let wrong_scalar = if ["zone", "side", "fill", "ellipsis", "system", "key"].contains(&field)
        {
            json!(true)
        } else {
            json!("bad")
        };
        for wrong in [Value::Null, json!([]), json!({}), wrong_scalar] {
            args[field] = wrong.clone();
            let error = call(name, &args).expect_err(&format!("{name}.{field} accepted {wrong}"));
            assert_eq!(error.code, "invalid_request", "{name}.{field}");
            assert_eq!(error.kind, AppletErrorKind::Permanent, "{name}.{field}");
            assert!(
                error.message.contains(field),
                "{name}.{field}: {}",
                error.message
            );
        }
    }
}

#[test]
fn independent_applet_cli_rejects_verified_wrong_types() {
    for (name, args) in [
        (
            "time.format",
            json!({"epoch":0,"format":"%H:%M","zone":19800}),
        ),
        ("text.pad", json!({"text":"x","width":3,"side":true})),
        ("number.format_size", json!({"value":1000,"system":true})),
    ] {
        let error = call(name, &args).expect_err("direct API must reject a wrong option type");
        assert_eq!(error.code, "invalid_request");
        assert_eq!(error.kind, AppletErrorKind::Permanent);
        let output = Command::new(env!("CARGO_BIN_EXE_execute-applet"))
            .args([name, "--args", &args.to_string()])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{name}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid_request"));
    }
}

#[test]
fn sleep_checks_supplied_types_without_changing_value_precedence() {
    for args in [
        json!({"milliseconds":"bad","seconds":0}),
        json!({"milliseconds":0,"seconds":"bad"}),
        json!({"milliseconds":0,"seconds":null}),
        json!({"milliseconds":0.0,"seconds":0}),
    ] {
        let error = call("time.sleep", &args).unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert_eq!(error.kind, AppletErrorKind::Permanent);
    }
    for args in [
        json!({"milliseconds":0,"seconds":-1}),
        json!({"milliseconds":-1,"seconds":0}),
        json!({"milliseconds":0}),
        json!({"seconds":0}),
    ] {
        assert_eq!(call("time.sleep", &args).unwrap(), json!(true), "{args}");
    }
}

#[test]
fn omission_and_explicit_defaults_match_and_valid_values_remain_supported() {
    for (name, mut args, field) in cases() {
        let omitted = call(name, &args).unwrap();
        let default = match field {
            "zone" => json!("utc"),
            "side" => json!("right"),
            "fill" => json!(" "),
            "ellipsis" => json!("…"),
            "system" => json!("iec"),
            "key" => json!(""),
            "padding" => json!(true),
            "interval_ms" => json!(50),
            _ => json!(false),
        };
        args[field] = default;
        let explicit = call(name, &args).unwrap();
        if name == "wait.until" {
            assert_eq!(omitted["ready"], explicit["ready"], "{name}.{field}");
        } else {
            assert_eq!(omitted, explicit, "{name}.{field}");
        }
    }
    for (name, args, expected) in [
        (
            "time.format",
            json!({"epoch":0,"format":"%H:%M","zone":"19800"}),
            json!("05:30"),
        ),
        (
            "text.pad",
            json!({"text":"x","width":3,"side":"left","fill":"."}),
            json!("..x"),
        ),
        (
            "text.truncate",
            json!({"text":"abcd","width":2,"ellipsis":"!"}),
            json!("a!"),
        ),
        (
            "number.format_size",
            json!({"value":1000,"system":"si"}),
            json!("1.0 kB"),
        ),
        (
            "number.format",
            json!({"value":1000,"grouping":true,"percent":true}),
            json!("1,000%"),
        ),
        (
            "list.top",
            json!({"items":[{"v":1},{"v":3}],"n":1,"key":"/v"}),
            json!([{"v":3}]),
        ),
        (
            "json.sort_by",
            json!({"document":[2,1],"numeric":true}),
            json!([1, 2]),
        ),
        (
            "encode.base64",
            json!({"text":"a","padding":false}),
            json!("YQ"),
        ),
        (
            "decode.base64",
            json!({"text":"YQ","padding":false}),
            json!("a"),
        ),
        (
            "encode.url",
            json!({"text":"a b","form":true}),
            json!("a+b"),
        ),
        (
            "decode.url",
            json!({"text":"a+b","form":true}),
            json!("a b"),
        ),
    ] {
        assert_eq!(call(name, &args).unwrap(), expected, "{name} {args}");
    }
}

#[test]
fn wait_interval_rejects_out_of_range_integers() {
    for interval in [-1, 0, 10_001] {
        let args = json!({"probe":"file_exists","path":".","timeout_ms":0,"interval_ms":interval});
        assert_eq!(
            call("wait.until", &args).unwrap_err().code,
            "invalid_request"
        );
    }
    for interval in [1, 10_000] {
        let args = json!({"probe":"file_exists","path":".","timeout_ms":0,"interval_ms":interval});
        assert_eq!(call("wait.until", &args).unwrap()["ready"], true);
    }
}
