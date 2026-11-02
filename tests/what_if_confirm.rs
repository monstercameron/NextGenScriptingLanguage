//! `run --what-if` and `run --confirm FINGERPRINT`: the fingerprint covers the plan
//! and everything the run would use, so a confirmed plan cannot run with something
//! else. Each test changes one covered setting between review and confirmation.
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[path = "support/private_answers.rs"]
mod private_answers;

const POLICY: &str = r#"{"version":1,"applets":{"text.concat":{"capabilities":["filesystem.read","filesystem.write"],"filesystem":{"read":["."],"write":["."]}}}}"#;

const POLICY_DENY_PRIVILEGED: &str = r#"{"version":1,"deny_privileged":true,"applets":{"text.concat":{"capabilities":["filesystem.read","filesystem.write"],"filesystem":{"read":["."],"write":["."]}}}}"#;

const PROGRAM: &str = "accept target as text\nuse value \"payload\"\nthen save to target\n";

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn manifests(root: &Path, note: &str) {
    write(
        root,
        "manifests/plain.helper.json",
        &format!(
            r#"{{"name":"plain.helper","binary":"printf","accepts_stdin":false,"parameters":{{}},"arguments":[{{"literal":"{note}"}}]}}"#
        ),
    );
    // The policy names this applet, so the catalog must declare it.
    write(
        root,
        "manifests/text.concat.json",
        r#"{"name":"text.concat","binary":"printf","accepts_stdin":false,"parameters":{},"arguments":[],"contract":{"version":1,"backend":"command","lifecycle":{"kind":"oneshot"},"capabilities":["filesystem.read","filesystem.write"],"effects":["filesystem.read","filesystem.write"]}}"#,
    );
}

fn execute(cwd: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .current_dir(cwd)
        .output()
        .unwrap()
}

/// The fingerprint `--what-if` prints for the given extra arguments.
fn review(cwd: &Path, program: &str, extra: &[&str]) -> (String, String) {
    let mut arguments = vec!["run", program, "--what-if", "--manifest-dir", "manifests"];
    arguments.extend_from_slice(extra);
    let output = execute(cwd, &arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    (
        report["fingerprint"].as_str().unwrap().to_owned(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn confirm(cwd: &Path, program: &str, fingerprint: &str, extra: &[&str]) -> Output {
    let mut arguments = vec![
        "run",
        program,
        "--confirm",
        fingerprint,
        "--manifest-dir",
        "manifests",
    ];
    arguments.extend_from_slice(extra);
    execute(cwd, &arguments)
}

fn refused(output: &Output) -> bool {
    !output.status.success()
        && String::from_utf8_lossy(&output.stderr).contains("plan changed since --what-if")
}

#[cfg(unix)]
#[test]
fn a_binary_swapped_after_confirm_is_refused_at_spawn() {
    // The program itself swaps the reviewed executable mid-run: the first
    // step copies new bytes over the tool, the second step runs it. The
    // confirm-time check passes (nothing changed yet), so only the
    // verify-at-spawn binding can refuse the run — and the swapped bytes
    // must never execute.
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("swaptool");
    let swapped = bin.join("swapped");
    fs::write(&tool, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&swapped, b"#!/bin/sh\necho swapped > pwned.txt\n").unwrap();
    write(
        root.path(),
        "job.program",
        &format!(
            "run \"cp\" with arguments [\"{}\", \"{}\"]\nrun \"swaptool\" with arguments []\n",
            swapped.display(),
            tool.display(),
        ),
    );
    let path = {
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin.clone()];
        paths.extend(std::env::split_paths(&old_path));
        std::env::join_paths(paths).unwrap()
    };
    let review_output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "job.program", "--what-if"])
        .current_dir(root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        review_output.status.success(),
        "{}",
        String::from_utf8_lossy(&review_output.stderr)
    );
    let report: Value = serde_json::from_slice(&review_output.stdout).unwrap();
    let fingerprint = report["fingerprint"].as_str().unwrap().to_owned();

    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "job.program", "--confirm", &fingerprint])
        .current_dir(root.path())
        .env("PATH", &path)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!output.status.success(), "swapped run must fail: {stderr}");
    // The confirm-time check passed — the swap landed mid-run — so the
    // refusal must come from the spawn-time binding, not the fingerprint.
    assert!(
        !stderr.contains("plan changed since --what-if"),
        "confirm check should pass; the spawn binding must refuse: {stderr}"
    );
    assert!(
        stderr.contains("changed after the confirmed review"),
        "spawn binding must name the swap: {stderr}"
    );
    // The swap really happened (a pre-fix build would have executed it),
    // yet the swapped bytes never ran.
    assert_eq!(
        fs::read(&tool).unwrap(),
        b"#!/bin/sh\necho swapped > pwned.txt\n"
    );
    assert!(
        !root.path().join("pwned.txt").exists(),
        "the swapped binary must never execute"
    );
}

