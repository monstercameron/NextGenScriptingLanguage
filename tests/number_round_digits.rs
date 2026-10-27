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
