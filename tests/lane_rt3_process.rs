//! Native process output regressions (lane RT3): redirected stderr and
//! `stdout to stderr` stream as the command writes, caught failures keep their
//! captured stdout, and `parallel for each` charges its output budget as bytes
//! arrive. Commands use `sh -c` on Unix and `cmd /C` on Windows.
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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

fn command(directory: &Path, script: &str, extra: &[&str]) -> Command {
    let path = directory.join("case.program");
    fs::write(&path, script).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory)
        .arg("run")
        .arg(&path)
        .args(extra);
    command
}

fn run(directory: &Path, script: &str, extra: &[&str]) -> Output {
    command(directory, script, extra).output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn stderr_redirect_keeps_the_output_of_a_timed_out_command() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("timeout.err"), "old\n").unwrap();
    let script = format!(
        "{} and stderr to \"timeout.err\"\n",
        shell(
            "echo early-stderr >&2; sleep 6",
            "(echo early-stderr)1>&2 & ping -n 7 127.0.0.1 >nul",
        )
    );
    let output = run(directory.path(), &script, &["--command-timeout-ms", "1500"]);
    assert!(!output.status.success(), "the command must time out");
    assert!(
        text(&output.stderr).contains("timed out"),
        "{}",
        text(&output.stderr)
    );
    let file = fs::read_to_string(directory.path().join("timeout.err")).unwrap();
    assert!(file.contains("early-stderr"), "{file:?}");
    assert!(!file.contains("old"), "{file:?}");
}

#[test]
fn stderr_redirect_writes_the_file_while_the_command_runs() {
    let directory = tempfile::tempdir().unwrap();
    let script = format!(
        "{} and stderr to \"live.err\"\n",
        shell(
            "echo early-stderr >&2; sleep 5; echo late >&2",
            "(echo early-stderr)1>&2 & ping -n 6 127.0.0.1 >nul & (echo late)1>&2",
        )
    );
    let mut child = command(directory.path(), &script, &[])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let file = directory.path().join("live.err");
    let started = Instant::now();
    let mut seen_while_running = false;
    while started.elapsed() < Duration::from_secs(4) {
        let contents = fs::read_to_string(&file).unwrap_or_default();
        if contents.contains("early-stderr") {
            seen_while_running = child.try_wait().unwrap().is_none();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(
        seen_while_running,
        "redirected stderr must reach the file while the command runs"
    );
    let contents = fs::read_to_string(&file).unwrap();
    assert!(
        contents.contains("early-stderr") && contents.contains("late"),
        "{contents:?}"
    );
}

#[test]
fn stdout_to_stderr_survives_a_failure_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let script = format!(
        "{} and stdout to stderr\n",
        shell(
            "echo out; echo err >&2; exit 1",
            "echo out& (echo err)1>&2 & exit /b 1",
        )
    );
    let output = run(directory.path(), &script, &[]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{}", text(&output.stdout));
    let stderr = text(&output.stderr);
    let out = stderr.find("out").expect("stdout must reach stderr");
    let err = stderr.find("err").expect("the command's own stderr");
    assert!(
        out < err,
        "stdout must keep its order with stderr: {stderr:?}"
    );
}

#[test]
fn stdout_to_stderr_survives_a_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let script = format!(
        "{} and stdout to stderr\n",
        shell(
            "echo early-out; sleep 6; echo late-out",
            "echo early-out& ping -n 7 127.0.0.1 >nul & echo late-out",
        )
    );
    let output = run(directory.path(), &script, &["--command-timeout-ms", "1500"]);
    assert!(!output.status.success());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("early-out"), "{stderr:?}");
    assert!(!stderr.contains("late-out"), "{stderr:?}");
}

#[test]
fn caught_native_failure_carries_its_captured_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let script = format!(
        "try\n  {} as x\ncatch failure\n  then encode failure as json\n",
        shell(
            "echo partial-output; exit 2",
            "echo partial-output& exit /b 2"
        )
    );
    let output = run(directory.path(), &script, &[]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["status"], 2, "{record}");
    assert_eq!(record["stdout_captured"], true, "{record}");
    let bytes = record["stdout"]
        .as_array()
        .unwrap()
        .iter()
        .map(|byte| byte.as_u64().unwrap() as u8)
        .collect::<Vec<_>>();
    assert!(text(&bytes).contains("partial-output"), "{record}");
}

#[test]
fn parallel_each_stops_an_element_when_the_shared_output_budget_runs_out() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("big.bin"), vec![b'a'; 700_000]).unwrap();
    // One element at a time: the first fits, the second exhausts the shared budget
    // partway through and must be stopped before it finishes (and before it marks).
    let script = format!(
        "parallel for each x in [\"a\", \"b\", \"c\"] at most 1 as results\n  {}\n",
        shell(
            "cat big.bin; echo done >> marker.txt",
            "type big.bin & echo done>>marker.txt",
        )
    );
    let output = run(
        directory.path(),
        &script,
        &["--max-output-bytes", "1000000"],
    );
    assert!(!output.status.success(), "the group must fail");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("parallel for each output exceeded"),
        "{stderr}"
    );
    let marks = fs::read_to_string(directory.path().join("marker.txt")).unwrap_or_default();
    assert_eq!(
        marks.lines().count(),
        1,
        "the element that crossed the budget must be stopped as its bytes arrive: {marks:?}"
    );
}
