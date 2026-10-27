use flow::applets::{self, AppletErrorKind};
use serde_json::{json, Value};
use std::{fs, process::Command};

fn count(args: Value, input: &[u8]) -> Result<Value, applets::AppletError> {
    applets::execute("text.count_lines_by", args.as_object().unwrap(), input)
}

#[test]
fn explicit_field_is_validated_and_absent_field_defaults_to_zero() {
    let input = b"a,1\nb,2\na,2\n";
    let expected = json!([{"key":"a","count":2},{"key":"b","count":1}]);
    assert_eq!(count(json!({"delimiter":","}), input).unwrap(), expected);
    assert_eq!(
        count(json!({"delimiter":",", "field":0}), input).unwrap(),
        expected
    );
    assert_eq!(
        count(json!({"delimiter":",", "field":1}), input).unwrap(),
        json!([{"key":"1","count":1},{"key":"2","count":2}])
    );
    for field in [
        json!(-1),
        json!(1.5),
        json!(1.0),
        json!("1"),
        Value::Null,
        json!(true),
        json!(u64::MAX),
        json!(16 * 1024 * 1024 + 1),
    ] {
        let error = count(json!({"delimiter":",", "field":field}), input).unwrap_err();
        assert_eq!(error.code, "invalid_request", "field={field}");
        assert_eq!(error.kind, AppletErrorKind::Permanent);
        assert!(error.message.contains("field"), "{}", error.message);
    }
    for field in [json!(-1), json!(u64::MAX), json!(16 * 1024 * 1024 + 1)] {
        assert_eq!(
            count(json!({"delimiter":",", "field":field}), b"")
                .unwrap_err()
                .code,
            "invalid_request"
        );
    }
    // The largest possible index in bounded input is still a valid index argument.
    assert_eq!(
        count(json!({"delimiter":",", "field":16 * 1024 * 1024}), b"").unwrap(),
        json!([])
    );
    let error = count(json!({"delimiter":",", "field":2}), input).unwrap_err();
    assert!(error.message.contains("line has no field 2"));
}

#[test]
fn pipeline_rejects_negative_index_and_honors_default() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.txt"), b"a,1\nb,2\na,2\n").unwrap();
    let program = dir.path().join("field.program");
    let run = |field: &str| {
        fs::write(
            &program,
            format!("read \"input.txt\"\nthen text.count_lines_by with delimiter \",\"{field}\n"),
        )
        .unwrap();
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(dir.path())
            .arg(&program)
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
            .output()
            .unwrap()
    };
    let invalid = run(" and field -1");
    assert!(!invalid.status.success());
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(
        stderr.contains("invalid_request") && stderr.contains("field"),
        "{stderr}"
    );
    let oversized = run(" and field 16777217");
    let stderr = String::from_utf8_lossy(&oversized.stderr);
    assert!(!oversized.status.success());
    assert!(
        stderr.contains("invalid_request") && stderr.contains("field"),
        "{stderr}"
    );
    let second = run(" and field 1");
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&second.stdout).unwrap(),
        json!([{"key":"1","count":1},{"key":"2","count":2}])
    );
    let default = run("");
    assert!(
        default.status.success(),
        "{}",
        String::from_utf8_lossy(&default.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&default.stdout).unwrap(),
        json!([{"key":"a","count":2},{"key":"b","count":1}])
    );
}
