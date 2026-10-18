#![cfg(feature = "failpoints")]

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn fixture(source: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("workflow.program"), source).unwrap();
    for name in ["fixture.ok", "fixture.panic"] {
        fs::write(
            directory.path().join(format!("{name}.json")),
            serde_json::json!({
                "name":name, "binary":env!("CARGO_BIN_EXE_execute"),
                "arguments":[{"literal":"--version"}],
            "contract":{"version":1,"effects":[],"capabilities":[],
                "output":{"type":"text","encoding":"raw","trim_final_lf":true}}
            })
            .to_string(),
        )
        .unwrap();
    }
    directory
}

// The same file-backed output and bounded wait pattern as resume.rs: an abort
// must not leave a test waiting on pipe handles inherited by descendants.
fn run(directory: &Path, arguments: &[&str], panic: bool, input: &[u8]) -> Output {
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory)
        .env("XDG_RUNTIME_DIR", directory)
        .env_remove("EXECUTE_DAEMON")
        .args(arguments)
        .arg("--manifest-dir")
        .arg(directory)
        .stdout(Stdio::from(stdout.try_clone().unwrap()))
        .stderr(Stdio::from(stderr.try_clone().unwrap()))
        .stdin(Stdio::piped());
    if panic {
        command.env("EXECUTE_TEST_PANIC_APPLET", "fixture.panic");
        // Debug tests exercise the hook's abort path. Release tests leave the
        // hook alone and rely on the binary's actual panic=abort strategy.
        if cfg!(debug_assertions) {
            command.env("EXECUTE_TEST_PANIC_ABORT", "1");
        }
        if matches!(
            arguments.first(),
            Some(&"daemon" | &"live" | &"repl" | &"lsp" | &"dap")
        ) {
            command.env("EXECUTE_TEST_PANIC_HOST", arguments[0]);
        }
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("panic fixture exceeded its ten-second deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    stdout.seek(SeekFrom::Start(0)).unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.read_to_end(&mut out).unwrap();
    stderr.read_to_end(&mut err).unwrap();
    Output {
        status,
        stdout: out,
        stderr: err,
    }
}

fn diagnostic(output: &Output, host: &str) -> serde_json::Value {
    assert!(
        !output.status.success(),
        "panic injection did not terminate the host"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(output.status.signal(), Some(libc::SIGABRT));
    }
    let lines: Vec<_> = output
        .stderr
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(lines[0])
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(value["protocol"], "execute.panic/1");
    assert_eq!(value["host"], host);
    assert_eq!(value["action"], "abort");
    value
}

#[test]
#[cfg(unix)]
fn checkpoint_resumes_after_a_structured_panic_abort() {
    let directory = fixture("run fixture.ok\nrun fixture.panic\n");
    let output = run(
        directory.path(),
        &["run", "workflow.program", "--checkpoint", "checkpoint"],
        true,
        b"",
    );
    let record = diagnostic(&output, "checkpoint");
    assert_eq!(record["line"], 2);
    assert!(record["step"].is_string());
    let resumed = run(
        directory.path(),
        &[
            "resume",
            "workflow.program",
            "--checkpoint",
            "checkpoint",
            "--retry-uncertain",
        ],
        false,
        b"",
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
}

#[test]
fn listener_handler_panic_aborts_with_one_diagnostic() {
    let directory =
        fixture("listen to stdin\nwhenever a line arrives as record\n  run fixture.panic\n");
    diagnostic(
        &run(
            directory.path(),
            &["run", "workflow.program"],
            true,
            b"event\n",
        ),
        "listener",
    );
}

#[test]
fn task_worker_panic_aborts_its_listener_host() {
    let directory = fixture("listen to stdin\nwhenever a line arrives as record\n  parallel at most 2 as results\n    run fixture.panic as doomed\n");
    let value = diagnostic(
        &run(
            directory.path(),
            &["run", "workflow.program"],
            true,
            b"event\n",
        ),
        "listener",
    );
    assert!(value["thread"]
        .as_str()
        .is_some_and(|name| name.starts_with("flow-task-")));
}

#[test]
fn daemon_parent_panic_emits_one_structured_abort_diagnostic() {
    let directory = fixture("");
    diagnostic(
        &run(directory.path(), &["daemon", "serve"], true, b""),
        "daemon",
    );
}

#[cfg(unix)]
#[test]
fn aborted_forwarded_run_leaves_daemon_available_for_the_next_request() {
    struct Daemon(std::process::Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let directory = fixture("run fixture.panic\n");
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["daemon", "serve"])
            .env("XDG_RUNTIME_DIR", directory.path())
            .env_remove("EXECUTE_DAEMON")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["daemon", "status"])
            .env("XDG_RUNTIME_DIR", directory.path())
            .output()
            .unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        if reply["running"] == true {
            break;
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited before becoming ready"
        );
        assert!(
            Instant::now() < deadline,
            "daemon did not start within ten seconds"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = run(
        directory.path(),
        &["run", "workflow.program", "--daemon"],
        true,
        b"",
    );
    // The forwarding client reports its child's signal as 128 + SIGABRT.
    assert_eq!(output.status.code(), Some(128 + libc::SIGABRT));
    let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["protocol"], "execute.panic/1");
    assert_eq!(value["host"], "daemon-run");
    assert_eq!(value["action"], "abort");
    assert!(daemon.0.try_wait().unwrap().is_none());
    let next = run(
        directory.path(),
        &["run", "workflow.program", "--daemon"],
        false,
        b"",
    );
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
}

#[cfg(feature = "live")]
#[test]
fn live_host_boundary_panic_emits_one_structured_abort_diagnostic() {
    let directory = fixture("");
    diagnostic(
        &run(directory.path(), &["live", "serve"], true, b""),
        "live",
    );
}

#[cfg(feature = "records")]
#[test]
fn repl_host_boundary_panic_emits_one_structured_abort_diagnostic() {
    let directory = fixture("");
    diagnostic(&run(directory.path(), &["repl"], true, b""), "repl");
}

#[cfg(feature = "lsp")]
#[test]
fn lsp_host_boundary_panic_emits_one_structured_abort_diagnostic() {
    let directory = fixture("");
    diagnostic(&run(directory.path(), &["lsp"], true, b""), "lsp");
}

#[cfg(feature = "dap")]
#[test]
fn dap_host_boundary_panic_emits_one_structured_abort_diagnostic() {
    let directory = fixture("");
    diagnostic(&run(directory.path(), &["dap"], true, b""), "dap");
}

#[cfg(feature = "graph")]
#[test]
fn graph_worker_panic_aborts_with_one_diagnostic() {
    let directory = fixture("graph panic\n  run fixture.panic as worker\nend graph\n");
    diagnostic(
        &run(directory.path(), &["run", "workflow.program"], true, b""),
        "graph",
    );
}
