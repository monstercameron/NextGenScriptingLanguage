//! Checkpoint resume regressions (lane RT3): declared inputs reach a first
//! top-level loop on resume, a lost per-user input key is reported without
//! creating a new one, and `try` / `on exit` blocks resume with their handlers.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn execute_path() -> String {
    env!("CARGO_BIN_EXE_execute").replace('\\', "/")
}

/// Keeps the per-user input key of every run in `key_home`, never the real one.
fn execute_command(args: &[&str], directory: &Path, key_home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .args(args)
        .current_dir(directory)
        .env("LOCALAPPDATA", key_home)
        .env("XDG_STATE_HOME", key_home);
    command
}

fn execute(args: &[&str], directory: &Path, key_home: &Path) -> Output {
    execute_command(args, directory, key_home).output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `step.program` appends the text of the file named by its argument to `log.txt`,
/// so a missing file fails exactly that element.
fn write_step(directory: &Path) {
    fs::write(
        directory.join("step.program"),
        "accept word as text from argument 1\nread word\nthen append to \"log.txt\"\n",
    )
    .unwrap();
}

fn element_call() -> String {
    format!(
        "run \"{}\" with arguments [\"run\", \"step.program\", \"--\", item]",
        execute_path()
    )
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| line.trim_end().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Run `program` with a checkpoint where element "b" fails, then supply "b" and
/// resume with the same extra arguments.
fn first_loop_resumes(program: &str, extra: &[&str]) {
    let directory = tempdir().unwrap();
    let keys = tempdir().unwrap();
    write_step(directory.path());
    fs::write(directory.path().join("a"), "A\n").unwrap();
    fs::write(directory.path().join("c"), "C\n").unwrap();
    fs::write(directory.path().join("p.program"), program).unwrap();
    let mut args = vec!["run", "p.program", "--checkpoint", "ck"];
    args.extend_from_slice(extra);
    let first = execute(&args, directory.path(), keys.path());
    assert_ne!(first.status.code(), Some(0), "{}", text(&first.stderr));
    assert_eq!(lines(&directory.path().join("log.txt")), vec!["A"]);

    fs::write(directory.path().join("b"), "B\n").unwrap();
    let mut args = vec![
        "resume",
        "p.program",
        "--checkpoint",
        "ck",
        "--retry-uncertain",
    ];
    args.extend_from_slice(extra);
    let resumed = execute(&args, directory.path(), keys.path());
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "stderr: {}",
        text(&resumed.stderr)
    );
    assert_eq!(
        lines(&directory.path().join("log.txt")),
        vec!["A", "B", "C"]
    );
}

#[test]
fn resume_restores_a_list_input_for_a_first_top_level_for_each() {
    first_loop_resumes(
        &format!(
            "accept items as list of text\nfor each item in items\n  {}\n",
            element_call()
        ),
        &["--input", "items=[\"a\",\"b\",\"c\"]"],
    );
}

#[test]
fn resume_restores_a_defaulted_input_for_a_first_top_level_for_each() {
    first_loop_resumes(
        &format!(
            "accept items as list of text with default [\"a\", \"b\", \"c\"]\nfor each item in items\n  {}\n",
            element_call()
        ),
        &[],
    );
}

#[test]
fn resume_restores_a_json_input_for_a_first_top_level_for_each() {
    first_loop_resumes(
        &format!(
            "accept items as json\nfor each entry in items\n  take text at \"\" from entry as item\n  {}\n",
            element_call()
        ),
        &["--input", "items=[\"a\",\"b\",\"c\"]"],
    );
}

#[test]
fn resume_restores_an_input_for_a_first_parallel_for_each() {
    // A parallel element that fails on its own does not fail the group, so the group
    // is stopped by the output limit instead: "b" starts out too large.
    let directory = tempdir().unwrap();
    let keys = tempdir().unwrap();
    write_step(directory.path());
    fs::write(directory.path().join("a"), "A\n").unwrap();
    fs::write(
        directory.path().join("b"),
        format!("{}\n", "x".repeat(5000)),
    )
    .unwrap();
    fs::write(directory.path().join("c"), "C\n").unwrap();
    fs::write(
        directory.path().join("p.program"),
        format!(
            "accept items as list of text\nparallel for each item in items at most 1 as results\n  {}\n",
            element_call()
        ),
    )
    .unwrap();
    let common = [
        "--checkpoint",
        "ck",
        "--input",
        "items=[\"a\",\"b\",\"c\"]",
        "--max-output-bytes",
        "4000",
    ];
    let mut args = vec!["run", "p.program"];
    args.extend_from_slice(&common);
    let first = execute(&args, directory.path(), keys.path());
    assert_ne!(first.status.code(), Some(0), "{}", text(&first.stderr));

    fs::write(directory.path().join("b"), "B\n").unwrap();
    let mut args = vec!["resume", "p.program", "--retry-uncertain"];
    args.extend_from_slice(&common);
    let resumed = execute(&args, directory.path(), keys.path());
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "stderr: {}",
        text(&resumed.stderr)
    );
    let log = lines(&directory.path().join("log.txt"));
    assert_eq!(log.iter().filter(|line| *line == "A").count(), 1, "{log:?}");
    assert_eq!(log[log.len() - 2..], ["B", "C"], "{log:?}");
}

