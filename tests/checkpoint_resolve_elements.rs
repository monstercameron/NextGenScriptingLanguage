//! Per-item uncertain-effect resolutions for issue #536. A `for each` loop of
//! three uncertain elements is killed mid-run; the operator resolves element 0 as
//! committed (with output) and element 1 as retry. Resume must rerun only
//! element 1 and then refuse on element 2, naming it. A parallel group test
//! resolves its held element as retry and resumes without the bulk flag,
//! rerunning only that element. Negative cases cover the fail-closed refusals.
//! Element indices are the 0-based indices used by the `for each element N`
//! diagnostics.
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

/// Regression for #536: a retry for an element that already began (its begin
/// record carries a fingerprint) still persists with no fingerprint, so
/// `resolve` succeeds and resume reruns exactly that element. The duplicate
/// is refused, so one authorization never becomes two runs of one effect.
#[test]
fn begun_element_retry_persists_without_fingerprint_and_reruns_once() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    // Element 0 began before the kill, so its pending entry carries a begin
    // fingerprint. Recording a retry must still succeed: a retry authorizes
    // one more run and carries no output, bindings, or fingerprint.
    let retry = resolve(
        directory.path(),
        &checkpoint,
        &[
            "--node",
            &step_id,
            "--element",
            "0",
            "--as",
            "retry",
            "--reason",
            "element 0 began before the kill, safe to rerun",
        ],
    );
    assert!(
        retry.status.success(),
        "begun-element retry failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let log = fs::read_to_string(checkpoint.join("resolutions.jsonl")).unwrap();
    let stored: Value = log
        .lines()
        .find(|line| line.contains("\"element\":0") && line.contains("\"retry\""))
        .map(|line| serde_json::from_str(line).expect("retry line is JSON"))
        .expect("begun-element retry is recorded");
    assert!(
        stored.get("fingerprint").is_none(),
        "a retry carries no fingerprint, stored: {stored}"
    );
    // One authorization is one run: a second record for the same element is
    // refused, never a second effect.
    let duplicate = resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "0", "--as", "retry"],
    );
    assert!(!duplicate.status.success());
    // Resume reruns exactly the resolved element, then names the next
    // uncovered uncertain element instead of replaying anything as complete.
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let resumed = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(!resumed.status.success());
    let stderr = String::from_utf8_lossy(&resumed.stderr).into_owned();
    assert!(
        stderr.contains("element 1"),
        "resume must name element 1, stderr: {stderr}"
    );
    assert_eq!(log_lines(directory.path()), vec!["A"]);
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

/// A held checkpoint lock fails `resolve` closed and reports `locked` status.
/// The holder keeps the same `lock` file the journal holds for the whole
/// resume, so this proves resolve-vs-resume mutual exclusion end-to-end.
#[cfg(unix)]
#[test]
fn locked_checkpoint_refuses_resolve_and_reports_locked() {
    use std::os::unix::io::AsRawFd;
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let lock_path = checkpoint.join("lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("lock file exists after a checkpointed run");
    // SAFETY: lock_file owns a live descriptor for the duration of this call.
    let held = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(held, 0, "test must hold the checkpoint lock");
    let summary = status_json(directory.path(), &checkpoint);
    assert_eq!(summary["lock_held"], true);
    assert_eq!(summary["resume_action"], "locked");
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    let refused = resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "1", "--as", "retry"],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("locked"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

/// A committed element whose list entry changed is refused, and a finished
/// resume is idempotent: re-running it restores the result without rerunning.
#[test]
fn committed_list_change_is_refused_and_rerun_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    fs::write(directory.path().join("verified.out"), "VERIFIED\n").unwrap();
    assert!(resolve(
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
            "element 0 verified",
        ],
    )
    .status
    .success());
    assert!(resolve(
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
            "element 1 never ran"
        ],
    )
    .status
    .success());
    // Only committed entries are pinned: rewrite the committed entry for
    // element 0. (Editing a never-committed entry cannot trip this refusal;
    // there is no commit to compare it against.)
    fs::write(
        directory.path().join("outer.program"),
        outer_program().replace("[\"a\", \"b\", \"c\"]", "[\"CHANGED\", \"b\", \"c\"]"),
    )
    .unwrap();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let changed = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(!changed.status.success());
    let stderr = String::from_utf8_lossy(&changed.stderr).into_owned();
    assert!(stderr.contains("list changed"), "stderr: {stderr}");
    assert!(
        stderr.contains("element 0"),
        "resume must name element 0, stderr: {stderr}"
    );
    fs::write(directory.path().join("outer.program"), outer_program()).unwrap();
    assert!(resolve(
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
            "authorized"
        ],
    )
    .status
    .success());
    let finished = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(
        finished.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert_eq!(log_lines(directory.path()), vec!["B", "C"]);
    let again = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(
        again.status.success(),
        "second resume failed: {}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(
        log_lines(directory.path()),
        vec!["B", "C"],
        "re-resume must not rerun elements"
    );
}

