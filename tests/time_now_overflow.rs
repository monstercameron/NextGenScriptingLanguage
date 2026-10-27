use flow::applets::{self, AppletErrorKind};
use serde_json::{json, Value};
use std::{fs, process::Command};

#[test]
fn whole_second_boundaries_return_exact_records_or_typed_errors() {
    for seconds in [-9_223_372_036_i64, 0, 9_223_372_036] {
        let args = json!({"epoch_s": seconds});
        let value = applets::execute("time.now", args.as_object().unwrap(), b"").unwrap();
        assert_eq!(value["epoch_s"], seconds);
        assert_eq!(value["epoch_ms"].as_i64().unwrap(), seconds * 1000);
        assert_eq!(value["epoch_ns"].as_i64().unwrap(), seconds * 1_000_000_000);
        let parsed = chrono::DateTime::parse_from_rfc3339(value["iso"].as_str().unwrap()).unwrap();
        assert_eq!(parsed.timestamp(), seconds);
    }
    for seconds in [-9_223_372_037_i64, 9_223_372_037, 10_000_000_000] {
        let args = json!({"epoch_s": seconds});
        let err = applets::execute("time.now", args.as_object().unwrap(), b"").unwrap_err();
        assert_eq!(err.kind, AppletErrorKind::Permanent);
        assert_eq!(err.code, "invalid_request");
        assert!(err.message.contains("nanoseconds"));
    }
}

#[test]
fn current_clock_fields_represent_the_same_instant() {
    // Use a fresh process with the clock override removed to avoid process-global env races.
    let output = Command::new(env!("CARGO_BIN_EXE_execute-applet"))
        .args(["time.now", "--args", "{}"])
        .env_remove("EXECUTE_NOW")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let dt = chrono::DateTime::parse_from_rfc3339(value["iso"].as_str().unwrap()).unwrap();
    let ns = value["epoch_ns"].as_i64().unwrap();
    assert_eq!(value["epoch_s"].as_i64().unwrap(), dt.timestamp());
    assert_eq!(
        ns,
        dt.timestamp() * 1_000_000_000 + i64::from(dt.timestamp_subsec_nanos())
    );
    assert_eq!(
        value["epoch_ms"].as_i64().unwrap(),
        ns.div_euclid(1_000_000)
    );
}

#[test]
fn helper_runtime_and_record_agree_on_success_and_overflow() {
    let root = tempfile::tempdir().unwrap();
    for (i, seconds) in [
        -9_223_372_036_i64,
        9_223_372_036,
        -9_223_372_037,
        9_223_372_037,
    ]
    .into_iter()
    .enumerate()
    {
        let args = json!({"epoch_s":seconds}).to_string();
        let helper = Command::new(env!("CARGO_BIN_EXE_execute-applet"))
            .args(["time.now", "--args", &args])
            .output()
            .unwrap();
        let program = root.path().join(format!("{i}.program"));
        fs::write(&program, format!("run time.now with epoch_s {seconds}\n")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command.arg("run").arg(&program);
        #[cfg(feature = "records")]
        let record = root.path().join(format!("record-{i}"));
        #[cfg(feature = "records")]
        command.arg("--record").arg(&record);
        let run = command
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
            .output()
            .unwrap();
        #[cfg(feature = "records")]
        let summary: Value =
            serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
        if helper.status.success() {
            assert!(
                run.status.success(),
                "{}",
                String::from_utf8_lossy(&run.stderr)
            );
            let expected: Value = serde_json::from_slice(&helper.stdout).unwrap();
            let actual: Value = serde_json::from_slice(&run.stdout).unwrap();
            assert_eq!(actual, expected);
            #[cfg(feature = "records")]
            {
                assert_eq!(summary["result"]["ok"], true);
                let trace = fs::read_to_string(record.join("trace.jsonl")).unwrap();
                let step: Value = serde_json::from_str(trace.lines().next().unwrap()).unwrap();
                let blob = step["stdout"]["blob"].as_str().unwrap();
                let recorded: Value =
                    serde_json::from_slice(&fs::read(record.join(blob)).unwrap()).unwrap();
                assert_eq!(recorded, expected);
            }
        } else {
            assert_eq!(helper.status.code(), Some(2));
            assert!(String::from_utf8_lossy(&helper.stderr).contains("invalid_request"));
            assert!(!run.status.success());
            assert!(String::from_utf8_lossy(&run.stderr).contains("invalid_request"));
            #[cfg(feature = "records")]
            assert_eq!(summary["result"]["ok"], false);
        }
    }
}
