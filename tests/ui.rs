//! UI tests never contact GitHub; the gh fixture records argv and returns controlled JSON.
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;
const TRACKER: &str = include_str!("../examples/github-stars.program");
fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("gh"), include_str!("fixtures/gh.py")).unwrap();
    fs::set_permissions(dir.path().join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
    dir
}
fn invoke(dir: &Path, source: &str, mode: &str, args: &[&str]) -> Output {
    let file = dir.join("test.program");
    fs::write(&file, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir)
        .args(args)
        .arg(file)
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .env(
            "PATH",
            format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
        )
        .env("GH_FIXTURE_DIR", dir)
        .env("GH_FIXTURE_MODE", mode)
        .output()
        .unwrap()
}
#[test]
fn headless_tracker_maps_argv_and_renders_real_chart_widgets() {
    let dir = fixture();
    let output = invoke(dir.path(), TRACKER, "normal", &["--headless"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let widgets = &data["programs"]["tracker"]["widgets"];
    assert_eq!(
        widgets["metrics"]["rows"][0]["nameWithOwner"],
        "morluto/rea"
    );
    assert_eq!(widgets["history"]["series"]["morluto/rea"][0][1], 101);
    assert_eq!(data["completed_jobs"], 1);
    let frame = data["frame"].as_str().unwrap();
    for label in [
        "history",
        "stars",
        "session seconds",
        "morluto/rea",
        "101",
        "Forks",
    ] {
        assert!(frame.contains(label), "{label}: {frame}");
    }
    let argv: serde_json::Value = serde_json::from_str(
        fs::read_to_string(dir.path().join("calls.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(
        argv,
        serde_json::json!([
            "repo",
            "view",
            "--json",
            "nameWithOwner,stargazerCount,forkCount,description,url,updatedAt",
            "--",
            "morluto/rea"
        ])
    );
}
#[test]
fn check_and_explain_validate_without_launching_gh() {
    let dir = fixture();
    for command in ["check", "explain"] {
        assert!(invoke(dir.path(), TRACKER, "normal", &[command])
            .status
            .success());
    }
    assert!(!dir.path().join("calls.jsonl").exists());
}
#[test]
fn invalid_interfaces_and_widget_types_prevent_every_command() {
    let dir = fixture();
    for bad in [
        TRACKER.replace("to repository in tracker","to missing in tracker"),
        TRACKER.replace("accept repository as text","accept repository as text\n  offer loop as text").replace("offer repository as text","offer repository as text\n  accept loop as text").replace("connect repository from chooser to repository in tracker","connect repository from chooser to repository in tracker\nconnect loop from tracker to loop in chooser"),
        TRACKER.replace("named repo with value","named repository with value").replace("value of repo","value of repository").replace("whenever repo is submitted","whenever repository is submitted").replace("offer repository as text","accept repository as text\n  offer repository as text"),
        TRACKER.replace("show \"Refreshing...\" in status","show \"Refreshing...\" in history"),
        TRACKER.replace("accept repository as text","accept error as text"),
        TRACKER.replace("then add field \"stargazerCount\" to history","then add field \"stargazerCount\" to metrics"),
    ] {
        let output=invoke(dir.path(),&bad,"normal",&["--headless"]);
        assert!(!output.status.success(),"accepted invalid script: {bad}");
    }
    assert!(!dir.path().join("calls.jsonl").exists());
}
#[test]
fn errors_are_bounded_and_never_turn_missing_stars_into_zero() {
    let dir = fixture();
    for mode in ["fail", "invalid", "missing", "negative", "noisy"] {
        let output = invoke(dir.path(), TRACKER, mode, &["--headless"]);
        assert!(!output.status.success(), "{mode}");
        assert!(output.stdout.is_empty());
        if mode == "fail" {
            assert!(String::from_utf8_lossy(&output.stderr).contains("fixture permission denied"));
        }
        assert!(output.stderr.len() < 20000, "diagnostics were not bounded");
    }
}
#[test]
fn failing_error_handler_terminates_instead_of_recursively_retrying() {
    let dir = fixture();
    let source=TRACKER.replace("whenever a command fails\n    show the error in status","whenever a command fails\n    run gh.repo with repository repository\n    then show the result in status");
    let output = invoke(dir.path(), &source, "fail", &["--headless"]);
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}
#[test]
fn failure_recovery_through_a_button_cannot_recursively_retry_itself() {
    let dir = fixture();
    let source = TRACKER.replace(
        "whenever a command fails\n    show the error in status",
        "whenever a command fails\n    press refresh",
    );
    let output = invoke(dir.path(), &source, "fail", &["--headless"]);
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}
#[test]
fn removing_initial_refresh_from_program_prevents_the_gh_call() {
    let dir = fixture();
    let source = TRACKER.replace("  whenever repository arrives\n    press refresh\n\n", "");
    let output = invoke(dir.path(), &source, "normal", &["--headless"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["completed_jobs"], 0);
    assert!(data["programs"]["tracker"]["widgets"]["history"]["series"]
        .as_object()
        .unwrap()
        .is_empty());
    assert!(!dir.path().join("calls.jsonl").exists());
}
#[test]
fn invalid_shortcuts_and_button_cycles_fail_before_launching_gh() {
    let dir = fixture();
    for source in [
        TRACKER.replace("Ctrl-R", "Ctrl-C"),
        TRACKER.replace("Ctrl-R", "Ctrl-Q"),
        TRACKER.replace("Ctrl-R", "F13"),
        TRACKER.replace("press refresh", "press status"),
        TRACKER.replace(
            "  whenever refresh is pressed\n",
            "  whenever refresh is pressed\n    press refresh\n",
        ),
        TRACKER.replace(
            "close the screen",
            "close the screen\n    show \"later\" in repo",
        ),
    ] {
        let output = invoke(dir.path(), &source, "normal", &["--headless"]);
        assert!(!output.status.success(), "accepted {source}");
    }
    assert!(!dir.path().join("calls.jsonl").exists());
}
#[test]
fn terminal_interaction_restoration_and_worker_cancellation() {
    let output = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui_pty.py"))
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
fn headless_rejects_run_limit_before_starting_workers() {
    let dir = fixture();
    let output = invoke(
        dir.path(),
        TRACKER,
        "normal",
        &["--headless", "--max-runs", "1"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be combined"));
    assert!(!dir.path().join("calls.jsonl").exists());
}

#[test]
fn static_text_contract_failures_prevent_preceding_commands() {
    let dir = fixture();
    let base = "open a screen titled \"bounds\"\nplace app on the left\ndefine a program named app\n  offer message as text\n  show a text field named query with value \"ok\"\n  whenever the program opens\n    run gh.repo with repository \"cli/cli\"\n";
    for (value, action) in [
        ("x".repeat(65537), "send VALUE as message"),
        ("bad\0value".into(), "send VALUE as message"),
        ("x".repeat(513), "show VALUE in query"),
        ("bad\0value".into(), "show VALUE in query"),
    ] {
        let source = format!(
            "{base}    then {}\n",
            action.replace("VALUE", &serde_json::to_string(&value).unwrap())
        );
        for args in [vec!["check"], vec!["--headless"]] {
            let output = invoke(dir.path(), &source, "normal", &args);
            assert!(!output.status.success(), "accepted invalid text contract");
        }
    }
    assert!(!dir.path().join("calls.jsonl").exists());
}

#[test]
fn headless_reports_missing_inputs_without_partial_handler_effects() {
    let dir = fixture();
    let source = "open a screen titled \"inputs\"\nplace app on the left\ndefine a program named app\n  accept required as text\n  show a text panel named status\n  whenever the program opens\n    run gh.repo with repository \"cli/cli\"\n    then printf with message required\n    then show the result in status\n";
    let output = invoke(dir.path(), source, "normal", &["--headless"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("waiting for required inputs: app.required"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dir.path().join("calls.jsonl").exists());
}

#[test]
fn result_projections_share_one_command_result_and_keep_legacy_json_syntax() {
    for spelling in ["show the result as rows", "show JSON as rows"] {
        let dir = fixture();
        let output = invoke(
            dir.path(),
            &TRACKER.replace("show the result as rows", spelling),
            "normal",
            &["--headless"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let widgets = &data["programs"]["tracker"]["widgets"];
        assert_eq!(widgets["metrics"]["rows"][0]["stargazerCount"], 101);
        assert_eq!(widgets["history"]["series"]["morluto/rea"][0][1], 101);
        assert_eq!(widgets["description"]["text"], "Fixture description");
        assert_eq!(
            fs::read_to_string(dir.path().join("calls.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}