/// Editing an entry that was never committed fails closed without rerunning
/// committed work. The edit orphans the pre-edit resolutions for the changed
/// loop, so resume must refuse instead of running an uncertain element
/// silently; the committed element is never re-executed, and restoring the
/// program makes the checkpoint usable again.
#[test]
fn edited_uncommitted_entry_fails_closed_without_rerunning_committed() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    fs::write(directory.path().join("verified.out"), "VERIFIED\n").unwrap();
    assert!(resolve(
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
            "element 0 verified",
        ],
    )
    .status
    .success());
    assert!(resolve(
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
            "element 1 never ran"
        ],
    )
    .status
    .success());
    // Rewrite only the never-committed entry for element 2: no commit covers
    // it, so this is not a "list changed" refusal, but resume must still fail
    // closed rather than run element 1 against the edited loop.
    fs::write(
        directory.path().join("outer.program"),
        outer_program().replace("[\"a\", \"b\", \"c\"]", "[\"a\", \"b\", \"CHANGED\"]"),
    )
    .unwrap();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let edited = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(!edited.status.success());
    // Nothing ran: element 1 was refused before its body, and the committed
    // element 0 was never re-executed (its effect would append "A").
    assert_eq!(log_lines(directory.path()), Vec::<String>::new());
    // Restoring the program makes the same checkpoint resumable again.
    fs::write(directory.path().join("outer.program"), outer_program()).unwrap();
    assert!(resolve(
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
            "authorized"
        ],
    )
    .status
    .success());
    let finished = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(
        finished.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert_eq!(log_lines(directory.path()), vec!["B", "C"]);
}

