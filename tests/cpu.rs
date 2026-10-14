#![cfg(unix)]
//! Ranking and scheduling belong to the .program; only ps is replaced in fixtures.
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;
const SOURCE: &str = include_str!("../examples/top-cpu.program");
fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("ps"), include_str!("fixtures/ps.py")).unwrap();
    fs::set_permissions(dir.path().join("ps"), fs::Permissions::from_mode(0o755)).unwrap();
    dir
}
fn invoke(dir: &Path, source: &str, mode: &str, args: &[&str]) -> Output {
    let file = dir.join("cpu.program");
    fs::write(&file, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .arg(file)
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .current_dir(dir)
        .env(
            "PATH",
            format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
        )
        .env("CPU_FIXTURE_DIR", dir)
        .env("CPU_FIXTURE_MODE", mode)
        .output()
        .unwrap()
}
#[test]
fn real_jq_selects_ten_numeric_cpu_values_and_routes_identical_records() {
    let dir = fixture();
    let result = invoke(dir.path(), SOURCE, "normal", &["--headless"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let data: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let bars = data["programs"]["monitor"]["widgets"]["usage"]["bars"]
        .as_array()
        .unwrap();
    let records = data["programs"]["details"]["widgets"]["records"]["rows"]
        .as_array()
        .unwrap();
    assert_eq!(bars.len(), 10);
    assert_eq!(records.len(), 10);
    assert_eq!(data["completed_jobs"], 1);
    let pids = records
        .iter()
        .map(|v| v["pid"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(pids, vec![55, 11, 99, 66, 77, 22, 88, 33, 12, 13]);
    for (bar, row) in bars.iter().zip(records) {
        assert_eq!(bar["value"], row["cpu"]);
        assert_eq!(bar["label"], row["label"]);
    }
    assert_eq!(records[0]["cpu"], 125.5);
    assert_eq!(records[2]["cpu"], 71.2);
    assert_eq!(records[5]["name"], "editor");
    assert_eq!(records[3]["name"], "quote\"name");
    let frame = data["frame"].as_str().unwrap();
    assert!(frame.contains("125.5%"));
    assert!(frame.contains('█'));
    assert!(frame.contains("55 multicore"));
    assert_eq!(
        fs::read_to_string(dir.path().join("ps-calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}
#[test]
fn top_n_and_initial_load_are_changed_by_program_text_alone() {
    let dir = fixture();
    let source = SOURCE.replace("with limit 10", "with limit 3");
    let output = invoke(dir.path(), &source, "normal", &["--headless"]);
    assert!(output.status.success());
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        data["programs"]["monitor"]["widgets"]["usage"]["bars"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let source = SOURCE.replace("  whenever the program opens\n    press refresh\n\n", "");
    let output = invoke(dir.path(), &source, "normal", &["--headless"]);
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("ps-calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}
#[test]
fn large_ps_output_is_drained_and_empty_output_is_valid() {
    let dir = fixture();
    for (mode, count) in [("large", 10), ("empty", 0)] {
        let result = invoke(dir.path(), SOURCE, mode, &["--headless"]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let data: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(
            data["programs"]["monitor"]["widgets"]["usage"]["bars"]
                .as_array()
                .unwrap()
                .len(),
            count
        );
    }
}
#[test]
fn failed_or_malformed_ps_output_does_not_display_fabricated_values() {
    let dir = fixture();
    for mode in ["fail", "malformed"] {
        let output = invoke(dir.path(), SOURCE, mode, &["--headless"]);
        assert!(!output.status.success(), "{mode}");
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if mode == "fail" {
                "fixture ps failure"
            } else {
                "Unexpected ps row"
            }),
            "{error}"
        );
    }
}
#[test]
fn invalid_bar_declarations_fail_before_any_process_query() {
    let dir = fixture();
    for source in [
        SOURCE.replace("    size each bar from \"cpu\"\n", ""),
        SOURCE.replace("bars in usage", "bars in status"),
        SOURCE.replace("label each bar from", "size each bar from"),
    ] {
        assert!(!invoke(dir.path(), &source, "normal", &["--headless"])
            .status
            .success());
    }
    assert!(!dir.path().join("ps-calls.jsonl").exists());
}
#[test]
fn cpu_terminal_refresh_timer_and_quit() {
    let output = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cpu_pty.py"))
        .arg(env!("CARGO_BIN_EXE_execute"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn installed_ps_and_jq_produce_a_valid_real_cpu_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("live.program");
    fs::write(&script, SOURCE).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(script)
        .arg("--headless")
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let records = data["programs"]["details"]["widgets"]["records"]["rows"]
        .as_array()
        .unwrap();
    assert!(!records.is_empty());
    assert!(records.len() <= 10);
    assert!(records
        .windows(2)
        .all(|pair| pair[0]["cpu"].as_f64().unwrap() >= pair[1]["cpu"].as_f64().unwrap()));
}