#[test]
fn resume_with_a_lost_input_key_names_the_key_and_creates_none() {
    let directory = tempdir().unwrap();
    let keys = tempdir().unwrap();
    let empty_keys = tempdir().unwrap();
    write_step(directory.path());
    fs::write(
        directory.path().join("p.program"),
        format!(
            "accept item as text with default \"missing\"\n{}\n",
            element_call()
        ),
    )
    .unwrap();
    let first = execute(
        &["run", "p.program", "--checkpoint", "ck"],
        directory.path(),
        keys.path(),
    );
    assert_ne!(first.status.code(), Some(0));

    let resumed = execute(
        &["resume", "p.program", "--checkpoint", "ck"],
        directory.path(),
        empty_keys.path(),
    );
    assert_ne!(resumed.status.code(), Some(0));
    let stderr = text(&resumed.stderr);
    assert!(stderr.contains("what-if.key"), "{stderr}");
    assert!(stderr.contains("restore that key"), "{stderr}");
    assert!(
        !empty_keys
            .path()
            .join("execute")
            .join("what-if.key")
            .exists(),
        "resume must not create a replacement key"
    );
}

/// A direct shell call: `sh -c UNIX` on Unix, `cmd /C WINDOWS` on Windows.
fn shell(unix: &str, windows: &str) -> String {
    let (program, flag, script) = if cfg!(windows) {
        ("cmd", "/C", windows)
    } else {
        ("sh", "-c", unix)
    };
    format!(
        "run \"{program}\" with arguments [\"{flag}\", {}]",
        serde_json::to_string(script).unwrap()
    )
}

/// Append one word to `side.txt`.
fn mark(word: &str) -> String {
    shell(
        &format!("echo {word} >> side.txt"),
        &format!("echo {word}>>side.txt"),
    )
}

#[cfg(windows)]
/// Append `word`, sleep long enough to be killed, then append `word-done`.
fn mark_and_sleep(word: &str) -> String {
    shell(
        &format!("echo {word} >> side.txt; sleep 6; echo {word}-done >> side.txt"),
        &format!("echo {word}>>side.txt& ping -n 7 127.0.0.1 >nul & echo {word}-done>>side.txt"),
    )
}

/// Start a checkpointed run, wait until `side.txt` shows `word`, then kill it hard.
/// Windows only: killing the runtime there ends its commands' job objects, while on
/// Unix the sleeping command lives on in its own process group and keeps writing.
#[cfg(windows)]
fn run_and_kill_at(directory: &Path, keys: &Path, word: &str) {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut child = execute_command(&["run", "p.program", "--checkpoint", "ck"], directory, keys)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let side = directory.join("side.txt");
    let started = Instant::now();
    while !lines(&side).iter().any(|line| line == word) {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the run never reached {word}"
        );
        assert!(child.try_wait().unwrap().is_none(), "the run ended early");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    // Let a killed sleeper's process tree finish dying before the resume.
    std::thread::sleep(Duration::from_millis(300));
}

#[cfg(windows)]
#[test]
fn checkpointed_try_crash_reruns_the_block_and_its_handlers_once() {
    let directory = tempdir().unwrap();
    let keys = tempdir().unwrap();
    fs::write(
        directory.path().join("p.program"),
        format!(
            "{}\ntry\n  {}\n  {}\ncatch err\n  {}\nfinally\n  {}\n{}\n",
            mark("setup"),
            mark_and_sleep("body1"),
            mark("body2"),
            mark("caught"),
            mark("finally"),
            mark("end"),
        ),
    )
    .unwrap();
    run_and_kill_at(directory.path(), keys.path(), "body1");
    let resumed = execute(
        &[
            "resume",
            "p.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
        ],
        directory.path(),
        keys.path(),
    );
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed.stderr));
    assert_eq!(
        lines(&directory.path().join("side.txt")),
        vec![
            "setup",
            "body1",
            "body1",
            "body1-done",
            "body2",
            "finally",
            "end"
        ]
    );
}

