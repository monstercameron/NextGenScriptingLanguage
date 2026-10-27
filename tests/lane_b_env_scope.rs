//! Per-command environment: `and environment {...}` sets variables for one native
//! command, and `and without environment [...]` removes them for that command only.
//! Values are secrets: explain shows names only.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_script(directory: &Path, command: &str, script: &str, extra: &[&str]) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .env("LANE_B_UNSET", "present")
        .arg(command)
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(extra)
        .output()
        .unwrap()
}

fn run(script: &str, extra: &[&str]) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "run", script, extra);
    (directory, output)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn environment_set_applies_to_that_command_only() {
    let (_directory, output) = run(
        "run \"env\" with arguments [] and environment {\"LANE_B_SCOPED\": \"yes\"}\nrun \"env\" with arguments []\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let text = stdout(&output);
    // Both commands print their environment. Only the first has the variable,
    // so the marker appears exactly once.
    assert_eq!(text.matches("LANE_B_SCOPED=yes").count(), 1, "{text}");
}

#[test]
fn environment_without_removes_for_that_command_only() {
    let (_directory, output) = run(
        "run \"env\" with arguments [] and without environment [\"LANE_B_UNSET\"]\nrun \"env\" with arguments []\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    // The second command still inherits LANE_B_UNSET from the Execute process.
    assert_eq!(stdout(&output).matches("LANE_B_UNSET=present").count(), 1);
}

#[test]
fn environment_value_can_come_from_a_binding() {
    let (_directory, output) = run(
        "accept mode as text with default \"zz\"\nrun \"env\" with arguments [] and environment {\"LANE_B_MODE\": mode}\n",
        &["--input", "mode=from-input"],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(
        stdout(&output).contains("LANE_B_MODE=from-input"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn literal_nul_environment_is_rejected_before_earlier_effects() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"first\"\nthen save to \"marker.txt\"\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": \"x\\u0000y\"}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert_ne!(
        check.status.code(),
        Some(0),
        "check stdout: {:?}",
        check.stdout
    );
    let check_stderr = String::from_utf8_lossy(&check.stderr);
    assert!(check_stderr.contains("environment value"), "{check_stderr}");

    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("environment value"),
        "{:?}",
        output.stderr
    );
    assert!(
        !directory.path().join("marker.txt").exists(),
        "the earlier save ran before environment validation"
    );
}

#[test]
fn nul_in_a_text_binding_is_rejected_before_spawning_the_command() {
    let (_directory, output) = run(
        "run \"printf\" with arguments [\"\\\\000\"] as value\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": value}\n",
        &[],
    );
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("environment value"), "{stderr}");
    assert!(stderr.contains("NUL byte"), "{stderr}");
}

#[test]
fn known_nul_text_binding_is_rejected_before_earlier_effects() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"x\\u0000y\" as bad\nuse value bad as alias\nthen save to \"marker.txt\"\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": alias}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert_ne!(
        check.status.code(),
        Some(0),
        "check stdout: {:?}",
        check.stdout
    );
    let check_stderr = String::from_utf8_lossy(&check.stderr);
    assert!(check_stderr.contains("environment value"), "{check_stderr}");
    assert!(check_stderr.contains("NUL byte"), "{check_stderr}");

    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("environment value"), "{stderr}");
    assert!(stderr.contains("NUL byte"), "{stderr}");
    assert!(
        !directory.path().join("marker.txt").exists(),
        "the save ran before validating the known NUL binding"
    );
}

#[test]
fn known_nul_environment_binding_survives_branch_merge() {
    let directory = tempfile::tempdir().unwrap();
    let script = "accept enabled as boolean with default false\nif enabled is true\n  use value \"x\\u0000y\" as bad\notherwise\n  use value \"a\\u0000b\" as bad\nthen save to \"marker.txt\"\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": bad}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("environment value"), "{stderr}");
    assert!(stderr.contains("NUL byte"), "{stderr}");
    assert!(
        !directory.path().join("marker.txt").exists(),
        "the save ran before validating the merged NUL binding"
    );
}

#[test]
fn known_nul_taken_through_a_static_pointer_is_rejected_before_earlier_effects() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/x\" as pointer\nuse value {\"x\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\ntake text at pointer from document as bad\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": bad}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert_ne!(
        check.status.code(),
        Some(0),
        "check stdout: {:?}",
        check.stdout
    );
    let check_stderr = String::from_utf8_lossy(&check.stderr);
    assert!(check_stderr.contains("NUL byte"), "{check_stderr}");

    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("NUL byte"), "{stderr}");
    assert!(
        !directory.path().join("marker.txt").exists(),
        "the save ran before validating the statically known take result"
    );
}

#[test]
fn unknown_pointer_does_not_treat_an_unselected_nul_as_the_take_result() {
    let directory = tempfile::tempdir().unwrap();
    let script = "accept pointer as text with default \"/safe\"\nuse value {\"safe\": \"ok\", \"other\": \"x\\u0000y\"} as document\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert!(check.status.success(), "check stderr: {:?}", check.stderr);
    let output = run_script(directory.path(), "run", script, &[]);
    assert!(output.status.success(), "run stderr: {:?}", output.stderr);
}

#[test]
fn exact_loop_rebinding_updates_known_pointer_values() {
    let invalid = tempfile::tempdir().unwrap();
    let invalid_script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let invalid_output = run_script(invalid.path(), "run", invalid_script, &[]);
    assert_ne!(invalid_output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&invalid_output.stderr).contains("NUL byte"),
        "{:?}",
        invalid_output.stderr
    );
    assert!(!invalid.path().join("marker.txt").exists());

    let valid = tempfile::tempdir().unwrap();
    let valid_script = "use value \"/nul\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let valid_output = run_script(valid.path(), "run", valid_script, &[]);
    assert!(valid_output.status.success(), "{:?}", valid_output.stderr);
    assert!(valid.path().join("marker.txt").exists());
}

