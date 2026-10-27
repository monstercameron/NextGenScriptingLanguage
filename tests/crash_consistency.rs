//! Crash consistency for checkpointed runs (#90). A run is killed at a random moment, or a
//! checkpoint write fails at a chosen call through the `failpoints` feature. Resuming must
//! then either reproduce the uninterrupted run exactly, with each completed step counted
//! once, or refuse with a message that names the reason. A panic, a duplicated step, or a
//! refusal without a reason fails the test. The seed is printed, so a failure replays with
//! `CRASH_SEED`. `CRASH_TRIALS` sets the number of random kills per mode (nightly raises it).
//! The `lost` action is a deterministic in-process write shim; these tests do not simulate
//! storage-device power loss or establish power-loss durability. `lost` skips one selected
//! write call; it does not drop all unsynced file contents and directory metadata at a crash.
//! The nightly Linux job separately drops all write bios emitted during a controlled
//! post-append interval on a private loopback filesystem with dm-flakey. Writes that
//! reached the backing image before the target switch are retained, so this does not prove
//! loss of every unsynced write at an arbitrary power-loss instant. Windows runs the
//! portable process-failure suite only.
//!
//! Run with: `cargo test --features failpoints --test crash_consistency`
#![cfg(feature = "failpoints")]

use sha2::Digest;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Steps in each sequential or job program, and the count log's expected length.
const STEPS: usize = 6;
/// Checkpoint boundaries exercised by the write-failure matrix. Blob collection is
/// exercised separately during resume, since it cannot run during a normal `run`.
const WRITE_POINTS: [&str; 19] = [
    "checkpoint.atomic",
    "checkpoint.atomic.write",
    "checkpoint.atomic.sync",
    "checkpoint.atomic.synced",
    "checkpoint.atomic.rename",
    "checkpoint.atomic.renamed",
    "checkpoint.atomic.directory_sync",
    "checkpoint.append",
    "checkpoint.append.truncate",
    "checkpoint.append.write",
    "checkpoint.append.sync",
    "checkpoint.graph.truncate",
    "checkpoint.graph.write",
    "checkpoint.graph.sync",
    "checkpoint.elements.write",
    "checkpoint.elements.truncate",
    "checkpoint.elements.sync",
    "checkpoint.elements.reset.truncate",
    "checkpoint.elements.reset.sync",
];

