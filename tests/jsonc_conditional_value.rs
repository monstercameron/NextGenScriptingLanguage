//! `jsonc.edit_content` needs a value for set, but not for remove.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn invoke(directory: &Path, source: &str, command: &str, inputs: &[&str]) -> Output {
    let program = directory.join("case.program");
    fs::write(&program, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(command)
        .arg(program)
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .args(inputs)
        .output()
        .unwrap()
}

#[test]
fn static_set_requires_a_value_but_remove_and_null_are_valid() {
    let directory = tempfile::tempdir().unwrap();
    let missing = "run jsonc.edit_content with content \"{}\" and action \"set\" and key \"x\"\n";
    for command in ["check", "run"] {
        let output = invoke(directory.path(), missing, command, &[]);
        assert!(!output.status.success(), "{command} accepted missing value");
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires value"));
    }
    for source in [
        "run jsonc.edit_content with content \"{}\" and action \"remove\" and key \"x\"\n",
        "run jsonc.edit_content with content \"{}\" and action \"set\" and key \"x\" and value null\n",
    ] {
        for command in ["check", "run"] {
            let output = invoke(directory.path(), source, command, &[]);
            assert!(output.status.success(), "{command} rejected {source}: {}", String::from_utf8_lossy(&output.stderr));
        }
    }
}

#[test]
fn dynamic_action_defers_the_requirement_until_resolution() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept choice as text\nrun jsonc.edit_content with content \"{}\" and action choice and key \"x\"\n";
    let checked = invoke(directory.path(), source, "check", &[]);
    assert!(
        checked.status.success(),
        "dynamic action must be deferred: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let removed = invoke(
        directory.path(),
        source,
        "run",
        &["--input", "choice=remove"],
    );
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let set = invoke(directory.path(), source, "run", &["--input", "choice=set"]);
    assert!(
        !set.status.success(),
        "dynamic set accepted a missing value"
    );
    assert!(String::from_utf8_lossy(&set.stderr).contains("requires value"));
}

#[test]
fn resolved_json_null_is_a_present_value() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept payload as json\nrun jsonc.edit_content with content \"{}\" and action \"set\" and key \"x\" and value payload\n";
    let checked = invoke(directory.path(), source, "check", &[]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let result = invoke(
        directory.path(),
        source,
        "run",
        &["--input", "payload=null"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["x"], serde_json::Value::Null);
    assert!(json.as_object().unwrap().contains_key("x"));
}

#[test]
fn bundled_manifest_applies_the_same_conditional_rule() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("case.program");
    fs::write(
        &program,
        "run jsonc.edit_content with content \"{}\" and action \"set\" and key \"x\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("check")
        .arg(&program)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires value"));
}