#[test]
fn finally_rebinding_updates_known_pointer_values() {
    let invalid = tempfile::tempdir().unwrap();
    let invalid_script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  try\n    use value \"/safe\" as pointer\n  finally\n    use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let invalid_output = run_script(invalid.path(), "run", invalid_script, &[]);
    assert_ne!(invalid_output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&invalid_output.stderr).contains("NUL byte"),
        "{:?}",
        invalid_output.stderr
    );
    assert!(!invalid.path().join("marker.txt").exists());

    let valid = tempfile::tempdir().unwrap();
    let valid_script = "use value \"/nul\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  try\n    use value \"/nul\" as pointer\n  finally\n    use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let valid_output = run_script(valid.path(), "run", valid_script, &[]);
    assert!(valid_output.status.success(), "{:?}", valid_output.stderr);
    assert!(valid.path().join("marker.txt").exists());
}

#[test]
fn unreachable_loop_assignments_do_not_replace_known_pointer_values() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  break\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(directory.path().join("marker.txt").exists());
}

#[test]
fn finally_created_pointer_facts_are_merged_for_preflight() {
    let invalid = tempfile::tempdir().unwrap();
    let invalid_script = "use value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\ntry\n  use value \"body\"\nfinally\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let invalid_output = run_script(invalid.path(), "run", invalid_script, &[]);
    assert_ne!(invalid_output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&invalid_output.stderr).contains("NUL byte"),
        "{:?}",
        invalid_output.stderr
    );
    assert!(!invalid.path().join("marker.txt").exists());

    let valid = tempfile::tempdir().unwrap();
    let valid_script = "use value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\ntry\n  use value \"body\"\nfinally\n  use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let valid_output = run_script(valid.path(), "run", valid_script, &[]);
    assert!(valid_output.status.success(), "{:?}", valid_output.stderr);
    assert!(valid.path().join("marker.txt").exists());
}

#[test]
fn nested_loop_break_does_not_hide_outer_loop_rebinding() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 1 times\n  repeat 1 times\n    break\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("NUL byte"),
        "{:?}",
        output.stderr
    );
    assert!(!directory.path().join("marker.txt").exists());
}

#[test]
fn repeated_constant_loop_assignments_keep_the_final_known_value() {
    let invalid = tempfile::tempdir().unwrap();
    let invalid_script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 2 times\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let invalid_output = run_script(invalid.path(), "run", invalid_script, &[]);
    assert_ne!(invalid_output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&invalid_output.stderr).contains("NUL byte"),
        "{:?}",
        invalid_output.stderr
    );
    assert!(!invalid.path().join("marker.txt").exists());

    let valid = tempfile::tempdir().unwrap();
    let valid_script = "use value \"/nul\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 2 times\n  use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let valid_output = run_script(valid.path(), "run", valid_script, &[]);
    assert!(valid_output.status.success(), "{:?}", valid_output.stderr);
    assert!(valid.path().join("marker.txt").exists());
}

#[test]
fn two_item_for_each_does_not_require_a_repeat_summary() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nuse value [\"one\", \"two\"] as items\nfor each item in items\n  use value \"/safe\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(directory.path().join("marker.txt").exists());
}

#[test]
fn repeat_with_early_transfer_does_not_require_a_repeat_summary() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nthen save to \"marker.txt\"\nrepeat 2 times\n  break\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let output = run_script(directory.path(), "run", script, &[]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(directory.path().join("marker.txt").exists());
}

#[test]
fn nested_loop_exit_still_blocks_outer_loop_fact_propagation() {
    let directory = tempfile::tempdir().unwrap();
    let script = "use value \"/safe\" as pointer\nuse value {\"safe\": \"ok\", \"nul\": \"x\\u0000y\"} as document\nrepeat 1 times\n  repeat 1 times\n    exit with status 0\n  use value \"/nul\" as pointer\ntake text at pointer from document as selected\nrun \"env\" with arguments [] and environment {\"LANE_B_NUL\": selected}\n";
    let check = run_script(directory.path(), "check", script, &[]);
    assert!(check.status.success(), "{:?}", check.stderr);
}

#[test]
fn explain_shows_names_but_not_values() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "explain",
        "run \"env\" with arguments [] and environment {\"LANE_B_SECRET\": \"hunter2-value\"} and without environment [\"LANE_B_OLD\"]\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let text = stdout(&output);
    assert!(text.contains("LANE_B_SECRET"), "{text}");
    assert!(text.contains("LANE_B_OLD"), "{text}");
    assert!(!text.contains("hunter2-value"), "{text}");
}

#[test]
fn invalid_environment_forms_are_rejected_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let cases = [
        (
            "run \"env\" with arguments [] and environment {\"BAD=NAME\": \"x\"}\n",
            "invalid environment name",
        ),
        (
            "run \"env\" with arguments [] and environment {\"LANE_B_NUM\": 1}\n",
            "must be a text literal",
        ),
        (
            "run \"env\" with arguments [] and environment {\"LANE_B_BOTH\": \"x\"} and without environment [\"LANE_B_BOTH\"]\n",
            "both set and removed",
        ),
        (
            "run \"env\" with arguments [] and environment {\"LANE_B_X\": \"x\"}\npipe to \"tr\" with arguments [\"a\", \"b\"]\n",
            "single command",
        ),
        (
            "run cd with path \".\" and environment {\"LANE_B_X\": \"x\"}\n",
            "builtin",
        ),
    ];
    for (script, expected) in cases {
        let output = run_script(directory.path(), "check", script, &[]);
        assert_ne!(output.status.code(), Some(0), "{script}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{script}: {stderr}");
    }
}