/// A parallel group killed while one element is held resumes through a
/// per-element `retry` record, without the bulk `--retry-uncertain` flag and
/// without rerunning the elements that already committed. Each element counts
/// its own runs, so a re-run shows up as a count above one.
#[cfg(feature = "test-fixtures")]
#[test]
fn parallel_retry_resume_reruns_only_the_resolved_element() {
    use std::process::Stdio;

    fn fixture() -> String {
        PathBuf::from(env!("CARGO_BIN_EXE_native-fixture"))
            .to_string_lossy()
            .replace('\\', "/")
    }
    fn commit_lines(elements: &Path) -> usize {
        fs::read_to_string(elements)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("\"kind\":\"commit\""))
            .count()
    }
    fn element_count(root: &Path, element: &str) -> u32 {
        fs::read_to_string(root.join("counters").join(element))
            .unwrap_or_else(|_| "0".into())
            .trim()
            .parse()
            .unwrap()
    }

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir(root.join("counters")).unwrap();
    fs::create_dir(root.join("gates")).unwrap();
    // Element "c" waits on its gate, so it is still running when the runner dies.
    fs::write(root.join("gates").join("c.hold"), b"").unwrap();
    fs::write(
        root.join("group.program"),
        format!(
            "parallel for each item in [\"a\", \"b\", \"c\", \"d\", \"e\"] at most 2 as results\n  run \"{}\" with arguments [\"count-wait\", \"counters\", item, \"gates\"]\n",
            fixture()
        ),
    )
    .unwrap();

    let elements = root.join("ck").join("elements.jsonl");
    // Stderr goes to a file, not a pipe: a deadlocked pipe would hide the
    // very early exit this log exists to diagnose, and a file never blocks.
    let stderr_log = fs::File::create(root.join("first.stderr")).unwrap();
    let mut first = Command::new(execute_binary())
        .args(["run", "group.program", "--checkpoint", "ck"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .expect("spawn parallel group");
    // With two slots, the other four elements commit while "c" waits. The run
    // must still be alive at that point: an early exit means the fixture
    // never engaged, and the counters below would misreport that as a
    // scheduling surprise, so surface the exit status and stderr instead.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let commits = commit_lines(&elements);
        if commits >= 4 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for four committed parallel elements (saw {commits})"
        );
        if let Some(status) = first.try_wait().expect("poll parallel group") {
            let _ = first.wait();
            let logged = fs::read_to_string(root.join("first.stderr")).unwrap_or_default();
            panic!(
                "parallel group exited early with {status} after {commits} commits; stderr: {logged}"
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = first.kill();
    let _ = first.wait();
    // Observed commits prove their fixtures exited: each fixture increments
    // its counter before exiting, and a commit is logged only after, so these
    // four counters are durable by the time the log is polled.
    assert_eq!(element_count(root, "a"), 1);
    assert_eq!(element_count(root, "b"), 1);
    assert_eq!(element_count(root, "d"), 1);
    assert_eq!(element_count(root, "e"), 1);
    // The held element's increment races the kill (its begin record is
    // synchronous, its fixture startup is not), so allow it a moment to land
    // rather than flaking on scheduling.
    let started = Instant::now() + Duration::from_secs(30);
    while element_count(root, "c") != 1 {
        assert!(
            Instant::now() < started,
            "held element c never started before the kill"
        );
        thread::sleep(Duration::from_millis(10));
    }

    // Release the gate. The held element's begin record is uncertain, so a
    // plain resume refuses it by name instead of rerunning it silently.
    fs::remove_file(root.join("gates").join("c.hold")).unwrap();
    let checkpoint = root.join("ck");
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let refused = run(
        root,
        &["resume", "group.program", "--checkpoint", &checkpoint_text],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("element 2"),
        "resume must name element 2, stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    // The pending uncertain element is "c" at index 2; a per-element retry
    // authorizes exactly that element to run again. `committed` stays refused
    // for parallel elements (see `parallel_elements_refuse_committed`).
    let summary = status_json(root, &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    assert!(
        summary["unresolved_uncertain"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["element"] == 2)),
        "status must list element 2 as unresolved, summary: {summary}"
    );
    let retry = resolve(
        root,
        &checkpoint,
        &[
            "--node",
            &step_id,
            "--element",
            "2",
            "--as",
            "retry",
            "--reason",
            "element c held at kill, safe to rerun",
        ],
    );
    assert!(
        retry.status.success(),
        "parallel retry failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );

    // Resume without the bulk flag finishes: the four committed elements are
    // restored from the log and only the resolved element runs again.
    let resumed = run(
        root,
        &["resume", "group.program", "--checkpoint", &checkpoint_text],
    );
    assert!(
        resumed.status.success(),
        "parallel resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    for element in ["a", "b", "d", "e"] {
        assert_eq!(element_count(root, element), 1, "element {element} re-ran");
    }
    assert_eq!(element_count(root, "c"), 2);

    // Re-resume is idempotent: a finished parallel group restores without
    // rerunning any element.
    let again = run(
        root,
        &["resume", "group.program", "--checkpoint", &checkpoint_text],
    );
    assert!(
        again.status.success(),
        "parallel re-resume failed: {}",
        String::from_utf8_lossy(&again.stderr)
    );
    for element in ["a", "b", "d", "e"] {
        assert_eq!(element_count(root, element), 1, "element {element} re-ran");
    }
    assert_eq!(element_count(root, "c"), 2);
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

/// Removing only the torn trailing bytes restores the log: the next resolve
/// records and status lists every valid record again.
#[test]
fn torn_trailing_bytes_repair_restores_resolve() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    assert!(resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "0", "--as", "retry"],
    )
    .status
    .success());
    let valid = fs::read(checkpoint.join("resolutions.jsonl")).unwrap();
    fs::write(
        checkpoint.join("resolutions.jsonl"),
        [valid.as_slice(), b"not-json-without-newline"].concat(),
    )
    .unwrap();
    let refused = resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "1", "--as", "retry"],
    );
    assert!(!refused.status.success());
    // Documented repair: truncate only the torn tail, keep the file 0600,
    // re-run resolve one item at a time.
    fs::write(checkpoint.join("resolutions.jsonl"), &valid).unwrap();
    assert!(resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "1", "--as", "retry"],
    )
    .status
    .success());
    let summary = status_json(directory.path(), &checkpoint);
    assert_eq!(summary["resolutions"].as_array().unwrap().len(), 2);
}