#[derive(Clone, Copy, Debug)]
enum Mode {
    Sequential,
    Jobs,
    Graph,
    Elements,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Sequential => "sequential",
            Mode::Jobs => "jobs",
            Mode::Graph => "graph",
            Mode::Elements => "elements",
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
            Mode::Elements => "for each item in [\"a\", \"b\"]\n\
                \x20 run ctrl.count with text item\n\
                for each item in [\"c\", \"d\"]\n\
                \x20 run ctrl.count with text item\n"
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

/// `ctrl.count` appends each run to the log and echoes its text. Unix uses `/bin/sh`;
/// Windows uses a generated `.cmd` applet so the same crash suite runs on both systems.
fn write_count_applet(manifests: &Path, log: &Path) {
    #[cfg(unix)]
    let (binary, arguments) = {
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
        (
            "/bin/sh",
            vec![
                serde_json::json!({"literal": script.to_string_lossy()}),
                serde_json::json!({"parameter": "text"}),
            ],
        )
    };
    #[cfg(windows)]
    let (binary, arguments) = {
        let script = manifests.with_file_name("count.cmd");
        let log = log.to_string_lossy().replace('"', "\"\"");
        fs::write(
            &script,
            format!("@echo off\r\n>>\"{log}\" echo run\r\necho %~1\r\n"),
        )
        .unwrap();
        (
            "cmd.exe",
            vec![
                serde_json::json!({"literal": "/D"}),
                serde_json::json!({"literal": "/C"}),
                serde_json::json!({"literal": script.to_string_lossy()}),
                serde_json::json!({"parameter": "text"}),
            ],
        )
    };
    let manifest = serde_json::json!({
        "name": "ctrl.count",
        "binary": binary,
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {"text": {"type": "string", "required": true}},
        "arguments": arguments,
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
        Mode::Elements => 4,
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
    for point in WRITE_POINTS {
        // Exercise only boundaries reachable for this program shape. The first
        // occurrence is enough to prove the failpoint is wired into the path; the
        // explicit abort/partial-write cases below cover torn writes.
        if !point_applies_to_mode(point, mode) {
            continue;
        }
        let fixture = fixture(mode);
        // checkpointed_run discards stderr for the other tests; this one needs the
        // injected error text, so stderr is piped back after that setting.
        let output = checkpointed_run(&fixture)
            .env("EXECUTE_FAILPOINT", format!("{point}:1"))
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let context = format!("{} {point} write 1", mode.name());
        assert!(
            !output.status.success(),
            "{context}: failpoint was not reached"
        );
        // The injected fault must surface as the run's error, not as a crash.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("injected failure"),
            "{context}: failed for another reason, status and output: {output:?}"
        );
        check_resume(&fixture, &expected, &context);
    }
}

fn point_applies_to_mode(point: &str, mode: Mode) -> bool {
    if matches!(mode, Mode::Graph)
        && (point.starts_with("checkpoint.atomic")
            || matches!(
                point,
                "checkpoint.append.truncate" | "checkpoint.append.write" | "checkpoint.append.sync"
            ))
    {
        return false;
    }
    match point {
        "checkpoint.graph.truncate" | "checkpoint.graph.write" | "checkpoint.graph.sync" => {
            matches!(mode, Mode::Graph)
        }
        "checkpoint.elements.write"
        | "checkpoint.elements.truncate"
        | "checkpoint.elements.sync"
        | "checkpoint.elements.reset.truncate"
        | "checkpoint.elements.reset.sync" => matches!(mode, Mode::Elements),
        _ => true,
    }
}

#[test]
fn atomic_and_append_boundaries_survive_abort_and_torn_write_actions() {
    let (expected, _) = baseline(Mode::Sequential);
    let points = [
        "checkpoint.atomic.write",
        "checkpoint.atomic.sync",
        "checkpoint.atomic.synced",
        "checkpoint.atomic.rename",
        "checkpoint.atomic.renamed",
        "checkpoint.atomic.directory_sync",
        "checkpoint.append.write",
        "checkpoint.append.sync",
    ];
    for point in points {
        // `partial:3` means a prefix write only for write failpoints; at sync,
        // rename, and directory boundaries it is just an abort action.
        let actions: &[&str] = match point {
            "checkpoint.atomic.write" | "checkpoint.append.write" => &["abort", "partial:3"],
            _ => &["abort"],
        };
        for &action in actions {
            let fixture = fixture(Mode::Sequential);
            let output = checkpointed_run(&fixture)
                .env("EXECUTE_FAILPOINT", format!("{point}:1:{action}"))
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "{point}:{action} did not terminate the checkpointed run"
            );
            check_resume(&fixture, &expected, &format!("{point}:{action}"));
        }
    }
}

#[test]
fn silently_lost_atomic_and_append_writes_recover_or_refuse_clearly() {
    let (expected, _) = baseline(Mode::Sequential);
    for point in ["checkpoint.atomic.write", "checkpoint.append.write"] {
        let fixture = fixture(Mode::Sequential);
        let _ = checkpointed_run(&fixture)
            .env("EXECUTE_FAILPOINT", format!("{point}:1:lost"))
            .output()
            .unwrap();
        check_resume(&fixture, &expected, &format!("{point}:lost"));
    }
}

#[test]
fn blob_gc_abort_preserves_checkpoint_recovery() {
    let (expected, _) = baseline(Mode::Sequential);
    // Blob collection is a remove boundary, not a byte-write boundary.
    for action in ["abort"] {
        let fixture = fixture(Mode::Sequential);
        let interrupted = checkpointed_run(&fixture)
            .env("EXECUTE_FAILPOINT", "checkpoint.append.write:1:abort")
            .status()
            .unwrap();
        assert!(
            !interrupted.success(),
            "append failpoint did not interrupt the run"
        );
        let orphan = format!("{:x}", sha2::Sha256::digest(b"orphan blob"));
        let blob_path = fixture.checkpoint.join("blobs").join(orphan);
        fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        fs::write(&blob_path, b"orphan blob").unwrap();

        let output = execute(
            &fixture,
            &[
                "resume".as_ref(),
                fixture.program.as_os_str(),
                "--checkpoint".as_ref(),
                fixture.checkpoint.as_os_str(),
            ],
        )
        .env(
            "EXECUTE_FAILPOINT",
            format!("checkpoint.blob_gc.remove:1:{action}"),
        )
        .output()
        .unwrap();
        assert!(
            !output.status.success(),
            "blob GC failpoint {action} was not reached"
        );
        assert!(blob_path.is_file(), "aborted collection removed the orphan");

        check_resume(&fixture, &expected, &format!("blob GC {action}"));
        // Collection may not run again after recovery commits the final checkpoint state;
        // leaving an orphan behind must not affect the recovered state.
        assert!(blob_path.is_file());
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
fn element_log_boundaries_survive_abort_and_torn_write_actions() {
    let (expected, _) = baseline(Mode::Elements);
    for point in [
        "checkpoint.elements.reset.truncate",
        "checkpoint.elements.reset.sync",
        "checkpoint.elements.truncate",
        "checkpoint.elements.write",
        "checkpoint.elements.sync",
    ] {
        // Only the append site can inject an actual prefix write. Truncate and
        // sync boundaries exercise abort recovery, not torn-write recovery.
        let actions: &[&str] = if point == "checkpoint.elements.write" {
            &["abort", "partial:3"]
        } else {
            &["abort"]
        };
        for &action in actions {
            let fixture = fixture(Mode::Elements);
            let output = checkpointed_run(&fixture)
                .env("EXECUTE_FAILPOINT", format!("{point}:1:{action}"))
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "{point}:{action} did not terminate the checkpointed run"
            );
            check_resume(&fixture, &expected, &format!("{point}:{action}"));
        }
    }
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

#[test]
fn element_runs_with_failed_checkpoint_writes_resume_or_refuse_clearly() {
    fail_writes(Mode::Elements);
}
