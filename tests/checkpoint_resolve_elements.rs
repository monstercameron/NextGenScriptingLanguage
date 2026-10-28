//! Per-item uncertain-effect resolutions for issue #536. A `for each` loop of
//! three uncertain elements is killed mid-run; the operator resolves element 0 as
//! committed (with output) and element 1 as retry. Resume must rerun only
//! element 1 and then refuse on element 2, naming it. Negative cases cover the
//! fail-closed refusals. Element indices are the 0-based indices used by the
//! `for each element N` diagnostics.
//!
//! The kill is deterministic, not timed: the test spawns the run, polls until
//! the element log shows the first begin record, and only then kills the child.
//! The test fails if the run finishes before the poll observes a begin.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread;
use std::time::{Duration, Instant};

fn execute_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_execute"))
}

/// Run `execute` with `arguments` in `directory`, capturing both streams.
fn run(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(execute_binary())
        .args(arguments)
        .current_dir(directory)
        .output()
        .expect("start execute")
}

/// The element body: reads the file named by the element and appends its text
/// to `log.txt`, so each committed element leaves one observable line.
fn write_step(directory: &Path) {
    fs::write(
        directory.join("step.program"),
        "accept word as text from argument 1\nread word\nthen append to \"log.txt\"\n",
    )
    .unwrap();
}

fn outer_program() -> String {
    let execute_path = execute_binary().to_string_lossy().replace('\\', "/");
    format!(
        "for each item in [\"a\", \"b\", \"c\"]\n  run \"{execute_path}\" with arguments [\"run\", \"step.program\", \"--\", item]\n"
    )
}