/// Removing only the duplicate line restores the log: status reads the one
/// kept record and the next element resolves.
#[test]
fn duplicate_line_repair_restores_status() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    assert!(resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "0", "--as", "retry"],
    )
    .status
    .success());
    let first = fs::read(checkpoint.join("resolutions.jsonl")).unwrap();
    fs::write(
        checkpoint.join("resolutions.jsonl"),
        [first.as_slice(), first.as_slice()].concat(),
    )
    .unwrap();
    let text = checkpoint.to_string_lossy().into_owned();
    let status = run(directory.path(), &["checkpoint", "status", &text, "--json"]);
    assert!(!status.status.success());
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("duplicate"),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    // Documented repair: keep only the first occurrence.
    fs::write(checkpoint.join("resolutions.jsonl"), &first).unwrap();
    let summary = status_json(directory.path(), &checkpoint);
    assert_eq!(summary["resolutions"].as_array().unwrap().len(), 1);
    assert!(resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "1", "--as", "retry"],
    )
    .status
    .success());
}

/// A hand-written sequential `committed` with a forged fingerprint is refused
/// at resume: it names element 0 and the list entry it was verified against.
#[test]
fn forged_sequential_fingerprint_is_refused_at_resume() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    let forged = serde_json::json!({
        "version": 1,
        "node": step_id,
        "element": 0,
        "decision": "committed",
        "reason": "forged",
        "actor": "unverified:forged",
        "at": 1,
        "output": "56455249464945440a",
        "bindings": {},
        "fingerprint": "forged-fingerprint",
    });
    fs::write(
        checkpoint.join("resolutions.jsonl"),
        format!("{}\n", serde_json::to_string(&forged).unwrap()),
    )
    .unwrap();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let resumed = run(
        directory.path(),
        &["resume", "outer.program", "--checkpoint", &checkpoint_text],
    );
    assert!(!resumed.status.success());
    let stderr = String::from_utf8_lossy(&resumed.stderr).into_owned();
    assert!(
        stderr.contains("list changed") && stderr.contains("element 0"),
        "forged fingerprint must name element 0, stderr: {stderr}"
    );
    assert_eq!(log_lines(directory.path()), Vec::<String>::new());
}

