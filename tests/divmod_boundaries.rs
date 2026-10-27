use flow::applets::{self, AppletErrorKind};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

fn divmod(left: Value, right: Value) -> Result<Value, applets::AppletError> {
    let args = json!({"left": left, "right": right});
    applets::execute("number.divmod", args.as_object().unwrap(), &[])
}

#[test]
fn integer_boundaries_have_fallible_results() {
    for (left, right, expected) in [
        (json!(7), json!(3), json!({"quotient":2,"remainder":1})),
        (json!(-7), json!(3), json!({"quotient":-2,"remainder":-1})),
        (json!(7), json!(-3), json!({"quotient":-2,"remainder":1})),
        (
            json!(u64::MAX),
            json!(1),
            json!({"quotient":u64::MAX,"remainder":0}),
        ),
        (
            json!(i64::MIN),
            json!(-1),
            json!({"quotient":9223372036854775808u64,"remainder":0}),
        ),
        (
            json!(9223372036854775808u64),
            json!(-1),
            json!({"quotient":i64::MIN,"remainder":0}),
        ),
        (
            json!(u64::MAX),
            json!(-2),
            json!({"quotient":-9223372036854775807i64,"remainder":1}),
        ),
        (
            json!(i64::MIN),
            json!(u64::MAX),
            json!({"quotient":0,"remainder":i64::MIN}),
        ),
    ] {
        assert_eq!(divmod(left, right).unwrap(), expected);
    }
    for left in [9223372036854775809u64, u64::MAX] {
        let error = divmod(json!(left), json!(-1)).unwrap_err();
        assert_eq!(error.code, "overflow");
        assert_eq!(error.kind, AppletErrorKind::Permanent);
    }
    let error = divmod(json!(1), json!(0)).unwrap_err();
    assert_eq!(error.code, "invalid_request");
    assert_eq!(error.kind, AppletErrorKind::Permanent);
}

#[test]
fn helper_and_worker_report_overflow_and_remain_usable() {
    let helper = env!("CARGO_BIN_EXE_execute-applet");
    let bad = json!({"left":u64::MAX,"right":-1}).to_string();
    let good = json!({"left":u64::MAX,"right":1}).to_string();
    let output = Command::new(helper)
        .args(["number.divmod", "--args", &bad])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("overflow"));

    let mut worker = Command::new(helper)
        .arg("--worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = worker.stdin.take().unwrap();
    for (id, args) in [(1, bad), (2, good)] {
        writeln!(
            stdin,
            "{}",
            json!({"id":id,"arguments":["number.divmod","--args",args]})
        )
        .unwrap();
    }
    drop(stdin);
    let output = worker.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses: Vec<Value> = output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["status"], 2);
    assert!(responses[0]["stderr"]
        .as_str()
        .unwrap()
        .contains("overflow"));
    assert_eq!(responses[1]["status"], 0);
    let bytes: Vec<u8> = serde_json::from_value(responses[1]["output"].clone()).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"quotient":u64::MAX,"remainder":0})
    );
}

#[test]
fn runtime_reports_overflow_without_process_panic() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("input.json"),
        format!("{{\"n\":{}}}", u64::MAX),
    )
    .unwrap();
    let source = "read \"input.json\"\nthen parse json as doc\ntake json at \"/n\" from doc as n\nrun number.divmod with left n and right -1\n";
    let program = dir.path().join("divmod.program");
    fs::write(&program, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .arg(&program)
        .arg("--manifest-dir")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_ne!(output.status.code(), Some(101));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("overflow"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");

    fs::write(
        &program,
        source.replace("right -1", "right 1 as result\nthen encode result as json"),
    )
    .unwrap();
    let success = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .arg(&program)
        .arg("--manifest-dir")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
        .output()
        .unwrap();
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&success.stdout).unwrap(),
        json!({"quotient":u64::MAX,"remainder":0})
    );
}
