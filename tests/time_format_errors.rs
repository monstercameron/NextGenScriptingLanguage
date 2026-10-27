use flow::applets::{self, AppletErrorKind};
use serde_json::json;
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn malformed_formats_are_typed_errors_and_valid_formats_still_work() {
    for format in ["%Q", "%", "%:", "before %Q after", "%#z"] {
        let args = json!({"epoch":0,"format":format});
        let err = applets::execute("time.format", args.as_object().unwrap(), b"").unwrap_err();
        assert_eq!(err.code, "invalid_request", "{format}");
        assert_eq!(err.kind, AppletErrorKind::Permanent, "{format}");
        assert!(err.message.contains("format"));
    }
    for (format, expected) in [
        ("%Y-%m-%d %H:%M:%S %z", "1970-01-01 00:00:00 +0000"),
        ("%%", "%"),
        ("", ""),
    ] {
        let args = json!({"epoch":0,"format":format});
        assert_eq!(
            applets::execute("time.format", args.as_object().unwrap(), b"").unwrap(),
            expected
        );
    }
}

#[test]
fn helper_and_runtime_return_errors_without_crashing() {
    for format in ["%Q", "%"] {
        let args = json!({"epoch":0,"format":format}).to_string();
        let helper = Command::new(env!("CARGO_BIN_EXE_execute-applet"))
            .args(["time.format", "--args", &args])
            .output()
            .unwrap();
        assert_eq!(helper.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&helper.stderr).contains("invalid_request"));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("format.program");
        fs::write(
            &file,
            format!("run time.format with epoch 0 and format \"{format}\"\n"),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(&file)
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("invalid_request"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn worker_survives_invalid_format_and_accepts_the_next_request() {
    let mut worker = Command::new(env!("CARGO_BIN_EXE_execute-applet"))
        .arg("--worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = worker.stdin.take().unwrap();
    for (id, format) in [(1, "%Q"), (2, "%Y")] {
        let args = json!({"epoch":0,"format":format}).to_string();
        writeln!(
            stdin,
            "{}",
            json!({"id":id,"arguments":["time.format","--args",args]})
        )
        .unwrap();
    }
    drop(stdin);
    let result = worker.wait_with_output().unwrap();
    assert!(result.status.success());
    let output = String::from_utf8(result.stdout).unwrap();
    let responses: Vec<serde_json::Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["status"], 2);
    assert!(responses[0]["stderr"]
        .as_str()
        .unwrap()
        .contains("invalid_request"));
    assert_eq!(responses[1]["status"], 0);
    assert_eq!(responses[1]["output"], json!(b"\"1970\"\n"));
}
