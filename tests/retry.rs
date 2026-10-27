//! `run ... and retry up to N times`: declared retries for retryable failures, with
//! the retry-safety rules, bounded backoff, trace events, and explain output.
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn fixture() -> String {
    env!("CARGO_BIN_EXE_native-fixture").to_owned()
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A direct executable run that fails `failures` times with `status`, then succeeds.
fn direct(counter: &Path, failures: u32, status: i32, suffix: &str) -> String {
    format!(
        "run {} with arguments {} {suffix}\n",
        quoted(&fixture()),
        serde_json::to_string(&[
            "flaky",
            counter.to_str().unwrap(),
            &failures.to_string(),
            &status.to_string(),
        ])
        .unwrap()
    )
}

fn run(root: &Path, source: &str, extra: &[&str]) -> Output {
    fs::write(root.join("job.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg("job.program")
        .args(extra)
        .current_dir(root)
        .output()
        .unwrap()
}

fn count(counter: &Path) -> String {
    fs::read_to_string(counter).unwrap_or_default()
}

/// A contract applet over the fixture. `outcome` is the JSON for the status-75 outcome.
fn write_applet(manifests: &Path, name: &str, counter: &Path, outcome: Value) {
    let manifest = json!({
        "name": name,
        "binary": fixture(),
        "parameters": {},
        "arguments": [
            {"literal": "flaky"},
            {"literal": counter.to_str().unwrap()},
            {"literal": "1"},
            {"literal": "75"}
        ],
        "contract": {
            "version": 1,
            "backend": "command",
            "outcomes": [
                {"status": 0, "name": "ok", "kind": "success"},
                outcome
            ],
            "capabilities": [],
            "effects": []
        }
    });
    fs::write(
        manifests.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn manifests(root: &Path) -> PathBuf {
    let manifests = root.join("manifests");
    fs::create_dir_all(&manifests).unwrap();
    write_applet(
        &manifests,
        "flaky.safe",
        &root.join("safe.txt"),
        json!({"status": 75, "name": "busy", "kind": "retryable", "effect_certainty": "not_started"}),
    );
    write_applet(
        &manifests,
        "flaky.uncertain",
        &root.join("uncertain.txt"),
        json!({"status": 75, "name": "busy", "kind": "retryable"}),
    );
    write_applet(
        &manifests,
        "flaky.plain",
        &root.join("plain.txt"),
        json!({"status": 75, "name": "gone", "kind": "failure", "effect_certainty": "not_started"}),
    );
    manifests
}

#[test]
fn a_declared_retry_runs_a_direct_command_again_after_a_nonzero_exit() {
    let root = tempfile::tempdir().unwrap();
    let counter = root.path().join("count.txt");
    let source = direct(&counter, 1, 75, "and retry up to 2 times even if uncertain");
    let output = run(root.path(), &source, &["--trace"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(count(&counter), "2");
    let stderr = stderr_of(&output);
    assert_eq!(stderr.matches("\"event\":\"retry\"").count(), 1, "{stderr}");
    assert!(stderr.contains("\"attempt\":2"), "{stderr}");
}

#[test]
fn exhausted_retries_fail_after_every_attempt() {
    let root = tempfile::tempdir().unwrap();
    let counter = root.path().join("count.txt");
    let source = direct(
        &counter,
        10,
        75,
        "and retry up to 2 times even if uncertain",
    );
    let output = run(root.path(), &source, &[]);
    assert!(!output.status.success());
    assert_eq!(count(&counter), "3");
    assert!(
        stderr_of(&output).contains("retry exhausted after 3 attempts"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn an_uncertain_direct_command_is_not_retried_without_the_opt_in() {
    let root = tempfile::tempdir().unwrap();
    let counter = root.path().join("count.txt");
    let source = direct(&counter, 10, 75, "and retry up to 2 times");
    let output = run(root.path(), &source, &[]);
    assert!(!output.status.success());
    assert_eq!(count(&counter), "1", "a refused retry must not run again");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("retry refused"), "{stderr}");
    assert!(stderr.contains("even if uncertain"), "{stderr}");
}

#[test]
fn a_retryable_contract_outcome_with_known_no_effect_needs_no_opt_in() {
    let root = tempfile::tempdir().unwrap();
    let manifests = manifests(root.path());
    let source = "run flaky.safe and retry up to 2 times\n";
    let counter = root.path().join("safe.txt");
    fs::write(root.path().join("job.program"), source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg("job.program")
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(count(&counter), "2");
}

#[test]
fn a_retryable_outcome_with_uncertain_effects_is_refused_then_allowed_with_the_opt_in() {
    let root = tempfile::tempdir().unwrap();
    let manifests = manifests(root.path());
    let counter = root.path().join("uncertain.txt");
    let refused = run_with_manifests(
        root.path(),
        &manifests,
        "run flaky.uncertain and retry up to 2 times\n",
    );
    assert!(!refused.status.success());
    assert_eq!(count(&counter), "1");
    assert!(
        stderr_of(&refused).contains("retry refused"),
        "{}",
        stderr_of(&refused)
    );

    let _ = fs::remove_file(&counter);
    let allowed = run_with_manifests(
        root.path(),
        &manifests,
        "run flaky.uncertain and retry up to 2 times even if uncertain\n",
    );
    assert!(allowed.status.success(), "{}", stderr_of(&allowed));
    assert_eq!(count(&counter), "2");
}

#[test]
fn an_outcome_not_declared_retryable_is_never_retried() {
    let root = tempfile::tempdir().unwrap();
    let manifests = manifests(root.path());
    let counter = root.path().join("plain.txt");
    let output = run_with_manifests(
        root.path(),
        &manifests,
        "run flaky.plain and retry up to 2 times even if uncertain\n",
    );
    assert!(!output.status.success());
    assert_eq!(count(&counter), "1");
    assert!(
        stderr_of(&output).contains("not retryable"),
        "{}",
        stderr_of(&output)
    );
}

fn run_with_manifests(root: &Path, manifests: &Path, source: &str) -> Output {
    fs::write(root.join("job.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg("job.program")
        .arg("--manifest-dir")
        .arg(manifests)
        .current_dir(root)
        .output()
        .unwrap()
}

#[test]
fn backoff_waits_between_attempts_within_bounds() {
    let root = tempfile::tempdir().unwrap();
    let counter = root.path().join("count.txt");
    let source = direct(
        &counter,
        2,
        75,
        "and retry up to 2 times even if uncertain with backoff 200 milliseconds",
    );
    let started = Instant::now();
    let output = run(root.path(), &source, &[]);
    let elapsed = started.elapsed();
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(count(&counter), "3");
    // Two waits of 200 ms each: at least 400 ms, and well under a generous ceiling.
    assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(20), "{elapsed:?}");
}

#[test]
fn doubling_backoff_doubles_each_later_wait() {
    let root = tempfile::tempdir().unwrap();
    let counter = root.path().join("count.txt");
    let source = direct(
        &counter,
        2,
        75,
        "and retry up to 2 times even if uncertain with backoff 150 milliseconds doubling",
    );
    let started = Instant::now();
    let output = run(root.path(), &source, &[]);
    let elapsed = started.elapsed();
    assert!(output.status.success(), "{}", stderr_of(&output));
    // Waits of 150 ms then 300 ms: at least 450 ms in total.
    assert!(elapsed >= Duration::from_millis(450), "{elapsed:?}");
}

#[test]
fn invalid_retry_declarations_are_parse_errors() {
    let cases = [
        ("run flaky.safe and retry up to 0 times\n", "retry count"),
        ("run flaky.safe and retry up to 101 times\n", "retry count"),
        ("run flaky.safe and retry up to two times\n", "retry count"),
        (
            "run flaky.safe and retry up to 2 fortnights\n",
            "expected `times`",
        ),
        (
            "run flaky.safe and retry up to 2 times with backoff 2 fortnights\n",
            "unsupported backoff unit",
        ),
        (
            "run flaky.safe and retry up to 2 times with backoff 2000 hours\n",
            "at most one hour",
        ),
        (
            "run flaky.safe and retry up to 2 times with backoff 2 seconds extra\n",
            "extra",
        ),
        (
            "run flaky.safe and keep status as code and retry up to 2 times\n",
            "cannot be combined",
        ),
        (
            "run flaky.safe and retry up to 2 times and cache\n",
            "stray token",
        ),
        (
            "then flaky.safe and retry up to 2 times\n",
            "applies to a `run` command",
        ),
        (
            "pipe to flaky.safe and retry up to 2 times\n",
            "applies to a `run` command",
        ),
        (
            "if run flaky.safe and retry up to 2 times succeeds\n",
            "a command condition cannot use",
        ),
    ];
    for (source, expected) in cases {
        let error = flow::parser::parse(source).expect_err(source);
        let message = format!("{error:#}");
        assert!(message.contains(expected), "{source}: {message}");
    }
}

#[test]
fn a_valid_declaration_keeps_each_field() {
    let program = flow::parser::parse(
        "run flaky.safe and retry up to 3 times even if uncertain with backoff 2 seconds doubling\n",
    )
    .unwrap();
    let flow::ast::StepKind::Invoke { call, .. } = &program.steps[0].kind else {
        panic!("expected an invocation");
    };
    let policy = call.retry.expect("retry policy");
    assert_eq!(policy.retries, 3);
    assert!(policy.even_if_uncertain);
    assert_eq!(policy.backoff_ms, Some(2_000));
    assert!(policy.doubling);

    let plain = flow::parser::parse("run flaky.safe and retry up to 1 time\n").unwrap();
    let flow::ast::StepKind::Invoke { call, .. } = &plain.steps[0].kind else {
        panic!("expected an invocation");
    };
    let policy = call.retry.expect("retry policy");
    assert_eq!(
        (policy.retries, policy.even_if_uncertain, policy.backoff_ms),
        (1, false, None)
    );
}

#[test]
fn explain_shows_the_declared_retry_and_relative_called_files() {
    let root = tempfile::tempdir().unwrap();
    let manifests = manifests(root.path());
    fs::create_dir_all(root.path().join("lib")).unwrap();
    fs::write(
        root.path().join("lib/echo.program"),
        "accept who as text with default \"x\"\nuse value who\n",
    )
    .unwrap();
    fs::write(
        root.path().join("job.program"),
        "run flaky.safe and retry up to 3 times with backoff 2 seconds doubling\nrun program file \"lib/echo.program\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["explain", "job.program", "--json", "--manifest-dir"])
        .arg(&manifests)
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr_of(&output));
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(
        steps[0]["retry"],
        json!({
            "attempts": 4,
            "backoff_ms": 2000,
            "doubling": true,
            "even_if_uncertain": false,
            "idempotent": false,
        })
    );
    assert!(steps[1]["retry"].is_null());
    let call = steps
        .iter()
        .find(|step| step["kind"] == "call_program")
        .unwrap();
    assert_eq!(call["applet"], "file:lib/echo.program");
}