#[test]
fn an_unchanged_reviewed_plan_runs() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "job.program", PROGRAM);
    manifests(root.path(), "--version");
    let (fingerprint, _) = review(root.path(), "job.program", &["--input", "target=out.txt"]);
    let output = confirm(
        root.path(),
        "job.program",
        &fingerprint,
        &["--input", "target=out.txt"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("out.txt")).unwrap(),
        "payload"
    );
}

#[test]
fn changing_path_resolution_changes_the_review_fingerprint() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("bin1");
    let second = root.path().join("bin2");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let executable = if cfg!(windows) {
        "mytool.exe"
    } else {
        "mytool"
    };
    fs::copy(env!("CARGO_BIN_EXE_execute-model"), first.join(executable)).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_execute-model"), second.join(executable)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [first.join(executable), second.join(executable)] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    write(
        root.path(),
        "job.program",
        "run \"mytool\" with arguments []\n",
    );

    let fingerprint_for = |bin: &Path| {
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin.to_path_buf()];
        paths.extend(std::env::split_paths(&old_path));
        let path = std::env::join_paths(paths).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["run", "job.program", "--what-if"])
            .current_dir(root.path())
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        report["fingerprint"].as_str().unwrap().to_owned()
    };
    assert_ne!(fingerprint_for(&first), fingerprint_for(&second));
}

#[test]
fn a_different_input_value_is_refused_before_any_effect() {
    // The review allowed out.txt; the confirmation names a different target.
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "job.program", PROGRAM);
    manifests(root.path(), "--version");
    let (fingerprint, _) = review(
        root.path(),
        "job.program",
        &["--input", "target=reviewed.txt"],
    );
    let output = confirm(
        root.path(),
        "job.program",
        &fingerprint,
        &["--input", "target=other.txt"],
    );
    assert!(
        refused(&output),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("other.txt").exists());
}

#[test]
fn an_environment_value_is_covered_by_a_keyed_digest_and_never_printed() {
    // The secret changes the fingerprint, so a changed secret needs a new review. The
    // report shows only an HMAC under the per-user key, never the value.
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "job.program",
        "accept target as text from environment \"EXECUTE_WHAT_IF_TARGET\"
use value \"payload\"
then save to target
",
    );
    manifests(root.path(), "--version");
    let state = root.path().join("state");
    let review = |value: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(args)
            .arg("--manifest-dir")
            .arg("manifests")
            .env("EXECUTE_WHAT_IF_TARGET", value)
            .env("XDG_STATE_HOME", &state)
            .env("LOCALAPPDATA", &state)
            .current_dir(root.path())
            .output()
            .unwrap()
    };

    let reviewed = review(
        "reviewed-secret-path.txt",
        &["run", "job.program", "--what-if"],
    );
    assert!(
        reviewed.status.success(),
        "{}",
        String::from_utf8_lossy(&reviewed.stderr)
    );
    let shown = String::from_utf8_lossy(&reviewed.stdout).into_owned();
    assert!(!shown.contains("reviewed-secret-path.txt"), "{shown}");
    let report: Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(report["settings"]["inputs"][0]["value"]["sensitive"], true);
    assert_eq!(
        report["settings"]["inputs"][0]["value"]["hmac_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    let fingerprint = report["fingerprint"].as_str().unwrap().to_owned();

    let same = review(
        "reviewed-secret-path.txt",
        &["run", "job.program", "--confirm", &fingerprint],
    );
    assert!(
        same.status.success(),
        "{}",
        String::from_utf8_lossy(&same.stderr)
    );

    let changed = review(
        "other-path.txt",
        &["run", "job.program", "--confirm", &fingerprint],
    );
    assert!(
        refused(&changed),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
}

#[test]
fn a_secret_prompt_answer_is_keyed_and_never_printed() {
    // A secret prompt answer supplied with --input is as sensitive as an environment
    // value: the report shows only its HMAC, and a changed answer changes the fingerprint.
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "job.program",
        "accept target as text from secret prompt \"Target?\"\nuse value \"payload\"\nthen save to target\n",
    );
    manifests(root.path(), "--version");
    let state = root.path().join("state");
    let run_review = |answer: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(args)
            .arg("--manifest-dir")
            .arg("manifests")
            .arg("--input")
            .arg(format!("target={answer}"))
            .env("XDG_STATE_HOME", &state)
            .env("LOCALAPPDATA", &state)
            .current_dir(root.path())
            .output()
            .unwrap()
    };

    let reviewed = run_review(
        "reviewed-secret-answer.txt",
        &["run", "job.program", "--what-if"],
    );
    assert!(
        reviewed.status.success(),
        "{}",
        String::from_utf8_lossy(&reviewed.stderr)
    );
    let shown = String::from_utf8_lossy(&reviewed.stdout).into_owned();
    assert!(!shown.contains("reviewed-secret-answer"), "{shown}");
    let report: Value = serde_json::from_str(&shown).unwrap();
    let input = &report["settings"]["inputs"][0];
    assert_eq!(input["source"], "secret_prompt");
    assert_eq!(input["value"]["sensitive"], true);
    assert_eq!(input["value"]["hmac_sha256"].as_str().unwrap().len(), 64);
    let fingerprint = report["fingerprint"].as_str().unwrap().to_owned();

    let same = run_review(
        "reviewed-secret-answer.txt",
        &["run", "job.program", "--confirm", &fingerprint],
    );
    assert!(
        same.status.success(),
        "{}",
        String::from_utf8_lossy(&same.stderr)
    );

    let changed = run_review(
        "other-secret-answer.txt",
        &["run", "job.program", "--confirm", &fingerprint],
    );
    assert!(
        refused(&changed),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert!(!root.path().join("other-secret-answer.txt").exists());
}

#[cfg(unix)]
#[test]
fn an_environment_input_refuses_the_review_when_the_key_is_readable_by_others() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "job.program",
        "accept target as text from environment \"EXECUTE_WHAT_IF_TARGET\"
use value \"payload\"
then save to target
",
    );
    manifests(root.path(), "--version");
    let state = root.path().join("state");
    let key = state.join("execute").join("what-if.key");
    write(root.path(), "state/execute/what-if.key", &"k".repeat(32));
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            "job.program",
            "--what-if",
            "--manifest-dir",
            "manifests",
        ])
        .env("EXECUTE_WHAT_IF_TARGET", "reviewed-secret-path.txt")
        .env("XDG_STATE_HOME", &state)
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("readable by other users"), "{stderr}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("reviewed-secret-path.txt"));
}

