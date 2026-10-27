//! Double interrupt during cleanup (#554): a repeat Ctrl-C while a `finally` or
//! `on exit` body runs aborts the cleanup at once and kills its running command
//! instead of waiting out the 5 s cleanup budget. Unix-only.
//!
//! The acceptance probe runs the child under a real PTY and sends two `\x03`
//! bytes through the terminal line discipline, so both interrupts arrive as
//! SIGINT to the foreground process group exactly like typed Ctrl-C keys. The
//! second Ctrl-C is gated on marker files (no guessed delays decide when the
//! run is interruptible) and the exit after it is bounded at 200 ms with
//! status 130 and a `cleanup interrupted` diagnostic. The direct-`kill`
//! variants below are supplementary regression cover for `finally` and
//! first-interrupt semantics with the looser 4 s bound mirrored from
//! `lane_b_drain_signals.rs`.
//!
//! The Windows signal path shares the same flag (`src/platform/windows.rs`)
//! but still needs verification on Windows CI: no PTY or console Ctrl-C
//! evidence has been collected there, and its supervisor polls on a 100 ms
//! fallback interval rather than a wake descriptor.
#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::os::fd::RawFd;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// `sh -c` body that records `marker` immediately before execing a 30-second
/// sleep, so a test can tell exactly when the sleep is the running command.
fn sleep_body() -> String {
    serde_json::to_string("touch \"$1\"; exec sleep 30").unwrap()
}

fn touch_body() -> String {
    serde_json::to_string("touch \"$1\"").unwrap()
}

fn on_exit_program(started: &Path, entered: &Path, finished: &Path) -> String {
    let sleep = sleep_body();
    let touch = touch_body();
    format!(
        "on exit\n  run \"sh\" with arguments [\"-c\", {sleep}, \"idle\", {:?}]\n  run \"sh\" with arguments [\"-c\", {touch}, \"idle\", {:?}]\nrun \"sh\" with arguments [\"-c\", {sleep}, \"idle\", {:?}]\n",
        entered.display().to_string(),
        finished.display().to_string(),
        started.display().to_string(),
    )
}

fn finally_program(started: &Path, entered: &Path, finished: &Path) -> String {
    let sleep = sleep_body();
    let touch = touch_body();
    format!(
        "try\n  run \"sh\" with arguments [\"-c\", {sleep}, \"idle\", {:?}]\nfinally\n  run \"sh\" with arguments [\"-c\", {sleep}, \"idle\", {:?}]\n  run \"sh\" with arguments [\"-c\", {touch}, \"idle\", {:?}]\n",
        started.display().to_string(),
        entered.display().to_string(),
        finished.display().to_string(),
    )
}

