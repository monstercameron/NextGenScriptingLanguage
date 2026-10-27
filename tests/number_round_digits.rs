use flow::applets::{self, AppletErrorKind};
use serde_json::{json, Value};
use std::{fs, process::Command};

fn round(value: Value, digits: Option<Value>) -> Result<Value, flow::applets::AppletError> {
    let mut args = serde_json::Map::new();
    args.insert("value".into(), value);
    if let Some(digits) = digits {
        args.insert("digits".into(), digits);
    }
    applets::execute("number.round", &args, b"")
}

#[test]
fn explicit_digits_require_a_signed_integer_and_branch_range() {
    for value in [json!(12), json!(12.5)] {
        for digits in [
            json!(u64::MAX),
            json!(i64::MAX as u64 + 1),
            json!(null),
            json!(true),
            json!("1"),
            json!(1.0),
        ] {
            let error = round(value.clone(), Some(digits.clone())).unwrap_err();
            assert_eq!(error.kind, AppletErrorKind::Permanent, "{value}, {digits}");
            assert_eq!(error.code, "invalid_request");
            assert!(error.message.contains("digits"));
        }
    }
    for digits in [-39, 39] {
        assert_eq!(
            round(json!(12), Some(json!(digits))).unwrap_err().code,
            "invalid_request"
        );
    }
    for digits in [-309, 309] {
        assert_eq!(
            round(json!(12.5), Some(json!(digits))).unwrap_err().code,
            "invalid_request"
        );
    }
    assert_eq!(round(json!(12), None).unwrap(), json!(12));
    assert_eq!(round(json!(12.5), None).unwrap(), json!(13.0));
    assert_eq!(round(json!(125), Some(json!(-1))).unwrap(), json!(130));
    assert_eq!(round(json!(12.5), Some(json!(1))).unwrap(), json!(12.5));
    for digits in [-38, 38] {
        assert!(round(json!(0), Some(json!(digits))).is_ok());
    }
    for digits in [-308, 308] {
        assert!(round(json!(0.0), Some(json!(digits))).is_ok());
    }
}

#[test]
fn helper_reports_bad_digits_and_workflow_preserves_valid_rounding() {
    for value in [json!(12), json!(12.5)] {
        let args = json!({"value":value,"digits":u64::MAX}).to_string();
        let output = Command::new(env!("CARGO_BIN_EXE_execute-applet"))
            .args(["number.round", "--args", &args])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid_request"));
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("round.program");
    fs::write(&file, "run number.round with value 125 and digits -1\n").unwrap();
    for action in ["check", "run"] {
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(action)
            .arg(&file)
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if action == "run" {
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                json!(130)
            );
        }
    }
}

#[test]
fn static_rounding_digit_bounds_follow_json_number_representation() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("round-bounds.program");
    let invoke = |source: &str, action: &str, extra: &[&str]| {
        fs::write(&file, source).unwrap();
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(action)
            .arg(&file)
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
            .args(extra)
            .output()
            .unwrap()
    };

    for source in [
        "run number.round with value 1 and digits -38\n",
        "run number.round with value 1 and digits 38\n",
        "run number.round with value 1.5 and digits -308\n",
        "run number.round with value 1.5 and digits 308\n",
        // Float values retain the wider domain; integer-only limits must not
        // accidentally narrow valid floating-point calls.
        "run number.round with value 1.5 and digits 39\n",
        "run number.round with value 1.5 and digits -39\n",
    ] {
        let checked = invoke(source, "check", &[]);
        assert!(
            checked.status.success(),
            "static checker rejected {source:?}: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
    }

    for source in [
        "run number.round with value 1 and digits 39\n",
        "run number.round with value 1 and digits -39\n",
        "run number.round with value 1.5 and digits 309\n",
        "run number.round with value 1.5 and digits -309\n",
    ] {
        let checked = invoke(source, "check", &[]);
        assert!(
            !checked.status.success()
                && String::from_utf8_lossy(&checked.stderr)
                    .contains("integer values or -308 and 308"),
            "static checker accepted {source:?}: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
    }

    // A reference may resolve to either representation, so checking defers
    // the conditional constraint and execution validates the resolved pair.
    let dynamic_value = concat!(
        "run json.parse with text \"1\" as amount\n",
        "run number.round with value amount and digits 39\n"
    );
    assert!(invoke(dynamic_value, "check", &[]).status.success());
    let dynamic_run = invoke(dynamic_value, "run", &[]);
    assert!(!dynamic_run.status.success());
    assert!(
        String::from_utf8_lossy(&dynamic_run.stderr).contains("digits must be between -38 and 38")
    );

    let dynamic_float = concat!(
        "run json.parse with text \"1.5\" as amount\n",
        "run number.round with value amount and digits 39\n"
    );
    assert!(invoke(dynamic_float, "check", &[]).status.success());
    assert!(invoke(dynamic_float, "run", &[]).status.success());

    // Likewise, a dynamic digits input remains statically possible but is
    // checked against the known floating-point representation at execution.
    let dynamic_digits = concat!(
        "accept digits as integer\n",
        "run number.round with value 1.5 and digits digits\n"
    );
    assert!(invoke(dynamic_digits, "check", &[]).status.success());
    let dynamic_run = invoke(dynamic_digits, "run", &["--input", "digits=309"]);
    assert!(!dynamic_run.status.success());
    assert!(
        String::from_utf8_lossy(&dynamic_run.stderr).contains("digits must be between -38 and 38")
    );
}
