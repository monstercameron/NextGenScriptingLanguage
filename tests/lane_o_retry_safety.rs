//! Retry safety decides which checkpointed elements write a durable begin record before
//! they run. A pure or read-only in-process loop writes none, so it costs no flush per
//! element. A native command has effects the runtime cannot see, so each of its elements
//! writes one.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn execute(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap()
}

fn fixture() -> String {
    let name = if cfg!(windows) {
        "native-fixture.exe"
    } else {
        "native-fixture"
    };
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join(name);
    path.to_str().unwrap().replace('\\', "/")
}

fn elements_log(checkpoint: &Path) -> String {
    fs::read_to_string(checkpoint.join("elements.jsonl")).unwrap_or_default()
}

fn begin_records(log: &str) -> usize {
    log.lines()
        .filter(|line| line.contains("\"kind\":\"begin\""))
        .count()
}

#[test]
fn a_pure_in_process_loop_writes_no_begin_records() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("pure.program"),
        "for each item in [\"a\", \"b\", \"c\", \"d\"]\n  run text.upper with text item\n",
    )
    .unwrap();
    let output = execute(
        &["run", "pure.program", "--checkpoint", "ck"],
        directory.path(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = elements_log(&directory.path().join("ck"));
    assert_eq!(
        begin_records(&log),
        0,
        "no element may write a begin record: {log}"
    );
}

#[test]
fn a_native_command_loop_writes_a_begin_record_per_element() {
    let directory = tempdir().unwrap();
    let program = format!(
        "for each item in [\"a\", \"b\", \"c\"]\n  run \"{}\" with arguments [\"count-wait\", \"counters\", item, \"gates\"]\n",
        fixture()
    );
    fs::write(directory.path().join("native.program"), program).unwrap();
    fs::create_dir(directory.path().join("gates")).unwrap();
    fs::create_dir(directory.path().join("counters")).unwrap();
    let output = execute(
        &["run", "native.program", "--checkpoint", "ck"],
        directory.path(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = elements_log(&directory.path().join("ck"));
    assert_eq!(
        begin_records(&log),
        3,
        "each native element must write a begin record: {log}"
    );
}
