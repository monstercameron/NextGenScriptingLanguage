//! Filesystem-operation deadlines (threat model H7, issue #579).
//!
//! A blocking open on a FIFO with no writer hangs forever: command timeouts
//! bound a child's run, but nothing bounded the open itself. Host `read`
//! paths and the in-process `fs.*` applets now open nonblocking, refuse
//! non-regular targets before any byte moves, and wait only within the
//! configurable filesystem-operation deadline. A FIFO fails with the typed
//! timeout; a device target is refused.
//!
//! All fixtures are owned temporary paths; no host processes are targeted.
//! The FIFO is created with the POSIX `mkfifo` utility so the tests add no
//! `unsafe`. `/dev/null` is only ever classified, never read: `/dev/zero`
//! is deliberately avoided because reading it is unbounded by construction.
#![cfg(unix)]

use flow::applets::{self, AppletErrorKind};
use serde_json::json;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The tests must finish in about a second (binary startup dominates); this
/// outer bound only trips when an operation hangs again.
const EXTERNAL_TIMEOUT: Duration = Duration::from_secs(10);

fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .stdin(Stdio::null())
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo {}", path.display());
}

fn is_fifo(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_fifo())
        .unwrap_or(false)
}

fn applet_args(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn write_args(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    applet_args(json!({
        "path": path,
        "content": "replacement\n",
        "expected_content_hash": "missing",
        "symlink_policy": "reject",
        "permissions_policy": "preserve",
    }))
}

/// Run the real `execute` binary the way `tests/timeout.rs` does: piped
/// stdio, polled exit, and a loud panic instead of a hang when the
/// filesystem operation ignores its deadline.
fn invoke(dir: &Path, args: &[&str]) -> (Output, Duration) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() >= EXTERNAL_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("filesystem operation hung past {EXTERNAL_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let elapsed = started.elapsed();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    use std::io::Read as _;
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    (
        Output {
            status,
            stdout,
            stderr,
        },
        elapsed,
    )
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn fs_read_fifo_fails_with_typed_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("target-fifo");
    mkfifo(&fifo);
    let started = Instant::now();
    let error = applets::execute("fs.read", &applet_args(json!({ "path": fifo })), &[])
        .expect_err("reading a FIFO must fail");
    assert!(
        started.elapsed() < EXTERNAL_TIMEOUT,
        "fs.read on a FIFO hung instead of timing out"
    );
    assert_eq!(error.kind, AppletErrorKind::Timeout, "error: {error}");
    assert_eq!(error.code, "timeout");
    assert!(
        error.message.contains("FIFO"),
        "unexpected message: {error}"
    );
    assert!(is_fifo(&fifo), "the FIFO target must be left alone");
}

#[test]
fn fs_read_device_is_refused() {
    let device = Path::new("/dev/null");
    assert!(
        std::fs::symlink_metadata(device)
            .map(|metadata| metadata.file_type().is_char_device())
            .unwrap_or(false),
        "/dev/null must be a character device for this refusal test"
    );
    let error = applets::execute("fs.read", &applet_args(json!({ "path": device })), &[])
        .expect_err("reading a device must be refused");
    assert_eq!(error.kind, AppletErrorKind::Permanent, "error: {error}");
    assert!(
        error.message.contains("not a regular file"),
        "unexpected message: {error}"
    );
}

#[test]
fn fs_write_atomic_to_fifo_fails_before_any_effect() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("target-fifo");
    mkfifo(&fifo);
    let before: Vec<PathBuf> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into())
        .collect();
    let error = applets::execute("fs.write_atomic", &write_args(&fifo), &[])
        .expect_err("writing a FIFO must fail");
    assert_eq!(error.kind, AppletErrorKind::Timeout, "error: {error}");
    assert!(is_fifo(&fifo), "the FIFO target must not be replaced");
    // The refused write must not create, truncate, or partially write the
    // target. The advisory lock file is the operation's normal pre-protocol
    // (it is also created on conflict failures); no temporary replacement
    // file may be left behind.
    let after: Vec<PathBuf> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into())
        .collect();
    for entry in &after {
        let name = entry.to_string_lossy();
        assert!(
            !name.ends_with(".tmp"),
            "temporary replacement file left behind: {name}"
        );
    }
    assert!(
        after.len() <= before.len() + 1,
        "refused write created unexpected files: {after:?}"
    );
}

