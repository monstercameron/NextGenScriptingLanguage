//! Conditions reject a JSON-typed operand compared with a value of another
//! kind. Null checks and same-kind comparisons keep working. The inputs avoid
//! quoted JSON because Windows argument quoting drops quotes in `printf` messages.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run(script: &str) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap()
}

#[test]
fn json_number_compared_with_boolean_is_a_type_error_not_false() {
    let output = run(
        "run printf with message \"42\"\nthen parse json as value\nif value is true\n  run printf with message \"matched\"\n",
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("condition compares integer with boolean"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}

#[test]
fn null_json_value_still_compares_false_with_a_number() {
    let output = run(
        "run printf with message \"null\"\nthen parse json as value\nif value is 1\n  run printf with message \"one\"\notherwise\n  run printf with message \"not one\"\n",
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "not one\n");
}
