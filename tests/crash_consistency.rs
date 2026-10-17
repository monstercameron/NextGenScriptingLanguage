//! Crash consistency for checkpointed runs (#90). A run is killed at a random moment, or a
//! checkpoint write fails at a chosen call through the `failpoints` feature. Resuming must
//! then either reproduce the uninterrupted run exactly, with each completed step counted
//! once, or refuse with a message that names the reason. A panic, a duplicated step, or a
//! refusal without a reason fails the test. The seed is printed, so a failure replays with
//! `CRASH_SEED`. `CRASH_TRIALS` sets the number of random kills per mode (nightly raises it).
//!
//! Run with: `cargo test --features failpoints --test crash_consistency`
#![cfg(all(unix, feature = "failpoints"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Steps in each sequential or job program, and the count log's expected length.
const STEPS: usize = 6;
/// Checkpoint writes injected per point and mode.
const FAILED_CALLS: usize = 6;
const POINTS: [&str; 2] = ["checkpoint.atomic", "checkpoint.append"];

#[derive(Clone, Copy, Debug)]
enum Mode {
    Sequential,
    Jobs,
    Graph,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Sequential => "sequential",
            Mode::Jobs => "jobs",
            Mode::Graph => "graph",
        }
    }

    /// Each program counts its runs in one log, so a duplicated step shows up as a line.
    fn program(self) -> String {
        match self {
            Mode::Sequential => (0..STEPS)
                .map(|step| format!("run ctrl.count with text \"s{step}\"\n"))
                .collect(),
            Mode::Jobs => (0..STEPS)
                .map(|job| {
                    format!(
                        "spawn as j{job}\n  run ctrl.count with text \"j{job}\"\nawait j{job} and keep status as c{job}\n"
                    )
                })
                .collect(),
            Mode::Graph => "graph chain\n\
                \x20 run ctrl.count with text \"a\" as first\n\
                \x20 run ctrl.count as second\n\
                \x20 bind first.value to second.text with capacity 1 lossless\n\
                \x20 run ctrl.count as third\n\
                \x20 bind second.value to third.text with capacity 1 lossless\n\
                end graph\n"
                .to_owned(),
        }
    }
}

/// A fresh workspace: the applets, the program, and an empty count log.
struct Fixture {
    _directory: tempfile::TempDir,
    manifests: PathBuf,
    program: PathBuf,
    checkpoint: PathBuf,
    log: PathBuf,
}

/// `ctrl.count` appends each run to the log and echoes its text. The script runs under
/// `/bin/sh`; the manifest is the same contract the other checkpoint tests use.
fn write_count_applet(manifests: &Path, log: &Path) {
    let script = manifests.with_file_name("count.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho run >> '{}'\nprintf '%s\\n' \"$1\"\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let manifest = serde_json::json!({
        "name": "ctrl.count",
        "binary": "/bin/sh",
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {"text": {"type": "string", "required": true}},
        "arguments": [
            {"literal": script.to_string_lossy()},
            {"parameter": "text"}
        ],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw", "trim_final_lf": true},
            "outcomes": [{"status": 0, "name": "success", "kind": "success"}],
            "capabilities": [],
            "effects": [],
            "ports": {
                "inputs": {"text": {"schema": {"type": "text"}, "delivery": "single"}},
                "outputs": {"value": {"schema": {"type": "text"}, "delivery": "single"}}
            }
        }
    });
    fs::write(manifests.join("ctrl.count.json"), manifest.to_string()).unwrap();
}

fn fixture(mode: Mode) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let manifests = root.join("manifests");
    fs::create_dir(&manifests).unwrap();
    let log = root.join("count.log");
    write_count_applet(&manifests, &log);
    let program = root.join("workflow.program");
    fs::write(&program, mode.program()).unwrap();
    Fixture {
        _directory: directory,
        manifests,
        program,
        checkpoint: root.join("checkpoint"),
        log,
    }
}

fn execute(fixture: &Fixture, arguments: &[&std::ffi::OsStr]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .args(arguments)
        .arg("--manifest-dir")
        .arg(&fixture.manifests)
        .stdin(Stdio::null());
    command
}

fn checkpointed_run(fixture: &Fixture) -> Command {
    let mut command = execute(
        fixture,
        &[
            "run".as_ref(),
            fixture.program.as_os_str(),
            "--checkpoint".as_ref(),
            fixture.checkpoint.as_os_str(),
        ],
    );
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command
}

fn resume(fixture: &Fixture) -> Output {
    execute(
        fixture,
        &[
            "resume".as_ref(),
            fixture.program.as_os_str(),
            "--checkpoint".as_ref(),
            fixture.checkpoint.as_os_str(),
        ],
    )
    .output()
    .unwrap()
}

