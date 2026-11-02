#![cfg(feature = "live")]
//! `execute run FILE --control`: the finite run is hosted under the live control
//! endpoint. The round trip uses the endpoint the host prints, so it runs wherever
//! the live host does (unix socket or Windows named pipe).

use std::process::{Command, Output, Stdio};

fn execute(arguments: &[&str], directory: &std::path::Path) -> Output {
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

const PLAIN: &str =
    "accept name as text with default \"x\"\nrun \"printf\" with arguments [\"finished\"]\n";
const TRIGGERED: &str = "every 1 second\n  run \"printf\" with arguments [\"tick\"]\n";

#[test]
fn run_control_refuses_options_the_live_host_cannot_honour() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), PLAIN).unwrap();
    let cases: &[(&[&str], &str)] = &[
        (&["--checkpoint", "ckpt"], "--checkpoint"),
        (&["--once"], "--once"),
        (&["--trace"], "--trace"),
        (&["--headless"], "--headless"),
        (&["--max-runs", "2"], "--max-runs"),
        (&["--missed-ticks", "skip"], "--missed-ticks"),
        (&["--max-catch-up", "2"], "--max-catch-up"),
        (&["--calendar-dedup", "instant"], "--calendar-dedup"),
        (&["--total-timeout-ms", "1000"], "--total-timeout-ms"),
        (&["--retry-uncertain"], "--retry-uncertain"),
        (&["--max-record-bytes", "1024"], "--max-record-bytes"),
        (&["--record", "records"], "--record"),
        (&["--what-if"], "--what-if"),
        (&["--policy-header"], "--policy-header"),
        (&["--allow", "writes"], "--allow"),
    ];
    for (options, named) in cases {
        let mut arguments = vec!["run", "tool.program", "--control"];
        arguments.extend_from_slice(options);
        let output = execute(&arguments, directory.path());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{arguments:?} should be refused");
        assert!(
            stderr.contains(named),
            "{arguments:?} should name {named}: {stderr}"
        );
        assert!(!stderr.contains("control endpoint"), "{stderr}");
        assert!(
            output.stdout.is_empty(),
            "{arguments:?} printed program output"
        );
    }
}

#[test]
fn run_control_refuses_triggered_programs_and_positional_arguments() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), TRIGGERED).unwrap();
    let triggered = execute(&["run", "tool.program", "--control"], directory.path());
    assert!(!triggered.status.success());
    assert!(
        String::from_utf8_lossy(&triggered.stderr).contains("a triggered program"),
        "{}",
        String::from_utf8_lossy(&triggered.stderr)
    );

    std::fs::write(directory.path().join("tool.program"), PLAIN).unwrap();
    let positional = execute(
        &["run", "tool.program", "--control", "--", "extra"],
        directory.path(),
    );
    // The program's own input validation runs before the host starts, so a stray
    // positional argument is refused without a control endpoint or program output.
    assert!(!positional.status.success());
    let stderr = String::from_utf8_lossy(&positional.stderr);
    assert!(
        stderr.contains("accepts at most 0 positional arguments"),
        "{stderr}"
    );
    assert!(!stderr.contains("control endpoint"), "{stderr}");
    assert!(positional.stdout.is_empty());
}

#[test]
fn control_is_accepted_only_by_run_and_only_once() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), PLAIN).unwrap();
    let check = execute(&["check", "tool.program", "--control"], directory.path());
    assert!(!check.status.success());
    assert!(String::from_utf8_lossy(&check.stderr).contains("unknown or unsupported option"));

    let twice = execute(
        &["run", "tool.program", "--control", "--control"],
        directory.path(),
    );
    assert!(!twice.status.success());
    assert!(String::from_utf8_lossy(&twice.stderr).contains("--control may be specified only once"));
}