/// Wait until `path` exists. Markers are written immediately before the sleep
/// they gate, so no guessed delay decides when the run is interruptible; the
/// short settle only lets the `exec` after the marker take over.
fn wait_for_marker(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(Instant::now() < deadline, "the run never reached {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
}

fn interrupt(child: &Child) {
    // SAFETY: the pid belongs to a child this test spawned and has not yet reaped.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0);
}

fn wait_for_exit(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stderr_of(child: &mut Child) -> String {
    let mut text = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// Drive the acceptance scenario: interrupt the main sleep so cleanup starts,
/// then interrupt again once the cleanup sleep is running. Returns the exit
/// status, the time from the second interrupt to exit, and stderr.
fn double_interrupt(
    directory: &Path,
    program: &str,
    started: &Path,
    entered: &Path,
) -> (ExitStatus, Duration, String) {
    fs::write(directory.join("cleanup.program"), program).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(directory.join("cleanup.program"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_marker(started, "its main sleep");
    interrupt(&child);
    wait_for_marker(entered, "its cleanup sleep");
    let second = Instant::now();
    interrupt(&child);
    let Some(status) = wait_for_exit(&mut child, Duration::from_secs(10)) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the second interrupt did not abort the cleanup sleep");
    };
    let elapsed = second.elapsed();
    let stderr = stderr_of(&mut child);
    (status, elapsed, stderr)
}

fn check_aborted_at_once(status: ExitStatus, elapsed: Duration, stderr: &str, finished: &Path) {
    // Status 130 like any interrupt; the point is it arrives at once instead of
    // after the 5 s cleanup budget or the 30 s cleanup sleep. Four seconds is
    // strictly under both, mirroring the 4 s bound for a 5 s budget in
    // `lane_b_drain_signals.rs`.
    assert_eq!(status.code(), Some(130), "stderr:\n{stderr}");
    assert!(
        elapsed < Duration::from_secs(4),
        "the run waited out its cleanup instead of aborting: {elapsed:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("cleanup interrupted"),
        "the diagnostic must name the interruption, stderr:\n{stderr}"
    );
    assert!(
        !finished.exists(),
        "the cleanup ran past the second interrupt"
    );
}

#[test]
fn double_interrupt_during_on_exit_aborts_cleanup_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started.txt");
    let entered = directory.path().join("entered-cleanup.txt");
    let finished = directory.path().join("finished.txt");
    let (status, elapsed, stderr) = double_interrupt(
        directory.path(),
        &on_exit_program(&started, &entered, &finished),
        &started,
        &entered,
    );
    check_aborted_at_once(status, elapsed, &stderr, &finished);
}

#[test]
fn double_interrupt_during_finally_aborts_cleanup_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started.txt");
    let entered = directory.path().join("entered-cleanup.txt");
    let finished = directory.path().join("finished.txt");
    let (status, elapsed, stderr) = double_interrupt(
        directory.path(),
        &finally_program(&started, &entered, &finished),
        &started,
        &entered,
    );
    check_aborted_at_once(status, elapsed, &stderr, &finished);
}

#[test]
fn single_interrupt_still_runs_on_exit_cleanup() {
    // First-interrupt semantics: one Ctrl-C unwinds into cleanup and the body
    // runs to completion with no "cleanup interrupted" diagnostic.
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started.txt");
    let cleaned = directory.path().join("cleaned.txt");
    let program = format!(
        "on exit\n  run \"sh\" with arguments [\"-c\", {}, \"idle\", {:?}]\nrun \"sh\" with arguments [\"-c\", {}, \"idle\", {:?}]\n",
        touch_body(),
        cleaned.display().to_string(),
        sleep_body(),
        started.display().to_string(),
    );
    fs::write(directory.path().join("cleanup.program"), program).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(directory.path().join("cleanup.program"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_marker(&started, "its main sleep");
    interrupt(&child);
    let Some(status) = wait_for_exit(&mut child, Duration::from_secs(10)) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("a single interrupt never finished cleanup");
    };
    let stderr = stderr_of(&mut child);
    assert_eq!(status.code(), Some(130), "stderr:\n{stderr}");
    assert!(
        cleaned.exists(),
        "a single interrupt must still run cleanup, stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("cleanup interrupted"),
        "one interrupt is not a repeat interrupt, stderr:\n{stderr}"
    );
}

/// Spawn the acceptance program under a real PTY so a `\x03` byte travels the
/// terminal line discipline and arrives as SIGINT to the foreground process
/// group, exactly like a typed Ctrl-C. Returns the child and the parent-side
/// master fd; the child's stdio is the slave.
fn spawn_pty(directory: &Path, program: &str) -> (Child, RawFd) {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    fs::write(directory.join("cleanup.program"), program).unwrap();
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: out-pointers are valid locals; default termios and window size.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(
        opened,
        0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: master is the owned parent side; fetching its flags is safe.
    let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
    assert!(flags >= 0, "pty fcntl failed");
    // SAFETY: enabling nonblocking reads on the parent side only.
    let nonblocking = unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_eq!(nonblocking, 0, "pty nonblocking setup failed");

    // SAFETY: slave is owned; wrap it once, then dup it for the other stdios.
    let stdin_fd = unsafe { OwnedFd::from_raw_fd(slave) };
    // SAFETY: dups of the live slave descriptor.
    let stdout_raw = unsafe { libc::dup(stdin_fd.as_raw_fd()) };
    assert!(stdout_raw >= 0, "pty stdout dup failed");
    // SAFETY: dups of the live slave descriptor.
    let stderr_raw = unsafe { libc::dup(stdin_fd.as_raw_fd()) };
    assert!(stderr_raw >= 0, "pty stderr dup failed");
    let (stdout_fd, stderr_fd) =
        // SAFETY: both fds came from successful dup calls above.
        unsafe { (OwnedFd::from_raw_fd(stdout_raw), OwnedFd::from_raw_fd(stderr_raw)) };

    let master_in_child = master;
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .arg("run")
        .arg(directory.join("cleanup.program"))
        .stdin(Stdio::from(stdin_fd))
        .stdout(Stdio::from(stdout_fd))
        .stderr(Stdio::from(stderr_fd));
    // SAFETY: the hook runs after fork before exec and calls only
    // async-signal-safe functions. The slave redirections are already on
    // fd 0/1/2, so fd 0 names the slave for the controlling-terminal
    // handshake; the parent-side master is closed in the child.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let group = libc::getpid();
            let _ = libc::tcsetpgrp(libc::STDIN_FILENO, group);
            libc::close(master_in_child);
            Ok(())
        });
    }
    let child = command.spawn().expect("spawning the run under a pty");
    (child, master)
}

/// Type one Ctrl-C key: the line discipline delivers SIGINT to the run's
/// foreground process group.
fn pty_interrupt(master: RawFd) {
    let byte = [3u8];
    // SAFETY: master is the live nonblocking parent side of the pty.
    let written = unsafe { libc::write(master, byte.as_ptr().cast(), 1) };
    assert_eq!(written, 1, "writing Ctrl-C to the pty failed");
}

/// Drain whatever the run printed to the slave without blocking: the master
/// is nonblocking, and after the child exits the slave is gone, so reads end
/// with `EAGAIN`/`EIO` instead of hanging.
fn pty_output(master: RawFd) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        // SAFETY: chunk is a live buffer of its length; master is nonblocking.
        let read = unsafe { libc::read(master, chunk.as_mut_ptr().cast(), chunk.len()) };
        if read > 0 {
            bytes.extend_from_slice(&chunk[..read as usize]);
        } else {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn pty_double_ctrl_c_during_on_exit_exits_within_200ms() {
    // Acceptance scenario (#554): Ctrl-C, then Ctrl-C again during a
    // `sleep 30` inside `on exit`. The run exits within 200 ms with status
    // 130 and a `cleanup interrupted` diagnostic instead of waiting out the
    // 5 s cleanup budget or the 30 s sleep.
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started.txt");
    let entered = directory.path().join("entered-cleanup.txt");
    let finished = directory.path().join("finished.txt");
    let (mut child, master) = spawn_pty(
        directory.path(),
        &on_exit_program(&started, &entered, &finished),
    );
    let done = |child: &mut Child, master: RawFd, why: &str| {
        let _ = child.kill();
        let _ = child.wait();
        // SAFETY: the parent-side descriptor is owned by this test.
        unsafe {
            libc::close(master);
        }
        panic!("{why}");
    };
    wait_for_marker(&started, "its main sleep");
    pty_interrupt(master);
    wait_for_marker(&entered, "its cleanup sleep");
    let second = Instant::now();
    pty_interrupt(master);
    let Some(status) = wait_for_exit(&mut child, Duration::from_secs(10)) else {
        done(
            &mut child,
            master,
            "the second Ctrl-C did not abort the cleanup sleep",
        );
        unreachable!();
    };
    let elapsed = second.elapsed();
    // Drain after the exit so output collection cannot stretch the measured
    // second-interrupt-to-exit interval above.
    let output = pty_output(master);
    // SAFETY: the parent-side descriptor is owned by this test.
    unsafe {
        libc::close(master);
    }
    assert_eq!(status.code(), Some(130), "pty output:\n{output}");
    assert!(
        elapsed < Duration::from_millis(200),
        "the run did not exit within 200 ms of the second Ctrl-C: {elapsed:?}\npty output:\n{output}"
    );
    assert!(
        output.contains("cleanup interrupted"),
        "the diagnostic must name the interruption, pty output:\n{output}"
    );
    assert!(
        !finished.exists(),
        "the cleanup ran past the second Ctrl-C, pty output:\n{output}"
    );
}