fn log_lines(fixture: &Fixture) -> Vec<String> {
    fs::read_to_string(&fixture.log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The uninterrupted run's count log and how long it took. Random kills fall inside it.
fn baseline(mode: Mode) -> (Vec<String>, Duration) {
    let fixture = fixture(mode);
    let started = Instant::now();
    let output = execute(&fixture, &["run".as_ref(), fixture.program.as_os_str()])
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        output.status.success(),
        "baseline {}: {:?}",
        mode.name(),
        output
    );
    let lines = log_lines(&fixture);
    assert_eq!(
        lines.len(),
        expected_runs(mode),
        "baseline {} counts",
        mode.name()
    );
    (lines, elapsed)
}

/// Runs the uninterrupted program performs: one per step, plus the graph's binds.
fn expected_runs(mode: Mode) -> usize {
    match mode {
        Mode::Sequential | Mode::Jobs => STEPS,
        Mode::Graph => 3,
    }
}

/// A seeded generator, so the same seed replays the same kill points.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound.max(1)
    }
}

fn seed() -> u64 {
    std::env::var("CRASH_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0x5EED_2026)
}

fn trials() -> usize {
    std::env::var("CRASH_TRIALS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
}

/// After a failure or a kill: resume, then either match the uninterrupted run or refuse.
fn check_resume(fixture: &Fixture, expected: &[String], context: &str) {
    let output = resume(fixture);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        assert_eq!(
            log_lines(fixture),
            expected,
            "{context}: the resumed run diverged from the uninterrupted run"
        );
        return;
    }
    assert!(
        output.status.code().is_some(),
        "{context}: resume ended on a signal: {output:?}"
    );
    assert!(
        !stderr.contains("panicked"),
        "{context}: resume panicked: {stderr}"
    );
    assert!(
        stderr.contains("checkpoint") || stderr.contains("retry-uncertain"),
        "{context}: refusal without a reason: {stderr}"
    );
}

fn kill_at_random_points(mode: Mode) {
    let (expected, elapsed) = baseline(mode);
    let window = elapsed.as_micros() as u64 + 1;
    let mut rng = Lcg(seed() ^ ((mode.name().len() as u64) << 32));
    for trial in 0..trials() {
        let fixture = fixture(mode);
        let mut child = checkpointed_run(&fixture).spawn().unwrap();
        let delay = Duration::from_micros(rng.below(window));
        let started = Instant::now();
        while started.elapsed() < delay {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let _ = child.kill();
        let _ = child.wait();
        let context = format!(
            "{} kill trial {trial} (seed {}, after {delay:?})",
            mode.name(),
            seed()
        );
        check_resume(&fixture, &expected, &context);
    }
}

fn fail_writes(mode: Mode) {
    let (expected, _) = baseline(mode);
    for point in POINTS {
        for nth in 1..=FAILED_CALLS {
            let fixture = fixture(mode);
            // checkpointed_run discards stderr for the other tests; this one needs the
            // injected error text, so stderr is piped back after that setting.
            let output = checkpointed_run(&fixture)
                .env("EXECUTE_FAILPOINT", format!("{point}:{nth}"))
                .stderr(Stdio::piped())
                .output()
                .unwrap();
            let context = format!("{} {point} write {nth}", mode.name());
            if !output.status.success() {
                // The injected fault must surface as the run's error, not as a crash.
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    stderr.contains("injected failure"),
                    "{context}: failed for another reason, status and output: {output:?}"
                );
            }
            check_resume(&fixture, &expected, &context);
        }
    }
}

#[test]
fn sequential_runs_killed_at_random_points_resume_to_the_uninterrupted_run() {
    kill_at_random_points(Mode::Sequential);
}

#[test]
fn job_runs_killed_at_random_points_resume_to_the_uninterrupted_run() {
    kill_at_random_points(Mode::Jobs);
}

#[test]
fn graph_runs_killed_at_random_points_resume_to_the_uninterrupted_run() {
    kill_at_random_points(Mode::Graph);
}

#[test]
fn sequential_runs_with_failed_checkpoint_writes_resume_or_refuse_clearly() {
    fail_writes(Mode::Sequential);
}

#[test]
fn job_runs_with_failed_checkpoint_writes_resume_or_refuse_clearly() {
    fail_writes(Mode::Jobs);
}

#[test]
fn graph_runs_with_failed_checkpoint_writes_resume_or_refuse_clearly() {
    fail_writes(Mode::Graph);
}
