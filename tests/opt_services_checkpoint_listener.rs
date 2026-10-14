#![cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

fn wait_bounded(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("checkpointed listener exceeded its five-second test deadline");
            }
        }
    }
}

#[test]
fn fresh_checkpointed_listener_keeps_each_record_and_binding() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let workflow = directory.path().join("listener.program");
    let checkpoint = directory.path().join("checkpoint");
    std::fs::write(
        &workflow,
        "listen to stdin\nwhenever a line arrives as record\n  run printf with message record\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(&manifests)
        .arg("--checkpoint")
        .arg(&checkpoint)
        .arg("--max-runs")
        .arg("2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"first record\nsecond record\n")
        .unwrap();
    drop(child.stdin.take());

    let status = wait_bounded(&mut child);
    let mut output = Vec::new();
    let mut errors = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut errors)
        .unwrap();
    assert!(status.success(), "{}", String::from_utf8_lossy(&errors));
    assert_eq!(output, b"first record\n\nsecond record\n\n");
}

struct ChildCleanup(Child);

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spawn_output_readers(
    child: &mut Child,
) -> (
    Receiver<String>,
    std::thread::JoinHandle<Vec<u8>>,
    std::thread::JoinHandle<Vec<String>>,
) {
    let (sender, receiver) = mpsc::channel();
    let mut stdout = child.stdout.take().unwrap();
    let stdout_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).unwrap();
        output
    });
    let stderr = child.stderr.take().unwrap();
    let trace_reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let _ = sender.send(line.clone());
            lines.push(line);
        }
        lines
    });
    (receiver, stdout_reader, trace_reader)
}

fn wait_for_handler_end(events: &Receiver<String>, run: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "handler_end for run {run} was not traced"
        );
        match events.recv_timeout(remaining) {
            Ok(line) => {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if event["event"] == "handler_end" && event["run"].as_u64() == Some(run) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("handler_end for run {run} was not traced before the safety deadline");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("listener trace closed before handler_end for run {run}");
            }
        }
    }
}

#[test]
fn failed_next_event_snapshot_write_prevents_handler_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let workflow = directory.path().join("listener.program");
    let checkpoint = directory.path().join("checkpoint");
    let effect = directory.path().join("effect.txt");
    std::fs::write(
        &workflow,
        "listen to stdin\nwhenever a line arrives as record\n  then save to file \"effect.txt\"\n",
    )
    .unwrap();

    let runner = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(&manifests)
        .arg("--checkpoint")
        .arg(&checkpoint)
        .arg("--trace")
        .arg("--max-runs")
        .arg("2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut runner = ChildCleanup(runner);
    let (events, stdout_reader, trace_reader) = spawn_output_readers(&mut runner.0);

    runner
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"first record\n")
        .unwrap();
    runner.0.stdin.as_mut().unwrap().flush().unwrap();
    // handler_end follows Journal::finish(), so this trace is the acknowledgement
    // barrier for the first record's durable idle checkpoint.
    wait_for_handler_end(&events, 1);

    let journal_path = checkpoint.join("journal.json");
    let journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    assert_eq!(journal["data"]["status"], "idle");
    assert_eq!(std::fs::read(&effect).unwrap(), b"first record\n");

    // Make the next atomic journal publication fail after the prior idle state
    // is durable. The second record must not enter its handler or overwrite save.
    std::fs::remove_file(&journal_path).unwrap();
    std::fs::create_dir(&journal_path).unwrap();
    runner
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"second record\n")
        .unwrap();
    runner.0.stdin.as_mut().unwrap().flush().unwrap();
    drop(runner.0.stdin.take());

    let status = wait_bounded(&mut runner.0);
    assert!(
        !status.success(),
        "failed snapshot publication should stop the listener"
    );
    let output = stdout_reader.join().unwrap();
    let trace = trace_reader.join().unwrap();
    assert_eq!(output, b"first record\n");
    assert_eq!(std::fs::read(&effect).unwrap(), b"first record\n");
    let handler_starts = trace
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["event"] == "handler_start")
        .count();
    assert_eq!(
        handler_starts, 1,
        "second handler started before snapshot persistence"
    );
}