#[test]
fn fs_ensure_file_on_fifo_is_refused_without_hanging() {
    // `fs.ensure_file` stat-checks before opening, so an existing FIFO is
    // refused up front (the bounded open behind it still guards the
    // check-to-open race). Either way it must return at once, fail closed,
    // and leave the FIFO alone.
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("target-fifo");
    mkfifo(&fifo);
    let started = Instant::now();
    let error = applets::execute(
        "fs.ensure_file",
        &applet_args(json!({ "path": fifo, "content": "x\n" })),
        &[],
    )
    .expect_err("ensure on a FIFO must fail");
    assert!(
        started.elapsed() < EXTERNAL_TIMEOUT,
        "ensure on a FIFO hung instead of failing"
    );
    assert_eq!(error.kind, AppletErrorKind::Permanent, "error: {error}");
    assert!(
        error.message.contains("not a regular file"),
        "unexpected message: {error}"
    );
    assert!(is_fifo(&fifo));
}

#[test]
fn text_render_fifo_template_fails_with_typed_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("template-fifo");
    mkfifo(&fifo);
    let error = applets::execute(
        "text.render",
        &applet_args(json!({ "template_path": fifo, "values": {} })),
        &[],
    )
    .expect_err("a FIFO template must fail");
    assert_eq!(error.kind, AppletErrorKind::Timeout, "error: {error}");
    assert!(is_fifo(&fifo));
}

#[test]
fn host_read_fifo_fails_with_typed_timeout_within_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    mkfifo(&root.join("fifo"));
    std::fs::write(root.join("job.program"), "read \"fifo\"\n").unwrap();
    let (output, elapsed) = invoke(
        root,
        &[
            "job.program",
            "--fs-timeout-ms",
            "500",
            "--command-timeout-ms",
            "8000",
        ],
    );
    assert!(
        !output.status.success(),
        "reading a FIFO must fail: {}",
        stderr_text(&output)
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("command timed out"),
        "expected the typed timeout rendering, got: {stderr}"
    );
    assert!(
        stderr.contains("FIFO"),
        "expected the FIFO refusal reason, got: {stderr}"
    );
    assert!(
        elapsed < EXTERNAL_TIMEOUT,
        "read hung for {elapsed:?} instead of failing within the deadline"
    );
    assert!(is_fifo(&root.join("fifo")));
}

#[test]
fn host_save_to_fifo_is_refused_without_effect() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("input"), "owned input\n").unwrap();
    mkfifo(&root.join("fifo"));
    std::fs::write(
        root.join("job.program"),
        "read \"input\"\nthen save to \"fifo\"\n",
    )
    .unwrap();
    let (output, _) = invoke(root, &["job.program", "--command-timeout-ms", "8000"]);
    assert!(
        !output.status.success(),
        "saving to a FIFO must fail: {}",
        stderr_text(&output)
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("non-regular file"),
        "expected the non-regular refusal, got: {stderr}"
    );
    // Fail before effect: the FIFO is untouched, the input is intact, and no
    // replacement file was created next to it.
    assert!(is_fifo(&root.join("fifo")));
    assert_eq!(std::fs::read(root.join("input")).unwrap(), b"owned input\n");
    let mut entries: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, ["fifo", "input", "job.program"]);
}

#[test]
fn host_read_device_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("job.program"), "read \"/dev/null\"\n").unwrap();
    let (output, _) = invoke(root, &["job.program", "--command-timeout-ms", "8000"]);
    assert!(
        !output.status.success(),
        "reading a device must fail: {}",
        stderr_text(&output)
    );
    assert!(
        stderr_text(&output).contains("non-regular file"),
        "expected the non-regular refusal, got: {}",
        stderr_text(&output)
    );
}
