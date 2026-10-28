//! End-to-end CLI tests for issue #558: a persistent graph streams one
//! `execute.graph-event/1` JSON line per terminal event, `| head -n 3`
//! stops it through the SIGPIPE convention (141) with no diagnostic, and
//! the options a graph run cannot honour are refused instead of ignored.

#![cfg(feature = "graph")]

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const TIMER_GRAPH: &str = "graph ticks\nevery 10 milliseconds as ticker\nend graph\n";
const FINITE_GRAPH: &str =
    "graph legacy\n  run text.concat with parts [\"hello\"] as message\nend graph\n";

struct Fixture {
    directory: tempfile::TempDir,
    program: PathBuf,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("graph.program");
        fs::write(&program, source).unwrap();
        Self { directory, program }
    }

    fn command(&self, action: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .current_dir(self.directory.path())
            .arg(action)
            .arg(&self.program)
            .arg("--manifest-dir")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"));
        command
    }
}

fn failed(output: Output) -> String {
    assert!(
        !output.status.success(),
        "refused options must fail: {:?}",
        output.status
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[cfg(unix)]
#[test]
fn piped_head_stops_the_timer_stream_quietly_at_141() {
    let fixture = Fixture::new(TIMER_GRAPH);
    let mut child = fixture
        .command("run")
        .arg("--events")
        .arg("jsonl")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("streaming run spawns");
    // Read three ticks like `head -n 3` would, checking each line is one
    // valid graph event, then close the reader the same way `head` exiting
    // does.
    let stdout = child.stdout.take().expect("stream stdout is piped");
    let mut reader = BufReader::new(stdout);
    for expected in 1..=3u64 {
        let mut line = String::new();
        let count = reader
            .read_line(&mut line)
            .expect("stream stays open for three ticks");
        assert!(count > 0, "stream ended before three ticks");
        let event: Value =
            serde_json::from_str(&line).expect("one JSON line per terminal event");
        assert_eq!(event["protocol"], "execute.graph-event/1");
        assert_eq!(event["graph"], "ticks");
        assert_eq!(event["port"]["node"], "ticker");
        assert_eq!(event["sequence"], expected);
        assert!(event.get("payload").is_some(), "payload travels with the event");
    }
    drop(reader);
    let output = child
        .wait_with_output()
        .expect("stream exits after the reader closes");
    assert_eq!(
        output.status.code(),
        Some(141),
        "a closed reader exits through the SIGPIPE convention"
    );
    assert!(
        output.stderr.is_empty(),
        "a head close stays quiet, like every other pipe writer: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn events_jsonl_refuses_record_answers_and_checkpoint() {
    let fixture = Fixture::new(TIMER_GRAPH);
    let record = fixture.directory.path().join("run.record");
    let stderr = failed(
        fixture
            .command("run")
            .arg("--events")
            .arg("jsonl")
            .arg("--record")
            .arg(&record)
            .output()
            .unwrap(),
    );
    assert!(
        stderr.contains("--record applies only to finite programs"),
        "unexpected refusal: {stderr}"
    );

    let answers = fixture.directory.path().join("answers.json");
    fs::write(&answers, "{}").unwrap();
    let stderr = failed(
        fixture
            .command("run")
            .arg("--events")
            .arg("jsonl")
            .arg("--answers")
            .arg(&answers)
            .output()
            .unwrap(),
    );
    assert!(
        stderr.contains("--answers applies only to finite programs"),
        "unexpected refusal: {stderr}"
    );

    let journal = fixture.directory.path().join("journal");
    let stderr = failed(
        fixture
            .command("run")
            .arg("--events")
            .arg("jsonl")
            .arg("--checkpoint")
            .arg(&journal)
            .output()
            .unwrap(),
    );
    assert!(
        stderr.contains("--events jsonl cannot be combined with --checkpoint"),
        "unexpected refusal: {stderr}"
    );
}

#[test]
fn resume_refuses_events_jsonl() {
    let fixture = Fixture::new(TIMER_GRAPH);
    let journal = fixture.directory.path().join("journal");
    let stderr = failed(
        fixture
            .command("resume")
            .arg("--checkpoint")
            .arg(&journal)
            .arg("--events")
            .arg("jsonl")
            .output()
            .unwrap(),
    );
    assert!(
        stderr.contains("--events jsonl cannot be combined with --checkpoint"),
        "unexpected refusal: {stderr}"
    );
}

#[test]
fn resume_without_checkpoint_reports_its_own_requirement() {
    // `resume` needs `--checkpoint` before any `--events` decision is
    // reached: the parse gate fires first, so the events refusal must not
    // mask the missing checkpoint.
    let fixture = Fixture::new(TIMER_GRAPH);
    let stderr = failed(
        fixture
            .command("resume")
            .arg("--events")
            .arg("jsonl")
            .output()
            .unwrap(),
    );
    assert!(
        stderr.contains("resume requires --checkpoint DIR"),
        "unexpected refusal: {stderr}"
    );
}

#[test]
fn finite_graph_runs_refuse_record_and_answers() {
    // Graph runs never honour these finite-program options, so they are
    // refused on the document path too, not just on the event stream.
    let fixture = Fixture::new(FINITE_GRAPH);
    let record = fixture.directory.path().join("run.record");
    let stderr = failed(
        fixture
            .command("run")
            .arg("--record")
            .arg(&record)
            .output()
            .unwrap(),
    );
    assert!(
        stderr.contains("--record applies only to finite programs"),
        "unexpected refusal: {stderr}"
    );

    let answers = fixture.directory.path().join("answers.json");
    fs::write(&answers, "{}").unwrap();
    let stderr = failed(
        fixture
            .command("run")
            .arg("--answers")
            .arg(&answers)
            .output()
            .unwrap(),
    );
    assert!(
        stderr.contains("--answers applies only to finite programs"),
        "unexpected refusal: {stderr}"
    );
}

#[test]
fn finite_graph_streams_events_jsonl_on_request() {
    // An explicit `--events jsonl` streams even a finite graph, so the
    // refusal gates above cover the same rendering the timer uses.
    let fixture = Fixture::new(FINITE_GRAPH);
    let output = fixture
        .command("run")
        .arg("--events")
        .arg("jsonl")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "explicit streaming must run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<&str> = std::str::from_utf8(&output.stdout)
        .expect("stream is UTF-8")
        .lines()
        .collect();
    assert_eq!(lines.len(), 1, "the finite graph emits one terminal event");
    let event: Value = serde_json::from_str(lines[0]).expect("one JSON line per event");
    assert_eq!(event["protocol"], "execute.graph-event/1");
    assert_eq!(event["graph"], "legacy");
}
