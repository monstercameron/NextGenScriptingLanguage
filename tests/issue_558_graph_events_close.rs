#![cfg(feature = "graph")]
//! #558: a persistent graph's jsonl stream stops cleanly when its reader goes
//! away. After three ticks are read and stdout is closed, as `| head -n 3`
//! does, the `execute` child must exit within a few seconds with no panic.

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const TIMER_GRAPH: &str = "graph ticks\nevery 10 milliseconds as ticker\nend graph\n";

#[test]
fn closing_the_reader_after_three_ticks_stops_the_graph() {
    let directory = tempfile::tempdir().unwrap();
    let program: PathBuf = directory.path().join("graph.program");
    fs::write(&program, TIMER_GRAPH).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&program)
        .arg("--events")
        .arg("jsonl")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("streaming run spawns");

    let stdout = child.stdout.take().expect("stdout is piped");
    let mut reader = BufReader::new(stdout);
    for expected in 1..=3u64 {
        let mut line = String::new();
        let count = reader
            .read_line(&mut line)
            .expect("stream stays open for three ticks");
        if count == 0 {
            let _ = child.kill();
            let _ = child.wait();
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!(
                "stream ended before three ticks; stderr: {}",
                stderr.replace('\n', " | ")
            );
        }
        let event: Value = serde_json::from_str(&line).expect("one JSON line per tick");
        assert_eq!(event["protocol"], "execute.graph-event/1");
        assert_eq!(event["sequence"], expected);
    }

    // Closing the read end is what `| head -n 3` does once it has its lines.
    drop(reader);

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait().expect("child status is readable") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("graph kept running for 5 s after its reader closed");
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };

    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    assert!(
        !stderr.contains("panicked") && !stderr.contains("panic"),
        "closing the reader must not panic the graph: {status:?}\n{stderr}"
    );
}
