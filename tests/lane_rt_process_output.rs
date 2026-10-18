//! Process and output regressions for RT.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_script(script: &str) -> Output {
    run_script_with_args(script, &[])
}

fn run_script_with_args(script: &str, extra: &[&str]) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("case.program");
    fs::write(&program, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(extra)
        .output()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn parallel_each_output_limit_fails_the_group() {
    let output = run_script_with_args(
        "parallel for each x in [\"a\", \"b\"] at most 1 as results\
           run \"head\" with arguments [\"-c\", \"700000\", \"/dev/zero\"]\n",
        &["--max-output-bytes", "1000000"],
    );
    assert!(
        !output.status.success(),
        "expected group output limit failure"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("parallel for each output exceeded"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn missing_use_folder_error_includes_line_and_path() {
    let output = run_script("use folder \"rt_missing_folder_7c2e\"\n");
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(diagnostic.contains("line 1: use folder"), "{diagnostic}");
    assert!(
        diagnostic.contains("rt_missing_folder_7c2e"),
        "{diagnostic}"
    );
}

#[cfg(unix)]
#[test]
fn early_successful_pipe_consumer_keeps_its_output() {
    let output = run_script(
        "run \"seq\" with arguments [\"1\", \"200000\"]\
         pipe to \"head\" with arguments [\"-n\", \"1\"] as first\
         run \"printf\" with arguments [\"first=%s\\n\", first]\n",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "first=1\n");
}

#[cfg(unix)]
#[test]
fn command_condition_drops_stderr() {
    let output = run_script(
        "if run \"sh\" with arguments [\"-c\", \"echo out-secret; echo err-secret >&2\"] succeeds\
           run \"printf\" with message \"ok\\n\"\n",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ok\n");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[cfg(unix)]
#[test]
fn merged_stderr_binding_matches_the_pipeline_value() {
    let output = run_script(
        "run \"sh\" with arguments [\"-c\", \"echo o; echo e >&2\"] and stderr merged as out\
         use value out\n",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "o\ne\n");
}