/// A hand-written parallel `committed` bypasses the resolve-time refusal but
/// resume still refuses it: parallel results cannot be replayed from a record.
/// Built from a real killed group so the element log holds the pending entry
/// the resume-time check needs.
#[cfg(feature = "test-fixtures")]
#[test]
fn forged_parallel_committed_is_refused_at_resume() {
    use std::process::Stdio;

    fn commit_lines(elements: &Path) -> usize {
        fs::read_to_string(elements)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("\"kind\":\"commit\""))
            .count()
    }

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir(root.join("counters")).unwrap();
    fs::create_dir(root.join("gates")).unwrap();
    fs::write(root.join("gates").join("c.hold"), b"").unwrap();
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_native-fixture"))
        .to_string_lossy()
        .replace('\\', "/");
    fs::write(
        root.join("group.program"),
        format!(
            "parallel for each item in [\"a\", \"b\", \"c\", \"d\", \"e\"] at most 2 as results\n  run \"{fixture}\" with arguments [\"count-wait\", \"counters\", item, \"gates\"]\n",
        ),
    )
    .unwrap();

    let elements = root.join("ck").join("elements.jsonl");
    let stderr_log = fs::File::create(root.join("first.stderr")).unwrap();
    let mut first = Command::new(execute_binary())
        .args(["run", "group.program", "--checkpoint", "ck"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .expect("spawn parallel group");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if commit_lines(&elements) >= 4 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for four committed parallel elements"
        );
        if let Some(status) = first.try_wait().expect("poll parallel group") {
            let _ = first.wait();
            let logged = fs::read_to_string(root.join("first.stderr")).unwrap_or_default();
            panic!("parallel group exited early with {status}; stderr: {logged}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = first.kill();
    let _ = first.wait();

    // Hand-write a `committed` record for the held element (index 2),
    // bypassing the resolve-time refusal for parallel elements. It is shaped
    // to pass log validation so the resume-time check is what refuses it.
    let checkpoint = root.join("ck");
    let summary = status_json(root, &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    let forged = serde_json::json!({
        "version": 1,
        "node": step_id,
        "element": 2,
        "decision": "committed",
        "reason": "forged",
        "actor": "unverified:forged",
        "at": 1,
        "output": "58",
        "bindings": {},
        "fingerprint": "forged",
    });
    fs::write(
        checkpoint.join("resolutions.jsonl"),
        format!("{}\n", serde_json::to_string(&forged).unwrap()),
    )
    .unwrap();
    fs::remove_file(root.join("gates").join("c.hold")).unwrap();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let resumed = run(
        root,
        &["resume", "group.program", "--checkpoint", &checkpoint_text],
    );
    assert!(!resumed.status.success());
    let stderr = String::from_utf8_lossy(&resumed.stderr).into_owned();
    assert!(
        stderr.contains("parallel") && stderr.contains("retry"),
        "forged parallel commit must direct to retry, stderr: {stderr}"
    );
}

/// A checkpoint that predates the version 6 element marker refuses both
/// node-level and per-element records at resolve time: without the marker a
/// plain node cannot be told from a loop killed before its first begin. The
/// refusal names the bulk `--retry-uncertain` flag as the safe path.
#[test]
fn pre_marking_journal_refuses_resolve_and_names_bulk_retry() {
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
                "step_id": "step:plain#1",
            },
            "error": "boom",
        }
    });
    fs::write(
        checkpoint.join("journal.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    let node_level = resolve(
        directory.path(),
        &checkpoint,
        &["--node", "step:plain#1", "--as", "retry"],
    );
    assert!(!node_level.status.success());
    assert!(
        String::from_utf8_lossy(&node_level.stderr).contains("predates element markers"),
        "stderr: {}",
        String::from_utf8_lossy(&node_level.stderr)
    );
    let element_level = resolve(
        directory.path(),
        &checkpoint,
        &["--node", "step:plain#1", "--element", "0", "--as", "retry"],
    );
    assert!(!element_level.status.success());
    assert!(
        String::from_utf8_lossy(&element_level.stderr).contains("--retry-uncertain"),
        "stderr: {}",
        String::from_utf8_lossy(&element_level.stderr)
    );
}

/// Bulk `--retry-uncertain` does not bypass the orphan guard: editing a
/// never-committed entry still fails closed, and after restoring the program
/// the bulk flag finishes the run without per-element records for the tail.
#[test]
fn bulk_retry_does_not_bypass_orphan_guard() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    fs::write(directory.path().join("verified.out"), "VERIFIED\n").unwrap();
    assert!(resolve(
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
            "element 0 verified",
        ],
    )
    .status
    .success());
    assert!(resolve(
        directory.path(),
        &checkpoint,
        &["--node", &step_id, "--element", "1", "--as", "retry"],
    )
    .status
    .success());
    fs::write(
        directory.path().join("outer.program"),
        outer_program().replace("[\"a\", \"b\", \"c\"]", "[\"a\", \"b\", \"CHANGED\"]"),
    )
    .unwrap();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let edited = run(
        directory.path(),
        &[
            "resume",
            "outer.program",
            "--checkpoint",
            &checkpoint_text,
            "--retry-uncertain",
        ],
    );
    assert!(!edited.status.success());
    assert!(
        String::from_utf8_lossy(&edited.stderr).contains("program changed"),
        "stderr: {}",
        String::from_utf8_lossy(&edited.stderr)
    );
    assert_eq!(log_lines(directory.path()), Vec::<String>::new());
    // Restoring the program makes the bulk flag the safe path for the
    // still-unresolved tail: no per-element record for element 2 needed.
    fs::write(directory.path().join("outer.program"), outer_program()).unwrap();
    let finished = run(
        directory.path(),
        &[
            "resume",
            "outer.program",
            "--checkpoint",
            &checkpoint_text,
            "--retry-uncertain",
        ],
    );
    assert!(
        finished.status.success(),
        "bulk resume failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert_eq!(log_lines(directory.path()), vec!["B", "C"]);
}

