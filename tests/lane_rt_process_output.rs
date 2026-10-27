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
        "parallel for each x in [\"a\", \"b\"] at most 1 as results\n\
         \x20 run \"head\" with arguments [\"-c\", \"700000\", \"/dev/zero\"]\n",
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
        "run \"seq\" with arguments [\"1\", \"200000\"]\n\
         pipe to \"head\" with arguments [\"-n\", \"1\"] as first\n\
         run \"printf\" with arguments [\"first=%s\\n\", first]\n",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    // The named result retains head's newline; printf appends its own.
    assert_eq!(String::from_utf8_lossy(&output.stdout), "first=1\n\n");
}

#[cfg(unix)]
#[test]
fn command_condition_drops_stderr() {
    let output = run_script(
        "if run \"sh\" with arguments [\"-c\", \"echo out-secret; echo err-secret >&2\"] succeeds\n\
         \x20 run \"printf\" with arguments [\"ok\\n\"]\n",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ok\n");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[cfg(unix)]
#[test]
fn merged_stderr_binding_matches_the_pipeline_value() {
    let output = run_script(
        "run \"sh\" with arguments [\"-c\", \"echo o; echo e >&2\"] and stderr merged as out\n\
         use value out\n",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "o\ne\n");
}

// These commands record an effect before producing output. A post-effect
// serialization error must not discard the completed results.
#[cfg(unix)]
fn assert_parallel_each_raw_budget(script: &str, expected_effects: usize) {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("case.program");
    fs::write(&program, script).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(["--max-output-bytes", "65536"])
        .output()
        .unwrap();
    let ledger = fs::read_to_string(directory.path().join("ledger")).unwrap_or_default();
    assert_eq!(
        ledger.lines().count(),
        expected_effects,
        "ledger: {ledger:?}"
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"done");
}

#[cfg(unix)]
#[test]
fn parallel_each_small_outputs_preserve_results_after_effects() {
    assert_parallel_each_raw_budget(
        "parallel for each n in [\"a\", \"b\", \"c\", \"d\"] at most 4 as results\n  run \"sh\" with arguments [\"-c\", \"echo $0 >> ledger; yes x | head -c 3000\", n]\nuse value \"done\"\n",
        4,
    );
}

#[cfg(unix)]
#[test]
fn parallel_each_single_element_may_use_ninety_percent_of_budget() {
    assert_parallel_each_raw_budget(
        "parallel for each n in [\"a\", \"b\"] at most 2 as results\n  run \"sh\" with arguments [\"-c\", \"echo $0 >> ledger; if [ $0 = a ]; then head -c 58982 /dev/zero; fi\", n]\nuse value \"done\"\n",
        2,
    );
}

#[cfg(unix)]
#[test]
fn parallel_each_oversized_metadata_fails_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("case.program");
    let items = (0..300)
        .map(|index| format!("\"item-{index:04}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        &program,
        format!("parallel for each n in [{items}] at most 1 as results\n  run \"sh\" with arguments [\"-c\", \"echo effect >> ledger\"]\n"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(["--max-output-bytes", "4096"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("result metadata exceeds"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!directory.path().join("ledger").exists());
}

#[cfg(unix)]
#[test]
fn parallel_each_stderr_and_stdout_share_the_capture_budget() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("case.program");
    fs::write(
        &program,
        "parallel for each n in [\"a\", \"b\"] at most 1 as results\n  run \"sh\" with arguments [\"-c\", \"if [ $0 = a ]; then head -c 8000 /dev/zero >&2; echo first >> ledger; else head -c 9000 /dev/zero && echo second >> ledger; fi\", n]\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(["--max-output-bytes", "16384"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("parallel for each output exceeded"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("ledger")).unwrap(),
        "first\n"
    );
}

#[cfg(unix)]
#[test]
fn parallel_each_v3_stores_utf8_output_once() {
    let output = run_script(
        "parallel for each n in [\"a\"] at most 1 as results\n  run \"sh\" with arguments [\"-c\", \"printf abc; printf xy >&2\"]\nthen encode results as json\n",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["items"][0]["output_encoding"], "utf8");
    assert_eq!(value["items"][0]["output"], "abc");
    assert_eq!(value["items"][0]["stderr_encoding"], "utf8");
    assert_eq!(value["items"][0]["stderr"], "xy");
    assert!(value["items"][0].get("output_text").is_none());
    assert!(value["items"][0].get("stderr_text").is_none());
}

#[cfg(unix)]
#[test]
fn parallel_each_native_element_can_use_more_than_its_even_share() {
    assert_parallel_each_raw_budget(
        "parallel for each n in [\"a\", \"b\", \"c\", \"d\"] at most 4 as results\n  run \"sh\" with arguments [\"-c\", \"echo $0 >> ledger; if [ $0 = a ]; then yes x | head -c 20000; fi\", n]\nuse value \"done\"\n",
        4,
    );
}

#[cfg(unix)]
#[test]
fn parallel_each_native_output_at_ninety_percent_of_raw_limit_passes() {
    let output = run_script_with_args(
        "parallel for each n in [\"a\", \"b\"] at most 2 as results\n  run \"head\" with arguments [\"-c\", \"29491\", \"/dev/zero\"]\nuse value \"done\"\n",
        &["--max-output-bytes", "65536"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"done");
}

#[test]
fn parallel_each_in_process_output_at_ninety_percent_of_raw_limit_passes() {
    let output = run_script_with_args(
        "parallel for each n in [\"a\"] at most 1 as results\n  run text.repeat with text \"x\" and count 58982\nuse value \"done\"\n",
        &["--max-output-bytes", "65536"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"done");
}

#[test]
fn parallel_each_in_process_output_counts_toward_group_budget() {
    let output = run_script_with_args(
        "parallel for each n in [\"a\", \"b\"] at most 1 as results\n  run text.repeat with text \"x\" and count 6000\n",
        &["--max-output-bytes", "10000"],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("parallel for each output exceeded"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn parallel_each_v2_preserves_non_utf8_output() {
    let output = run_script(
        "parallel for each n in [\"a\"] at most 1 as results\n  run \"sh\" with arguments [\"-c\", \"printf '\\\\377'\"]\nthen encode results as json\n",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["items"][0]["output_encoding"], "base64");
    assert_eq!(value["items"][0]["output"], "/w==");
    assert!(value["items"][0].get("output_text").is_none());
}
