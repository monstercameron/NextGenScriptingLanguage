//! `with temporary file|directory as NAME`: each scope gets a private path that is
//! removed when the scope ends on every exit path. Each test points the child's
//! temporary directory at its own folder, so the checks see only that run's scopes.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn execute_in(args: &[&str], directory: &Path, temp: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(directory)
        .env("TMPDIR", temp)
        .env("TMP", temp)
        .env("TEMP", temp)
        .output()
        .unwrap()
}

fn write_program(directory: &Path, text: &str) -> std::path::PathBuf {
    let program = directory.join("scope.program");
    fs::write(&program, text).unwrap();
    program
}

fn entries(temp: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(temp)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_file_scope_is_written_and_removed_after_success() {
    let directory = tempdir().unwrap();
    let temp = tempdir().unwrap();
    let program = write_program(
        directory.path(),
        "with temporary file as scratch\n  run text.trim_final_lf with text \"hello\"\n  then save to scratch\n  run text.trim_final_lf with text \"after\"\n",
    );
    let output = execute_in(
        &["run", program.to_str().unwrap()],
        directory.path(),
        temp.path(),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "after");
    assert!(
        entries(temp.path()).is_empty(),
        "scope left {:?}",
        entries(temp.path())
    );
}

#[test]
fn a_scope_is_removed_when_its_body_fails() {
    let directory = tempdir().unwrap();
    let temp = tempdir().unwrap();
    let program = write_program(
        directory.path(),
        "with temporary directory as work\n  fail with message \"boom\"\n",
    );
    let output = execute_in(
        &["run", program.to_str().unwrap()],
        directory.path(),
        temp.path(),
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stderr).contains("boom"));
    assert!(
        entries(temp.path()).is_empty(),
        "scope left {:?}",
        entries(temp.path())
    );
}

#[test]
fn an_exit_inside_a_scope_still_removes_it() {
    let directory = tempdir().unwrap();
    let temp = tempdir().unwrap();
    let program = write_program(
        directory.path(),
        "with temporary directory as outer\n  with temporary file as inner\n    exit with status 3\n",
    );
    let output = execute_in(
        &["run", program.to_str().unwrap()],
        directory.path(),
        temp.path(),
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(
        entries(temp.path()).is_empty(),
        "scope left {:?}",
        entries(temp.path())
    );
}

#[test]
fn a_scope_name_is_not_visible_after_its_body() {
    let directory = tempdir().unwrap();
    let temp = tempdir().unwrap();
    let program = write_program(
        directory.path(),
        "with temporary file as scratch\n  run text.trim_final_lf with text \"in\"\nthen save to scratch\n",
    );
    let output = execute_in(
        &["run", program.to_str().unwrap()],
        directory.path(),
        temp.path(),
    );
    assert_ne!(output.status.code(), Some(0), "a name outlived its scope");
}

#[test]
fn temporary_scopes_are_refused_under_checkpoints() {
    let directory = tempdir().unwrap();
    let temp = tempdir().unwrap();
    let checkpoint = directory.path().join("ck");
    let program = write_program(
        directory.path(),
        "with temporary file as scratch\n  run text.trim_final_lf with text \"x\"\n",
    );
    let output = execute_in(
        &[
            "run",
            program.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
        directory.path(),
        temp.path(),
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not supported under --checkpoint"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!checkpoint.exists());
    assert!(entries(temp.path()).is_empty());
}

#[test]
fn the_first_scope_sweeps_directories_whose_owner_is_gone() {
    let directory = tempdir().unwrap();
    let temp = tempdir().unwrap();
    let dead = temp.path().join("execute-scope-dead");
    let live = temp.path().join("execute-scope-live");
    let unrelated = temp.path().join("unrelated");
    for dir in [&dead, &live, &unrelated] {
        fs::create_dir(dir).unwrap();
    }
    // A pid this large is not a live process on any supported system.
    fs::write(dead.join("owner"), "4000000000").unwrap();
    // The test process is alive for the whole run.
    fs::write(live.join("owner"), std::process::id().to_string()).unwrap();
    let program = write_program(
        directory.path(),
        "with temporary file as scratch\n  run text.trim_final_lf with text \"x\"\n",
    );
    let output = execute_in(
        &["run", program.to_str().unwrap()],
        directory.path(),
        temp.path(),
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(!dead.exists(), "stale scope was not swept");
    assert!(live.exists(), "a live owner's scope was swept");
    assert!(unrelated.exists(), "an unrelated directory was swept");
}

/// Process substitution's replacement: two saves into temporaries, then diff on the paths.
#[cfg(unix)]
#[test]
fn a_diff_of_two_sorted_listings_runs_on_temporary_paths() {
    let directory = tempdir().unwrap();
    let temp = tempdir().unwrap();
    fs::write(directory.path().join("a"), "y\nx\n").unwrap();
    fs::write(directory.path().join("b"), "x\ny\n").unwrap();
    let program = write_program(
        directory.path(),
        "with temporary file as left\n  with temporary file as right\n    run \"sort\" with arguments [\"a\"]\n    then save to left\n    run \"sort\" with arguments [\"b\"]\n    then save to right\n    run \"diff\" with arguments [left, right]\n",
    );
    let output = execute_in(
        &["run", program.to_str().unwrap()],
        directory.path(),
        temp.path(),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        entries(temp.path()).is_empty(),
        "scope left {:?}",
        entries(temp.path())
    );
}

/// A policy that grants no write to the temporary directory refuses the scope before
/// it creates anything, and before the stale sweep can remove anything.
#[test]
fn a_policy_without_the_temporary_directory_refuses_scopes_before_any_effect() {
    let directory = tempdir().unwrap();
    let temp = tempdir().unwrap();
    let stale = temp.path().join("execute-scope-dead");
    fs::create_dir(&stale).unwrap();
    fs::write(stale.join("owner"), "4000000000").unwrap();
    let policy = directory.path().join("policy.json");
    fs::write(&policy, r#"{"version":1,"applets":{}}"#).unwrap();
    let program = write_program(
        directory.path(),
        "with temporary file as scratch\n  exit with status 0\n",
    );
    let output = execute_in(
        &[
            "run",
            program.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
        directory.path(),
        temp.path(),
    );
    assert_ne!(
        output.status.code(),
        Some(0),
        "a scope ran under a policy that forbids the temporary directory"
    );
    assert!(stale.exists(), "the stale sweep ran under that policy");
    assert_eq!(entries(temp.path()), vec!["execute-scope-dead".to_string()]);
}
