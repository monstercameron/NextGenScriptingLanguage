//! `execute explain FILE --json --allow CATEGORY...`: the effects approval gate.
//! It denies by default, and exits nonzero unless every violation is approved.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const HELPER: &str = env!("CARGO_BIN_EXE_execute-model");

fn write_manifests(directory: &Path) -> PathBuf {
    let manifests = directory.join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    let plain = serde_json::json!({
        "name": "plain.helper", "binary": HELPER, "accepts_stdin": false,
        "parameters": {}, "arguments": [{ "literal": "--version" }]
    });
    std::fs::write(manifests.join("plain.helper.json"), plain.to_string()).unwrap();
    let declared = serde_json::json!({
        "name": "net.helper", "binary": HELPER, "accepts_stdin": false,
        "parameters": {}, "arguments": [{ "literal": "--version" }],
        "contract": {
            "version": 1, "backend": "command", "lifecycle": { "kind": "oneshot" },
            "capabilities": ["network.loopback"], "effects": ["network.loopback"]
        }
    });
    std::fs::write(manifests.join("net.helper.json"), declared.to_string()).unwrap();
    // One applet per declared effect shape, so each mapping is tested directly.
    for (name, effect) in [
        ("read.helper", "fs.read.workspace"),
        ("write.helper", "fs.write.workspace"),
        ("spawn.helper", "process.spawn"),
        ("mystery.helper", "mystery.effect"),
    ] {
        let manifest = serde_json::json!({
            "name": name, "binary": HELPER, "accepts_stdin": false,
            "parameters": {}, "arguments": [{ "literal": "--version" }],
            "contract": {
                "version": 1, "backend": "command", "lifecycle": { "kind": "oneshot" },
                "capabilities": [effect], "effects": [effect]
            }
        });
        std::fs::write(manifests.join(format!("{name}.json")), manifest.to_string()).unwrap();
    }
    manifests
}

fn gate(directory: &Path, source: &str, extra: &[&str]) -> Output {
    let manifests = write_manifests(directory);
    std::fs::write(directory.join("gate.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("explain")
        .arg("gate.program")
        .args(extra)
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("start execute")
}

fn verdict(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("gate prints one object")
}

#[test]
fn the_gate_denies_by_default_and_names_each_violation() {
    let directory = tempfile::tempdir().unwrap();
    let output = gate(
        directory.path(),
        "run plain.helper\n",
        &["--json", "--allow", "reads"],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let object = verdict(&output);
    assert_eq!(object["protocol"], "execute.gate/1");
    assert_eq!(object["allowed"], false);
    let violations = object["violations"].as_array().unwrap();
    assert!(
        violations
            .iter()
            .any(|v| v["category"] == "undeclared" && v["line"] == 1),
        "{object}"
    );
}

#[test]
fn approving_the_violating_categories_passes_with_exit_zero() {
    let directory = tempfile::tempdir().unwrap();
    let output = gate(
        directory.path(),
        "run plain.helper\n",
        &["--json", "--allow", "undeclared"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let object = verdict(&output);
    assert_eq!(object["allowed"], true);
    assert!(object["violations"].as_array().unwrap().is_empty());
}

#[test]
fn listener_producer_requires_privileged_approval() {
    let directory = tempfile::tempdir().unwrap();
    let output = gate(
        directory.path(),
        "listen to \"sudo\" with arguments [\"rm\", \"-rf\", \"/important\"]\nwhenever a line arrives as msg\n  run \"printf\" with arguments [\"x\"]\n",
        &["--json", "--allow", "undeclared"],
    );
    assert_eq!(output.status.code(), Some(1));
    let object = verdict(&output);
    assert!(
        object["violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|violation| { violation["category"] == "privileged" && violation["line"] == 1 }),
        "{object}"
    );
}

#[test]
fn a_network_effect_needs_its_own_approval() {
    let directory = tempfile::tempdir().unwrap();
    let denied = gate(
        directory.path(),
        "run net.helper\n",
        &["--json", "--allow", "undeclared"],
    );
    assert_eq!(denied.status.code(), Some(1));
    assert!(verdict(&denied)["violations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["category"] == "network"));

    let approved = gate(
        directory.path(),
        "run net.helper\n",
        &["--json", "--allow", "network"],
    );
    assert_eq!(
        approved.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
}

#[test]
fn a_privileged_step_is_a_violation_until_approved() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run \"sudo\" with arguments [\"-n\", \"true\"]\n";
    let denied = gate(
        directory.path(),
        source,
        &["--json", "--allow", "undeclared"],
    );
    assert_eq!(denied.status.code(), Some(1));
    assert!(verdict(&denied)["violations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["category"] == "privileged"));
    let approved = gate(
        directory.path(),
        source,
        &["--json", "--allow", "undeclared", "--allow", "privileged"],
    );
    assert_eq!(
        approved.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
}

#[test]
fn unknown_categories_are_usage_errors_and_allow_requires_json() {
    let directory = tempfile::tempdir().unwrap();
    let unknown = gate(
        directory.path(),
        "run plain.helper\n",
        &["--json", "--allow", "everything"],
    );
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown --allow category"));
    assert!(unknown.stdout.is_empty());

    let text = gate(
        directory.path(),
        "run plain.helper\n",
        &["--allow", "undeclared"],
    );
    assert!(!text.status.success());
    assert!(String::from_utf8_lossy(&text.stderr).contains("--allow requires --json"));
    assert!(text.stdout.is_empty());
}

#[test]
fn a_plain_explain_without_allow_stays_advisory_and_exits_zero() {
    let directory = tempfile::tempdir().unwrap();
    let output = gate(directory.path(), "run plain.helper\n", &["--json"]);
    assert_eq!(output.status.code(), Some(0));
    let object: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(object["protocol"], "execute.explain/1");
}

fn violation_categories(object: &Value) -> Vec<String> {
    object["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["category"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn each_declared_effect_maps_to_its_own_category_and_needs_its_own_approval() {
    let directory = tempfile::tempdir().unwrap();
    for (applet, category) in [
        ("read.helper", "reads"),
        ("write.helper", "writes"),
        ("spawn.helper", "process"),
    ] {
        let source = format!("run {applet}\n");
        let denied = gate(
            directory.path(),
            &source,
            &["--json", "--allow", "undeclared"],
        );
        assert_eq!(denied.status.code(), Some(1), "{applet}");
        assert!(
            violation_categories(&verdict(&denied)).contains(&category.to_owned()),
            "{applet} must trigger {category}"
        );
        let approved = gate(directory.path(), &source, &["--json", "--allow", category]);
        assert_eq!(
            approved.status.code(),
            Some(0),
            "{applet} with --allow {category}: {}",
            String::from_utf8_lossy(&approved.stderr)
        );
    }
}

#[test]
fn an_unknown_effect_is_undeclared_and_needs_explicit_approval() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run mystery.helper\n";
    let denied = gate(directory.path(), source, &["--json", "--allow", "reads"]);
    assert_eq!(denied.status.code(), Some(1));
    assert!(violation_categories(&verdict(&denied)).contains(&"undeclared".to_owned()));
    let approved = gate(
        directory.path(),
        source,
        &["--json", "--allow", "undeclared"],
    );
    assert_eq!(
        approved.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
}

#[test]
fn an_invocation_with_null_effects_is_undeclared_even_without_an_explicit_flag() {
    let directory = tempfile::tempdir().unwrap();
    let object = verdict(&gate(
        directory.path(),
        "run plain.helper\n",
        &["--json", "--allow", "reads"],
    ));
    assert!(
        violation_categories(&object).contains(&"undeclared".to_owned()),
        "{object}"
    );
}
