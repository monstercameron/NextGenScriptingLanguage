#![cfg(feature = "live")]
//! Breakpoints and statement stepping on a hosted run (`execute run FILE --control`).
//! The program sleeps first so the breakpoint is set before the loop starts.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PREFIX: &str = "execute: control endpoint ";

/// Sleeps three seconds, then loops over three values. The body is line 3.
const LOOP: &str = "run \"sleep\" with arguments [\"3\"]\n\
                    for each item in [\"a\", \"b\", \"c\"]\n\
                    \x20\x20run printf with message item\n";

struct Hosted {
    child: Child,
    endpoint: PathBuf,
    stdout: JoinHandle<String>,
    stderr: JoinHandle<Vec<String>>,
}

fn execute(arguments: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("start execute")
}

fn host(directory: &Path, program: &str) -> Hosted {
    std::fs::write(directory.join("tool.program"), program).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "tool.program", "--control"])
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start execute");
    let mut stdout_pipe = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let stderr = thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
            if let Some(endpoint) = line.strip_prefix(PREFIX) {
                let _ = sender.send(endpoint.to_owned());
            }
            lines.push(line);
        }
        lines
    });
    let stdout = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout_pipe.read_to_string(&mut text);
        text
    });
    let endpoint = match receiver.recv_timeout(Duration::from_secs(20)) {
        Ok(endpoint) => endpoint,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("no control endpoint line on stderr");
        }
    };
    Hosted {
        child,
        endpoint: PathBuf::from(endpoint),
        stdout,
        stderr,
    }
}

fn wait_exit(child: &mut Child, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("hosted run did not exit within {limit:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Send one control request through `execute live control`.
fn control(endpoint: &Path, arguments: &[&str]) -> Value {
    let endpoint = endpoint.to_string_lossy().into_owned();
    let mut all = vec!["live", "control"];
    all.extend_from_slice(arguments);
    all.extend_from_slice(&["--socket", endpoint.as_str()]);
    let output = execute(&all, Path::new("."));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "expected JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// The `/runtime/debug` object: breakpoints and the paused statement, if any.
fn debug_state(endpoint: &Path) -> Value {
    let endpoint = endpoint.to_string_lossy().into_owned();
    let output = execute(
        &[
            "live",
            "read",
            "/runtime/debug",
            "--socket",
            endpoint.as_str(),
        ],
        Path::new("."),
    );
    let object: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(object["ok"], true, "{object}");
    object["result"].clone()
}

/// Wait until the run pauses before a statement at `line`, and return that pause.
fn wait_paused_at(endpoint: &Path, line: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let debug = debug_state(endpoint);
        if debug["paused"]["line"] == line {
            return debug["paused"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "run did not pause at line {line}: {debug}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn breakpoint_in_a_loop_reads_the_iteration_steps_once_and_continues() {
    let directory = tempfile::tempdir().unwrap();
    let mut hosted = host(directory.path(), LOOP);
    let endpoint = hosted.endpoint.clone();

    let set = control(&endpoint, &["breakpoint", "3"]);
    assert_eq!(set["ok"], true, "{set}");

    // The first iteration pauses before the body statement, with its binding.
    let first = wait_paused_at(&endpoint, 3);
    assert_eq!(first["bindings"]["item"], "a", "{first}");

    // One step runs the body statement and pauses before the loop header.
    let stepped = control(&endpoint, &["step"]);
    assert_eq!(stepped["ok"], true, "{stepped}");
    let header = wait_paused_at(&endpoint, 2);
    assert!(header["bindings"].is_object(), "{header}");

    // Continue runs to the breakpoint again, on the second iteration.
    let continued = control(&endpoint, &["continue"]);
    assert_eq!(continued["ok"], true, "{continued}");
    let second = wait_paused_at(&endpoint, 3);
    assert_eq!(second["bindings"]["item"], "b", "{second}");

    // Clearing the breakpoints lets the rest of the loop run to completion.
    let cleared = control(&endpoint, &["breakpoint"]);
    assert_eq!(cleared["ok"], true, "{cleared}");
    let released = control(&endpoint, &["continue"]);
    assert_eq!(released["ok"], true, "{released}");

    let status = wait_exit(&mut hosted.child, Duration::from_secs(60));
    let stdout = hosted.stdout.join().unwrap();
    let stderr = hosted.stderr.join().unwrap();
    assert!(status.success(), "{stderr:?}");
    // Every unconsumed iteration writes its stdout in order.
    assert_eq!(stdout, "a\nb\nc\n");
    assert!(!endpoint.exists(), "the endpoint should be removed on exit");
}

#[test]
fn cancelling_a_run_paused_at_a_breakpoint_ends_it_nonzero() {
    let directory = tempfile::tempdir().unwrap();
    let mut hosted = host(directory.path(), LOOP);
    let endpoint = hosted.endpoint.clone();
    control(&endpoint, &["breakpoint", "3"]);
    wait_paused_at(&endpoint, 3);

    let cancelled = control(&endpoint, &["cancel"]);
    assert_eq!(cancelled["ok"], true, "{cancelled}");

    let status = wait_exit(&mut hosted.child, Duration::from_secs(60));
    assert!(!status.success());
    assert!(!endpoint.exists(), "the endpoint should be removed on exit");
}

#[test]
fn continue_is_refused_unless_the_run_is_paused_and_breakpoints_need_positive_lines() {
    let directory = tempfile::tempdir().unwrap();
    let mut hosted = host(directory.path(), LOOP);
    let endpoint = hosted.endpoint.clone();

    // The program is still sleeping, so nothing is paused.
    let refused = control(&endpoint, &["continue"]);
    assert_eq!(refused["error"]["code"], "not_paused", "{refused}");

    let endpoint_text = endpoint.to_string_lossy().into_owned();
    let bad = execute(
        &[
            "live",
            "control",
            "breakpoint",
            "0",
            "--socket",
            &endpoint_text,
        ],
        directory.path(),
    );
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stdout).contains("positive integer"),
        "{}",
        String::from_utf8_lossy(&bad.stdout)
    );

    // Let the run finish so the child exits cleanly.
    let status = wait_exit(&mut hosted.child, Duration::from_secs(60));
    assert!(status.success());
}