/// Status shows who, when and why but never the committed output, bindings,
/// or fingerprint: they stay in the 0600 checkpoint as resume evidence.
#[test]
fn status_hides_committed_output_bindings_and_fingerprint() {
    let directory = tempfile::tempdir().unwrap();
    setup_three_element_loop(directory.path());
    let checkpoint = directory.path().join("ck");
    kill_during_first_element(directory.path(), "ck");
    let summary = status_json(directory.path(), &checkpoint);
    let step_id = summary["journal"]["in_flight"]["step_id"]
        .as_str()
        .expect("status reports the in-flight step id")
        .to_owned();
    fs::write(directory.path().join("verified.out"), "VERIFIED\n").unwrap();
    assert!(resolve(
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
            "--binding",
            "count=3",
            "--reason",
            "element 0 verified",
        ],
    )
    .status
    .success());
    let summary = status_json(directory.path(), &checkpoint);
    let resolutions = summary["resolutions"].as_array().unwrap();
    assert_eq!(resolutions.len(), 1);
    assert_eq!(resolutions[0]["reason"], "element 0 verified");
    for key in ["output", "bindings", "fingerprint"] {
        assert!(
            resolutions[0].get(key).is_none(),
            "status must not leak {key}: {}",
            resolutions[0]
        );
    }
}

/// A live run holding the checkpoint lock makes `resolve` fail with `locked`
/// and status report `locked`. This is the cross-platform live-process half
/// of the locking contract (the `flock` unit test covers a manual holder on
/// Unix): it runs in CI on every platform, including Windows, and no Windows
/// runtime behavior beyond CI is claimed here.
#[cfg(feature = "test-fixtures")]
#[test]
fn live_run_holds_lock_against_resolve_and_status() {
    use std::process::Stdio;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir(root.join("counters")).unwrap();
    fs::create_dir(root.join("gates")).unwrap();
    fs::write(root.join("gates").join("c.hold"), b"").unwrap();
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_native-fixture"))
        .to_string_lossy()
        .replace('\\', "/");
    fs::write(
        root.join("group.program"),
        format!(
            "parallel for each item in [\"a\", \"b\", \"c\", \"d\", \"e\"] at most 2 as results\n  run \"{fixture}\" with arguments [\"count-wait\", \"counters\", item, \"gates\"]\n",
        ),
    )
    .unwrap();

    let mut child: Child = Command::new(execute_binary())
        .args(["run", "group.program", "--checkpoint", "ck"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn blocking group run");
    // Poll until the run holds the checkpoint lock with a readable journal
    // (or the run exits, which is itself a failure to set up the
    // contention).
    let checkpoint = root.join("ck");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if checkpoint.join("lock").exists() && checkpoint.join("journal.json").exists() {
            break;
        }
        if let Some(status) = child.try_wait().expect("poll blocking run") {
            panic!("blocking run exited early with {status} before holding the lock");
        }
        assert!(
            Instant::now() < deadline,
            "blocking run never created the checkpoint lock"
        );
        thread::sleep(Duration::from_millis(50));
    }
    // Capture both observations while the run still holds the lock, then
    // stop the run before asserting so a failure cannot leak the child.
    let summary = status_json(root, &checkpoint);
    let refused = resolve(
        root,
        &checkpoint,
        &["--node", "step:parallel#1", "--as", "retry"],
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(summary["lock_held"], true);
    assert_eq!(summary["resume_action"], "locked");
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("locked"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}