#[test]
fn a_changed_capability_policy_is_refused() {
    // The policy preflight needs literal write paths, so this program writes a literal.
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "literal.program",
        "use value \"payload\"
then save to \"out.txt\"
",
    );
    manifests(root.path(), "--version");
    write(root.path(), "policy.json", POLICY);
    // A capability policy is security-sensitive: without a private ACL Windows
    // refuses the review before any fingerprint runs.
    private_answers::make_private(&root.path().join("policy.json"));
    let (fingerprint, _) = review(
        root.path(),
        "literal.program",
        &["--capability-policy", "policy.json"],
    );
    write(root.path(), "policy.json", POLICY_DENY_PRIVILEGED);
    // Truncating keeps the private ACL, but re-assert it so the confirm refusal
    // proves a policy change, not a privacy error.
    private_answers::make_private(&root.path().join("policy.json"));
    let output = confirm(
        root.path(),
        "literal.program",
        &fingerprint,
        &["--capability-policy", "policy.json"],
    );
    assert!(
        refused(&output),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("out.txt").exists());
}

#[test]
fn a_changed_manifest_is_refused() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "job.program", PROGRAM);
    manifests(root.path(), "--version");
    let (fingerprint, _) = review(root.path(), "job.program", &["--input", "target=out.txt"]);
    manifests(root.path(), "--help");
    let output = confirm(
        root.path(),
        "job.program",
        &fingerprint,
        &["--input", "target=out.txt"],
    );
    assert!(
        refused(&output),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_different_working_directory_is_refused() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "job.program", PROGRAM);
    manifests(root.path(), "--version");
    let other = root.path().join("elsewhere");
    fs::create_dir_all(&other).unwrap();
    let (fingerprint, _) = review(root.path(), "job.program", &["--input", "target=out.txt"]);
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            root.path().join("job.program").to_str().unwrap(),
            "--confirm",
            &fingerprint,
            "--input",
            "target=out.txt",
            "--manifest-dir",
            root.path().join("manifests").to_str().unwrap(),
        ])
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(
        refused(&output),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!other.join("out.txt").exists());
}

#[test]
fn the_report_lists_the_settings_it_covers() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "job.program", PROGRAM);
    manifests(root.path(), "--version");
    let (_, shown) = review(root.path(), "job.program", &["--input", "target=out.txt"]);
    let report: Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(report["settings"]["inputs"][0]["name"], "target");
    assert_eq!(report["settings"]["inputs"][0]["value"], "out.txt");
    assert_eq!(report["settings"]["manifests"].as_array().unwrap().len(), 2);
    assert!(report["settings"]["cwd"].is_string());
}
