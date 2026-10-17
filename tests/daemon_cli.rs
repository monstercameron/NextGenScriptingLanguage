//! `execute daemon` (#77, first slice): status, serve, stop, directory checks, and the
//! `run --daemon` probe. Program output must match a cold run exactly.
#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn daemon_command(runtime_dir: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.env("XDG_RUNTIME_DIR", runtime_dir);
    command.env_remove("EXECUTE_DAEMON");
    command.arg("daemon").args(arguments);
    command
}

fn daemon_output(runtime_dir: &Path, arguments: &[&str]) -> Output {
    daemon_command(runtime_dir, arguments).output().unwrap()
}

/// The socket `daemon serve` uses when no `--socket` is given under `runtime_dir`.
fn default_socket_under(runtime_dir: &Path) -> PathBuf {
    let uid = unsafe { libc::geteuid() };
    runtime_dir
        .join(format!("execute-{uid}"))
        .join("daemon.sock")
}

/// Kills the background daemon if a test fails before stopping it.
struct Daemon(Option<Child>);

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn serve_in_background(runtime_dir: &Path) -> Daemon {
    let child = daemon_command(runtime_dir, &["serve"])
        .spawn()
        .expect("spawn daemon serve");
    let mut daemon = Daemon(Some(child));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = daemon_output(runtime_dir, &["status"]);
        let reply: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        if reply["running"] == true {
            return daemon;
        }
        if Instant::now() > deadline {
            let _ = daemon.0.as_mut().unwrap().kill();
            panic!("daemon did not start within 10 s");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn run_program(runtime_dir: &Path, directory: &Path, extra: &[&str]) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, r#"run printf with message "out""#).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.env("XDG_RUNTIME_DIR", runtime_dir);
    command.env_remove("EXECUTE_DAEMON");
    command.current_dir(directory);
    command.arg("run").arg(&path);
    command.arg("--manifest-dir").arg(manifests());
    command.args(extra);
    command.output().unwrap()
}

#[test]
fn daemon_status_reports_not_running_when_no_socket_exists() {
    let runtime = tempfile::tempdir().unwrap();
    let output = daemon_output(runtime.path(), &["status"]);
    assert!(output.status.success(), "{:?}", output);
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["running"], false);
    assert_eq!(reply["protocol"], "execute.daemon/1");
}

#[test]
fn daemon_serve_answers_status_and_stop_removes_its_socket() {
    let runtime = tempfile::tempdir().unwrap();
    let socket = default_socket_under(runtime.path());
    let mut daemon = serve_in_background(runtime.path());

    let status = daemon_output(runtime.path(), &["status"]);
    let reply: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(reply["running"], true);
    assert!(socket.exists(), "socket at {}", socket.display());

    let stop = daemon_output(runtime.path(), &["stop"]);
    assert!(stop.status.success(), "{:?}", stop);
    let child = daemon.0.as_mut().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        assert!(Instant::now() < deadline, "daemon did not exit after stop");
        thread::sleep(Duration::from_millis(20));
    };
    assert!(exit.success());
    assert!(
        !socket.exists(),
        "socket left behind at {}",
        socket.display()
    );
    daemon.0 = None;
}

#[test]
fn daemon_serve_refuses_a_directory_that_is_not_private() {
    let runtime = tempfile::tempdir().unwrap();
    let directory = runtime.path().join("open");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = directory.join("daemon.sock");
    let output = daemon_output(
        runtime.path(),
        &["serve", "--socket", socket.to_str().unwrap()],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("private"), "{stderr}");
    assert!(!socket.exists());
}

#[test]
fn run_with_daemon_flag_prints_the_same_output_and_notes_the_daemon() {
    let runtime = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cold = run_program(runtime.path(), work.path(), &[]);
    assert!(cold.status.success(), "{:?}", cold);

    let _daemon = serve_in_background(runtime.path());
    let warm = run_program(runtime.path(), work.path(), &["--daemon"]);
    assert!(warm.status.success(), "{:?}", warm);
    assert_eq!(warm.stdout, cold.stdout);
    let stderr = String::from_utf8_lossy(&warm.stderr);
    assert!(stderr.contains("daemon is running"), "{stderr}");
}

#[test]
fn run_with_daemon_flag_without_a_daemon_is_silent_and_cold() {
    let runtime = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let output = run_program(runtime.path(), work.path(), &["--daemon"]);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn daemon_status_reports_not_running_without_creating_a_missing_directory() {
    let runtime = TempDir::new().unwrap();
    let missing = runtime.path().join("nope");
    let output = daemon_output(
        runtime.path(),
        &[
            "status",
            "--socket",
            missing.join("daemon.sock").to_str().unwrap(),
        ],
    );
    assert!(output.status.success());
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["running"], false);
    assert!(!missing.exists());
}

#[test]
fn a_socket_in_a_non_private_directory_is_never_contacted() {
    let runtime = tempfile::tempdir().unwrap();
    let directory = runtime.path().join("open");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = directory.join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let contacted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&contacted);
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let _ = stream
                .write_all(b"{\"ok\":true,\"protocol\":\"execute.daemon/1\",\"running\":true}\n");
        }
    });
    let path = socket.to_str().unwrap();

    let status = daemon_output(runtime.path(), &["status", "--socket", path]);
    assert!(status.status.success(), "{:?}", status);
    let reply: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(reply["running"], false, "{reply}");
    let reason = reply["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("private"), "{reply}");

    let stop = daemon_output(runtime.path(), &["stop", "--socket", path]);
    assert!(!stop.status.success(), "stop must not succeed: {:?}", stop);
    let stderr = String::from_utf8_lossy(&stop.stderr);
    assert!(
        stderr.contains("refusing to use the daemon socket"),
        "{stderr}"
    );
    assert!(stderr.contains(path), "{stderr}");

    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        contacted.load(Ordering::SeqCst),
        0,
        "a request reached the untrusted socket"
    );
}