#[cfg(windows)]
#[test]
fn checkpointed_on_exit_crash_resumes_inside_the_program_and_cleans_up_once() {
    let directory = tempdir().unwrap();
    let keys = tempdir().unwrap();
    fs::write(
        directory.path().join("p.program"),
        format!(
            "{}\n{}\n{}\non exit\n  {}\n",
            mark("one"),
            mark_and_sleep("two"),
            mark("three"),
            mark("cleanup"),
        ),
    )
    .unwrap();
    run_and_kill_at(directory.path(), keys.path(), "two");
    let resumed = execute(
        &[
            "resume",
            "p.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
        ],
        directory.path(),
        keys.path(),
    );
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed.stderr));
    assert_eq!(
        lines(&directory.path().join("side.txt")),
        vec!["one", "two", "two", "two-done", "three", "cleanup"]
    );
}

#[test]
fn checkpointed_on_exit_failure_retries_the_failed_statement() {
    let directory = tempdir().unwrap();
    let keys = tempdir().unwrap();
    let program = |status: u8| {
        format!(
            "{}\n{}\non exit\n  {}\n",
            mark("one"),
            shell(
                &format!("echo two >> side.txt; exit {status}"),
                &format!("echo two>>side.txt& exit /b {status}"),
            ),
            mark("cleanup"),
        )
    };
    fs::write(directory.path().join("p.program"), program(9)).unwrap();
    let first = execute(
        &["run", "p.program", "--checkpoint", "ck"],
        directory.path(),
        keys.path(),
    );
    assert_eq!(first.status.code(), Some(9), "{}", text(&first.stderr));
    assert_eq!(
        lines(&directory.path().join("side.txt")),
        vec!["one", "two", "cleanup"]
    );

    fs::write(directory.path().join("p.program"), program(0)).unwrap();
    let resumed = execute(
        &[
            "resume",
            "p.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
        ],
        directory.path(),
        keys.path(),
    );
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed.stderr));
    assert_eq!(
        lines(&directory.path().join("side.txt")),
        vec!["one", "two", "cleanup", "two", "cleanup"]
    );
}

#[test]
fn checkpointed_on_exit_completed_run_resumes_without_rerunning_cleanup() {
    let directory = tempdir().unwrap();
    let keys = tempdir().unwrap();
    fs::write(
        directory.path().join("p.program"),
        format!("{}\non exit\n  {}\n", mark("one"), mark("cleanup")),
    )
    .unwrap();
    let first = execute(
        &["run", "p.program", "--checkpoint", "ck"],
        directory.path(),
        keys.path(),
    );
    assert_eq!(first.status.code(), Some(0), "{}", text(&first.stderr));
    let resumed = execute(
        &["resume", "p.program", "--checkpoint", "ck"],
        directory.path(),
        keys.path(),
    );
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed.stderr));
    assert_eq!(
        lines(&directory.path().join("side.txt")),
        vec!["one", "cleanup"]
    );
}

#[test]
fn parallel_each_rejects_restored_output_over_new_limit_before_effects() {
    let directory = tempdir().unwrap();
    let keys = tempdir().unwrap();
    write_step(directory.path());
    fs::write(directory.path().join("a"), "a".repeat(3000)).unwrap();
    fs::write(directory.path().join("b"), "b".repeat(3000)).unwrap();
    fs::write(directory.path().join("c"), "c".repeat(20000)).unwrap();
    fs::write(
        directory.path().join("p.program"),
        format!(
            "parallel for each item in [\"a\", \"b\", \"c\"] at most 1 as results\n  {}\n",
            element_call()
        ),
    )
    .unwrap();
    let first = execute(
        &[
            "run",
            "p.program",
            "--checkpoint",
            "ck",
            "--max-output-bytes",
            "10000",
        ],
        directory.path(),
        keys.path(),
    );
    assert!(!first.status.success(), "{}", text(&first.stderr));
    let prior = fs::read(directory.path().join("log.txt")).unwrap();
    let resumed = execute(
        &[
            "resume",
            "p.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
            "--max-output-bytes",
            "5000",
        ],
        directory.path(),
        keys.path(),
    );
    assert!(!resumed.status.success());
    assert!(
        text(&resumed.stderr).contains("restored parallel for each output exceeded"),
        "{}",
        text(&resumed.stderr)
    );
    assert_eq!(fs::read(directory.path().join("log.txt")).unwrap(), prior);
}
