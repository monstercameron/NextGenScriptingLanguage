//! `execute explain FILE --json` through the real binary. The plan is derived
//! from parsed steps and resolved manifests; nothing runs.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const HELPER: &str = env!("CARGO_BIN_EXE_execute-model");

fn write_manifests(directory: &Path) -> PathBuf {
    let manifests = directory.join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    // No contract: effects must stay undeclared (null) in the plan.
    let plain = serde_json::json!({
        "name": "plain.helper",
        "binary": HELPER,
        "accepts_stdin": false,
        "parameters": { "text": { "type": "string" } },
        "arguments": [{ "literal": "--version" }, { "when_present": "text", "append": [{ "literal": "--text" }, { "parameter": "text" }] }]
    });
    std::fs::write(manifests.join("plain.helper.json"), plain.to_string()).unwrap();
    // A contract that declares a loopback network effect and a write effect.
    let declared = serde_json::json!({
        "name": "net.helper",
        "binary": HELPER,
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [{ "literal": "--version" }],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": { "kind": "oneshot" },
            "capabilities": ["network.loopback", "fs.write.workspace"],
            "effects": ["network.loopback", "fs.write.workspace"]
        }
    });
    std::fs::write(manifests.join("net.helper.json"), declared.to_string()).unwrap();
    manifests
}

fn explain(directory: &Path, program: &str, extra: &[&str]) -> Output {
    let manifests = write_manifests(directory);
    std::fs::write(directory.join("plan.program"), program).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .arg("explain")
        .arg("plan.program")
        .args(extra)
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.output().expect("start execute")
}

fn plan(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("--json prints one object")
}

#[test]
fn json_plan_has_one_row_per_invocation_with_resolved_argv_and_declared_effects() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "run net.helper\nrun plain.helper with text \"hello\"\n",
        &["--json"],
    );
    let object = plan(&output);
    assert_eq!(object["protocol"], "execute.explain/1");
    let steps = object["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2, "{steps:?}");

    let declared = &steps[0];
    assert_eq!(declared["applet"], "net.helper");
    assert_eq!(declared["argv"], serde_json::json!(["--version"]));
    assert_eq!(
        declared["effects"],
        serde_json::json!(["network.loopback", "fs.write.workspace"])
    );
    assert_eq!(declared["network"], true);
    assert_eq!(declared["writes"], true);

    let plain = &steps[1];
    assert_eq!(plain["applet"], "plain.helper");
    assert_eq!(
        plain["argv"],
        serde_json::json!(["--version", "--text", "hello"])
    );
    assert!(
        plain["effects"].is_null(),
        "undeclared effects must be null"
    );
    assert!(plain["network"].is_null());
    assert!(plain["writes"].is_null());
}

#[test]
fn argv_is_withheld_when_a_parameter_is_a_runtime_value() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "accept message as text\nrun plain.helper with text message\n",
        &["--json"],
    );
    let object = plan(&output);
    let steps = object["steps"].as_array().unwrap();
    let invocation = steps
        .iter()
        .find(|row| row["applet"] == "plain.helper")
        .expect("plain.helper row");
    assert!(
        invocation["argv"].is_null(),
        "argv must not be guessed: {invocation}"
    );
}

#[test]
fn summary_lists_executables_writes_network_and_undeclared_count() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "run net.helper\nrun plain.helper\n",
        &["--json"],
    );
    let summary = &plan(&output)["summary"];
    assert_eq!(summary["invocations"], 2);
    assert_eq!(summary["network"], true);
    assert_eq!(summary["writes"], true);
    assert_eq!(summary["effects_declared"], false);
    assert_eq!(summary["undeclared_invocations"], 1);
    let executables = summary["executables"].as_array().unwrap();
    assert_eq!(
        executables.len(),
        1,
        "both manifests share one helper binary: {executables:?}"
    );
    assert_eq!(summary["privileged"], "not_classified_see_policy_marking");
}

#[test]
fn save_steps_count_as_writes_and_other_steps_have_no_effects() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "accept message as text\nrun plain.helper\nthen save to \"out.txt\"\n",
        &["--json"],
    );
    let object = plan(&output);
    let steps = object["steps"].as_array().unwrap();
    assert!(steps
        .iter()
        .any(|row| row["kind"] == "save" && row["writes"] == true));
    // `accept` declares an input rather than a step, so this program has no
    // "other" row. Any "other" row that does appear must carry no effects.
    assert!(steps
        .iter()
        .all(|row| row["kind"] != "other" || row["effects"].is_null()));
    assert_eq!(object["summary"]["save_steps"], true);
}

#[test]
fn text_explain_is_unchanged_without_the_flag() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(directory.path(), "run plain.helper\n", &[]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        !text.trim_start().starts_with('{'),
        "text mode printed JSON: {text}"
    );
}

#[test]
fn json_is_rejected_for_run_and_the_check_json_contract_is_intact() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path());
    std::fs::write(directory.path().join("ok.program"), "run plain.helper\n").unwrap();
    let run_json = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg("ok.program")
        .arg("--json")
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!run_json.status.success(), "run must not accept --json");

    let check = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("check")
        .arg("ok.program")
        .arg("--json")
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let object: Value = serde_json::from_slice(&check.stdout).expect("check --json object");
    assert_eq!(object["protocol"], "execute.check/1");
}