/// Stderr without the one line `--control` adds for the endpoint address.
fn without_endpoint(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| !line.starts_with("execute: control endpoint "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run `execute run tool.program` with or without `--control`; the pair must agree.
fn run_both(directory: &std::path::Path, source: &str) -> (Output, Output) {
    std::fs::write(directory.join("tool.program"), source).unwrap();
    let plain = execute(&["run", "tool.program"], directory);
    let hosted = execute(&["run", "tool.program", "--control"], directory);
    (plain, hosted)
}

#[test]
fn run_control_streams_each_command_like_plain_run() {
    let directory = tempfile::tempdir().unwrap();
    let (plain, hosted) = run_both(
        directory.path(),
        "run printf with message \"first\"\nrun printf with message \"second\"\n",
    );
    assert!(plain.status.success());
    assert_eq!(String::from_utf8_lossy(&plain.stdout), "first\nsecond\n");
    assert!(
        hosted.status.success(),
        "{}",
        String::from_utf8_lossy(&hosted.stderr)
    );
    // Both commands reach stdout as they run, not only the last one.
    assert_eq!(hosted.stdout, plain.stdout);
}

#[test]
fn run_control_keeps_streamed_stdout_when_a_command_fails() {
    let directory = tempfile::tempdir().unwrap();
    let (plain, hosted) = run_both(
        directory.path(),
        "run printf with message \"before\"\nrun false\nrun printf with message \"never\"\n",
    );
    assert_eq!(plain.status.code(), Some(1));
    assert_eq!(hosted.status.code(), plain.status.code());
    assert_eq!(hosted.stdout, plain.stdout);
    assert_eq!(String::from_utf8_lossy(&hosted.stdout), "before\n");
    let stderr = String::from_utf8_lossy(&hosted.stderr);
    assert!(stderr.contains("applet \"false\" failed"), "{stderr}");
}

#[test]
fn run_control_exit_with_status_sets_the_exit_status_and_skips_later_steps() {
    let directory = tempfile::tempdir().unwrap();
    let (plain, hosted) = run_both(
        directory.path(),
        "run printf with message \"a\"\nexit with status 7\nrun printf with message \"never\"\n",
    );
    assert_eq!(plain.status.code(), Some(7));
    assert_eq!(
        hosted.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&hosted.stderr)
    );
    assert_eq!(hosted.stdout, plain.stdout);
    assert_eq!(String::from_utf8_lossy(&hosted.stdout), "a\n");
}

#[test]
fn run_control_fail_with_message_writes_the_message_and_exits_with_its_status() {
    let directory = tempfile::tempdir().unwrap();
    let (plain, hosted) = run_both(
        directory.path(),
        "fail with message \"boom\" and status 4\n",
    );
    assert_eq!(plain.status.code(), Some(4));
    assert_eq!(hosted.status.code(), Some(4));
    assert!(hosted.stdout.is_empty());
    assert_eq!(
        without_endpoint(&hosted.stderr),
        without_endpoint(&plain.stderr)
    );
    assert!(String::from_utf8_lossy(&hosted.stderr).contains("boom"));
}

#[test]
fn run_control_does_not_cap_streamed_output_at_the_live_limit() {
    // Two MiB is past the live 1 MiB per-command capture limit. Uncaptured stdout
    // streams, so it must arrive whole.
    let directory = tempfile::tempdir().unwrap();
    let big = vec![b'a'; 2 * 1024 * 1024];
    std::fs::write(directory.path().join("big.txt"), &big).unwrap();
    let (plain, hosted) = run_both(directory.path(), "run cat with path \"big.txt\"\n");
    assert!(plain.status.success());
    assert!(
        hosted.status.success(),
        "{}",
        String::from_utf8_lossy(&hosted.stderr)
    );
    assert_eq!(plain.stdout.len(), big.len());
    assert_eq!(hosted.stdout.len(), big.len());
}

#[test]
fn live_serve_still_refuses_exit_and_fail() {
    // A served program keeps its state and never ends, so exit and fail stay refused
    // there. Validation fails before the socket is bound.
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("tool.program"),
        "run printf with message \"a\"\nexit with status 7\n",
    )
    .unwrap();
    let socket = directory.path().join("tool.sock");
    let output = execute(
        &[
            "live",
            "serve",
            "tool.program",
            "--socket",
            &socket.to_string_lossy(),
        ],
        directory.path(),
    );
    assert!(!output.status.success());
    // The live command reports its error as one JSON object on stdout.
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("cannot exit or fail"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!socket.exists());
}

mod round_trip {
    use super::*;
    use serde_json::Value;
    use std::io::{BufRead, BufReader, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ExitStatus};
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const PREFIX: &str = "execute: control endpoint ";

    struct Hosted {
        child: Child,
        endpoint: PathBuf,
        stdout: JoinHandle<String>,
        stderr: JoinHandle<Vec<String>>,
    }