fn log_lines(directory: &Path) -> Vec<String> {
    fs::read_to_string(directory.join("log.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn checkpoint_arguments(checkpoint: &Path, extra: &[&str]) -> Vec<String> {
    let mut arguments = vec![
        "checkpoint".to_owned(),
        "resolve".to_owned(),
        checkpoint.to_string_lossy().into_owned(),
    ];
    arguments.extend(extra.iter().map(|item| item.to_string()));
    arguments
}

fn resolve(directory: &Path, checkpoint: &Path, extra: &[&str]) -> Output {
    let arguments = checkpoint_arguments(checkpoint, extra);
    let owned: Vec<&str> = arguments.iter().map(String::as_str).collect();
    run(directory, &owned)
}

fn status_json(directory: &Path, checkpoint: &Path) -> Value {
    let text = checkpoint.to_string_lossy().into_owned();
    let output = run(directory, &["checkpoint", "status", &text, "--json"]);
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("--json prints one object")
}

/// Spawn a checkpointed run of the three-element loop and kill it once the
/// element log proves the first element began. Returns once the child exits.
fn kill_during_first_element(directory: &Path, checkpoint_name: &str) {
    let mut child: Child = Command::new(execute_binary())
        .args(["run", "outer.program", "--checkpoint", checkpoint_name])
        .current_dir(directory)
        .spawn()
        .expect("spawn execute");
    let log = directory.join(checkpoint_name).join("elements.jsonl");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut saw_begin = false;
    while Instant::now() < deadline {
        if let Ok(bytes) = fs::read(&log) {
            if bytes.windows(7).any(|window| window == b"\"begin\"") {
                saw_begin = true;
                break;
            }
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(saw_begin, "the run finished before the kill could land");
    // The kill must land before the first commit: nothing ran twice yet and
    // the first element awaits a decision.
    let bytes = fs::read(&log).expect("element log exists after kill");
    assert!(
        !bytes.windows(8).any(|window| window == b"\"commit\""),
        "the kill landed after the first commit"
    );
}

fn setup_three_element_loop(directory: &Path) {
    write_step(directory);
    for (name, text) in [("a", "A\n"), ("b", "B\n"), ("c", "C\n")] {
        fs::write(directory.join(name), text).unwrap();
    }
    fs::write(directory.join("outer.program"), outer_program()).unwrap();
}

#[test]
fn killed_loop_resolves_committed_then_retry_and_names_the_next_element() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");

    kill_during_first_element(directory.path(), "ck");
    assert_eq!(log_lines(directory.path()), Vec::<String>::new());

    // Plain resume refuses the interrupted side effect.
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let refused = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("interrupted side effect"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    // Status names the begun-but-uncommitted first element with its step id.
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    assert_eq!(summary["resolutions"].as_array().unwrap().len(), 0);
    let unresolved = summary["unresolved_uncertain"].as_array().unwrap();
    assert_eq!(unresolved.len(), 1, "{summary}");
    assert_eq!(unresolved[0]["node"], step_id);
    assert_eq!(unresolved[0]["element"], 0);
    assert_eq!(summary["resume_action"], "requires_retry_uncertain");

    // Negative cases fail closed before anything is recorded.
    let no_node = resolve(directory.path(), &checkpoint, &["--as", "retry"]);
    assert!(!no_node.status.success());
    let unknown_node = resolve(
        directory.path(),
        &checkpoint,
        &["--node", "no-such-step", "--as", "retry"],
    );
    assert!(!unknown_node.status.success());
    let retry_with_output = resolve(
        directory.path(),
        &checkpoint,
        &[
            "--node",
            &step_id,
            "--element",
            "1",
            "--as",
            "retry",
            "--output",
            "out.bin",
        ],
    );
    assert!(!retry_with_output.status.success());
    let committed_without_output = resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "0", "--as", "committed"],
    );
    assert!(!committed_without_output.status.success());
    let committed_future_element = resolve(
        directory.path(),
        &checkpoint,
        &[
            "--node",
            &step_id,
            "--element",
            "2",
            "--as",
            "committed",
            "--accept-empty-output",
            "--accept-empty-bindings",
        ],
    );
    assert!(
        !committed_future_element.status.success(),
        "a committed decision for an element that has not begun carries no fingerprint"
    );
    let node_level_on_element_node = resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--as", "retry"],
    );
    assert!(
        !node_level_on_element_node.status.success(),
        "element nodes take only per-element records"
    );
    assert!(
        String::from_utf8_lossy(&node_level_on_element_node.stderr)
            .contains("not valid for for-each"),
        "stderr: {}",
        String::from_utf8_lossy(&node_level_on_element_node.stderr)
    );
    // The resolutions log is append-only and the negatives above recorded
    // nothing, but this test uses a second checkpoint for the main flow so a
    // regression in the negatives cannot pollute the committed/retry path.
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();

    // Resolve element 0 as committed with output, element 1 as retry.
    fs::write(directory.path().join("verified.out"), "VERIFIED\n").unwrap();
    let committed = resolve(
        directory.path(),
        &checkpoint,
        &[
            "--node",
            &step_id,
            "--element",
            "0",
            "--as",
            "committed",
            "--output",
            "verified.out",
            "--accept-empty-bindings",
            "--reason",
            "element 0 wrote A, verified in log.txt",
        ],
    );
    assert!(
        committed.status.success(),
        "resolve committed failed: {}",
        String::from_utf8_lossy(&committed.stderr)
    );
    let retry = resolve(
        directory.path(),
        &checkpoint,
        &[
            "--node",
            &step_id,
            "--element",
            "1",
            "--as",
            "retry",
            "--reason",
            "element 1 never ran",
        ],
    );
    assert!(
        retry.status.success(),
        "resolve retry failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    // A second decision for the same element is refused: never two effects.
    let duplicate = resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "0", "--as", "retry"],
    );
    assert!(!duplicate.status.success());

    // Status shows both decisions with their reasons.
    let summary = status_json(directory.path(), &checkpoint);
    let resolutions = summary["resolutions"].as_array().unwrap();
    assert_eq!(resolutions.len(), 2, "{summary}");
    assert!(resolutions.iter().any(|record| {
        record["node"] == step_id
            && record["element"] == 0
            && record["decision"] == "committed"
            && record["reason"] == "element 0 wrote A, verified in log.txt"
    }));
    assert!(resolutions.iter().any(|record| {
        record["node"] == step_id && record["element"] == 1 && record["decision"] == "retry"
    }));

    // Resume reruns only element 1, then refuses on element 2 by name: the
    // committed element is never re-executed from its resolution.
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let resumed = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(!resumed.status.success());
    let stderr = String::from_utf8_lossy(&resumed.stderr).into_owned();
    assert!(
        stderr.contains("element 2"),
        "resume must name element 2, stderr: {stderr}"
    );
    // Element 0's append ran zero times (its output came from the resolution),
    // element 1's append ran exactly once.
    assert_eq!(log_lines(directory.path()), vec!["B"]);

    // Authorizing element 2 lets the same resume finish without rerunning 0 or 1.
    let retry_two = resolve(
        directory.path(),
        &checkpoint,
        &[
            "--node",
            &step_id,
            "--element",
            "2",
            "--as",
            "retry",
            "--reason",
            "authorized after inspection",
        ],
    );
    assert!(retry_two.status.success());
    let finished = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(
        finished.status.success(),
        "final resume failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert_eq!(log_lines(directory.path()), vec!["B", "C"]);

    // Committed output and bindings are stored in the resolutions log as hex
    // and JSON so resume replays exactly what the operator verified.
    let log = fs::read_to_string(checkpoint.join("resolutions.jsonl")).unwrap();
    let committed_line = log
        .lines()
        .find(|line| line.contains("\"element\":0") && line.contains("\"committed\""))
        .expect("committed element 0 is recorded");
    let stored: Value = serde_json::from_str(committed_line).unwrap();
    let hex = stored["output"].as_str().expect("committed carries output");
    assert_eq!(hex, "56455249464945440a", "VERIFIED\\n hex-encodes");
    assert!(
        stored["fingerprint"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "committed elements carry the begin fingerprint"
    );
    assert!(
        stored["actor"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "resolutions record who decided"
    );
}

/// A crafted parallel checkpoint refuses `committed`: the record cannot
/// reproduce a parallel result's value, stderr and outcome, so only `retry`
/// is valid there. The journal below is the smallest `resolve` accepts: a
/// real directory, a journal with an in-flight parallel node, and no element
/// log needed because the kind marker decides.
#[test]
fn parallel_elements_refuse_committed() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("ck");
    fs::create_dir_all(&checkpoint).unwrap();
    let journal = serde_json::json!({
        "checksum": "x",
        "data": {
            "kind": "finite",
            "identity": { "mode": "finite" },
            "status": "failed",
            "completed": [],
            "in_flight": {
                "line": 1,
                "start": 0,
                "end": 1,
                "retry_safe": false,
                "step_id": "step:parallel#1",
                "element_kind": "parallel_for_each",
            },
            "error": "boom",
        }
    });
    fs::write(
        checkpoint.join("journal.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    fs::write(directory.path().join("out.bin"), "X").unwrap();
    let step_id = "step:parallel#1";
    let refused = resolve(
        directory.path(),
        &checkpoint,
        &[
            "--node",
            step_id,
            "--element",
            "0",
            "--as",
            "committed",
            "--output",
            "out.bin",
            "--accept-empty-bindings",
        ],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("parallel"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    // Retry on the same parallel element passes validation (no begun element
    // to authorize yet, so it records a future retry).
    let retry = resolve(
        directory.path(),
        &checkpoint,
        &["--node", step_id, "--element", "0", "--as", "retry"],
    );
    assert!(
        retry.status.success(),
        "parallel retry failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
}

/// A torn resolutions log fails closed: both `resolve` and `status` refuse
/// instead of deciding against a partial view.
#[test]
fn torn_resolutions_log_refuses_resolve_and_status() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    fs::write(
        checkpoint.join("resolutions.jsonl"),
        b"{\"version\":1}\nnot-json-without-newline",
    )
    .unwrap();
    // Status itself must fail on the torn log rather than report zero
    // resolutions; run it directly to observe the refusal.
    let text = checkpoint.to_string_lossy().into_owned();
    let status = run(directory.path(), &["checkpoint", "status", &text, "--json"]);
    assert!(!status.status.success());
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("incomplete")
            || String::from_utf8_lossy(&status.stdout).is_empty(),
        "torn log must not read as empty"
    );
    let refused = resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "0", "--as", "retry"],
    );
    assert!(!refused.status.success());
}
