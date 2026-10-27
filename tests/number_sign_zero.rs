use flow::applets;
use serde_json::{json, Value};
use std::{fs, process::Command};

#[test]
fn sign_is_zero_for_all_zero_representations_and_correct_for_nonzero_values() {
    for (value, expected) in [
        (json!(0), 0.0),
        (json!(0.0), 0.0),
        (json!(-0.0), 0.0),
        (json!(42), 1.0),
        (json!(-42), -1.0),
        (json!(0.001), 1.0),
        (json!(-0.001), -1.0),
        (json!(u64::MAX), 1.0),
    ] {
        let args = json!({"value":value});
        let result = applets::execute("number.sign", args.as_object().unwrap(), b"").unwrap();
        assert_eq!(result.as_f64().unwrap(), expected, "{value}");
        if value.as_f64() == Some(0.0) {
            assert!(!result.as_f64().unwrap().is_sign_negative());
        }
    }
}

#[test]
fn signed_zero_has_zero_sign_in_helper_and_workflow() {
    for source in ["0", "0.0", "-0.0"] {
        let args = format!("{{\"value\":{source}}}");
        let helper = Command::new(env!("CARGO_BIN_EXE_execute-applet"))
            .args(["number.sign", "--args", &args])
            .output()
            .unwrap();
        assert!(
            helper.status.success(),
            "{}",
            String::from_utf8_lossy(&helper.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&helper.stdout)
                .unwrap()
                .as_f64(),
            Some(0.0)
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sign.program");
        fs::write(&path, format!("run number.sign with value {source}\n")).unwrap();
        for action in ["check", "run"] {
            let out = Command::new(env!("CARGO_BIN_EXE_execute"))
                .arg(action)
                .arg(&path)
                .arg("--manifest-dir")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{source}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            if action == "run" {
                assert_eq!(
                    serde_json::from_slice::<Value>(&out.stdout)
                        .unwrap()
                        .as_f64(),
                    Some(0.0)
                );
            }
        }
    }
}