    fn host(directory: &Path, program: &str, extra: &[&str]) -> Hosted {
        std::fs::write(directory.join("tool.program"), program).unwrap();
        let mut arguments = vec!["run", "tool.program", "--control"];
        arguments.extend_from_slice(extra);
        let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(&arguments)
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

    fn json(output: &Output) -> Value {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "expected JSON on stdout ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    fn live_read(endpoint: &Path, pointer: &str) -> Value {
        let endpoint = endpoint.to_string_lossy().into_owned();
        let output = execute_here(&["live", "read", pointer, "--socket", endpoint.as_str()]);
        let object = json(&output);
        assert_eq!(object["ok"], true, "{object}");
        object
    }

    fn attach(endpoint: &Path, extra: &[&str]) -> Value {
        let endpoint = endpoint.to_string_lossy().into_owned();
        let mut arguments: Vec<&str> = vec!["attach", endpoint.as_str()];
        arguments.extend_from_slice(extra);
        json(&execute_here(&arguments))
    }

    fn execute_here(arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(arguments)
            .env_remove("EXECUTE_MODELS_OFFLINE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("start execute")
    }

    fn runtime_status(endpoint: &Path) -> String {
        let status = attach(endpoint, &[]);
        status["runtime"]["status"]
            .as_str()
            .unwrap_or_else(|| panic!("no runtime status in {status}"))
            .to_owned()
    }

    #[test]
    fn run_control_reads_bindings_and_cursor_and_pauses_and_resumes_the_run() {
        let directory = tempfile::tempdir().unwrap();
        let program = "accept name as text with default \"x\"\n\
                       run \"sleep\" with arguments [\"2\"]\n\
                       run \"printf\" with arguments [\"finished\"]\n";
        let mut hosted = host(directory.path(), program, &["--input", "name=ready"]);
        let endpoint = hosted.endpoint.clone();

        let bindings = live_read(&endpoint, "/bindings");
        assert!(bindings.to_string().contains("ready"), "{bindings}");
        let cursor = live_read(&endpoint, "/cursor");
        assert!(cursor.get("result").is_some(), "{cursor}");

        let paused = attach(&endpoint, &["--control", "pause"]);
        assert_eq!(paused["applied"], true, "{paused}");
        let deadline = Instant::now() + Duration::from_secs(20);
        while runtime_status(&endpoint) != "paused" {
            assert!(Instant::now() < deadline, "run did not pause");
            thread::sleep(Duration::from_millis(50));
        }

        let resumed = attach(&endpoint, &["--control", "resume"]);
        assert_eq!(resumed["applied"], true, "{resumed}");

        let status = wait_exit(&mut hosted.child, Duration::from_secs(60));
        let stdout = hosted.stdout.join().unwrap();
        let stderr = hosted.stderr.join().unwrap();
        assert!(status.success(), "{stderr:?}");
        assert_eq!(stdout, "finished");
        assert!(
            stderr.iter().any(|line| line.starts_with(PREFIX)),
            "{stderr:?}"
        );
        assert!(!endpoint.exists(), "the endpoint should be removed on exit");
    }

    #[test]
    fn control_stop_cancels_a_hosted_run_and_exits_nonzero() {
        let directory = tempfile::tempdir().unwrap();
        let mut hosted = host(
            directory.path(),
            "run \"sleep\" with arguments [\"30\"]\n",
            &[],
        );
        let endpoint = hosted.endpoint.clone();

        let stopped = attach(&endpoint, &["--control", "stop"]);
        assert_eq!(stopped["applied"], true, "{stopped}");

        let status = wait_exit(&mut hosted.child, Duration::from_secs(60));
        let stderr = hosted.stderr.join().unwrap();
        assert!(!status.success());
        assert!(
            stderr.iter().any(|line| line.contains("cancelled")),
            "{stderr:?}"
        );
        assert!(!endpoint.exists(), "the endpoint should be removed on exit");
    }
}

#[test]
fn run_control_honours_the_per_loop_iteration_limit() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("loop.program"),
        "repeat 4 times\n  use value \"done\"\n",
    )
    .unwrap();
    let limited = execute(
        &[
            "run",
            "loop.program",
            "--control",
            "--max-loop-iterations",
            "3",
        ],
        directory.path(),
    );
    assert!(!limited.status.success());
    assert!(
        String::from_utf8_lossy(&limited.stderr).contains("budget of 3"),
        "{}",
        String::from_utf8_lossy(&limited.stderr)
    );
    let allowed = execute(
        &[
            "run",
            "loop.program",
            "--control",
            "--max-loop-iterations",
            "4",
        ],
        directory.path(),
    );
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert_eq!(allowed.stdout, b"done");
}

#[test]
fn run_control_endpoint_uses_a_private_randomized_directory() {
    const PREFIX: &str = "execute: control endpoint ";
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), PLAIN).unwrap();
    let mut endpoints = Vec::new();
    for _ in 0..2 {
        let output = execute(&["run", "tool.program", "--control"], directory.path());
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let endpoint = stderr
            .lines()
            .find_map(|line| line.strip_prefix(PREFIX))
            .expect("run --control prints its endpoint")
            .to_owned();
        endpoints.push(endpoint);
    }
    assert_ne!(
        endpoints[0], endpoints[1],
        "each hosted run gets a fresh endpoint"
    );
    for endpoint in &endpoints {
        let socket = std::path::Path::new(endpoint);
        let parent = socket.parent().expect("the endpoint has a parent");
        assert!(parent.is_dir(), "the endpoint directory exists");
        assert!(
            !socket.exists(),
            "the endpoint is removed when the run exits"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::symlink_metadata(parent).unwrap();
            assert!(!metadata.file_type().is_symlink());
            assert!(metadata.is_dir());
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o700,
                "the endpoint directory is private: {parent:?}"
            );
        }
    }
}
