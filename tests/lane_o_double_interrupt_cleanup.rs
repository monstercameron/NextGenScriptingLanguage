//! Double interrupt during cleanup (#554): a repeat Ctrl-C while a `finally` or
//! `on exit` body runs aborts the cleanup at once and kills its running command
//! instead of waiting out the 5 s cleanup budget. Unix-only.
//!
//! The acceptance probe runs the child under a real PTY and sends two `\x03`
//! bytes through the terminal line discipline, so both interrupts arrive as
//! SIGINT to the foreground process group exactly like typed Ctrl-C keys. The
//! second Ctrl-C is gated on marker files (no guessed delays decide when the
//! run is interruptible) and the exit after it is bounded at 200 ms with
//! status 130 and a `cleanup interrupted` diagnostic. Every wait also watches
//! the run, and a fail-safe watchdog kills a stuck run, so the probes always
//! end with diagnostics instead of hanging after their child is gone. The
//! direct-`kill` variants below are supplementary regression cover for
//! `finally` and first-interrupt semantics with the looser 4 s bound mirrored
//! from `lane_b_drain_signals.rs`.
//!
//! The Windows signal path shares the same flag (`src/platform/windows.rs`)
//! but still needs verification on Windows CI: no PTY or console Ctrl-C
//! evidence has been collected there, and its supervisor polls on a 100 ms
//! fallback interval rather than a wake descriptor.
#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, AtomicBool, Mutex};
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

/// Budgets that keep every acceptance wait fail-safe. Marker and exit waits
/// only burn their full budget while the run is still alive: each wait also
/// observes the child, and the watchdog kills a stuck run at `WATCHDOG_BUDGET`,
/// so a test always ends with diagnostics instead of hanging after its child
/// is gone.
const MARKER_BUDGET: Duration = Duration::from_secs(30);
const EXIT_BUDGET: Duration = Duration::from_secs(10);
const REAP_BUDGET: Duration = Duration::from_secs(5);
const OUTPUT_DRAIN_BUDGET: Duration = Duration::from_secs(2);
const WATCHDOG_BUDGET: Duration = Duration::from_secs(50);

/// Shared ownership of one acceptance child so a fail-safe watchdog can always
/// terminate it. Every wait below observes the child through this guard, so a
/// stuck run surfaces as an early exit with diagnostics instead of a hung
/// test. The watchdog kills through the live handle under the same lock that
/// records exits, so it can never signal a recycled pid.
struct WatchedChild {
    child: Mutex<Child>,
    exited: AtomicBool,
    done: AtomicBool,
    watchdog_fired: AtomicBool,
}

impl WatchedChild {
    fn spawn(child: Child) -> Arc<Self> {
        let watched = Arc::new(Self {
            child: Mutex::new(child),
            exited: AtomicBool::new(false),
            done: AtomicBool::new(false),
            watchdog_fired: AtomicBool::new(false),
        });
        let watchdog = Arc::clone(&watched);
        std::thread::spawn(move || {
            std::thread::sleep(WATCHDOG_BUDGET);
            if watchdog.done.load(Ordering::SeqCst) {
                return;
            }
            watchdog.watchdog_fired.store(true, Ordering::SeqCst);
            watchdog.kill();
        });
        watched
    }

    /// Nonblocking exit poll that records the exit under the same lock the
    /// watchdog kills under, so the two can never race on the pid.
    fn poll_exit(&self) -> Option<ExitStatus> {
        let Ok(mut guard) = self.child.lock() else {
            return None;
        };
        match guard.try_wait() {
            Ok(Some(status)) => {
                self.exited.store(true, Ordering::SeqCst);
                Some(status)
            }
            Ok(None) => None,
            Err(_) => {
                self.exited.store(true, Ordering::SeqCst);
                None
            }
        }
    }

    fn pid(&self) -> Option<u32> {
        self.child.lock().ok().map(|child| child.id())
    }

    fn watchdog_killed(&self) -> bool {
        self.watchdog_fired.load(Ordering::SeqCst)
    }

    /// Best-effort SIGKILL through the live handle. Safe by construction: the
    /// handle owns the child while `exited` is unset, so its pid is still
    /// reserved and cannot have been recycled.
    fn kill(&self) {
        let Ok(mut guard) = self.child.lock() else {
            return;
        };
        if self.exited.load(Ordering::SeqCst) {
            return;
        }
        let _ = guard.kill();
    }

    /// Bounded reap for failure paths: SIGKILL, then poll. Never blocks
    /// forever; reports the status when the run exits in time.
    fn reap_bounded(&self, limit: Duration) -> Option<ExitStatus> {
        self.kill();
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.poll_exit() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn take_stderr(&self) -> Option<std::process::ChildStderr> {
        let Ok(mut guard) = self.child.lock() else {
            return None;
        };
        guard.stderr.take()
    }

    /// Mark the test finished so a late watchdog stands down.
    fn finish(&self) {
        self.done.store(true, Ordering::SeqCst);
    }

    fn exit_note(&self, status: ExitStatus) -> String {
        if self.watchdog_killed() {
            format!("{status} (fail-safe watchdog fired after {WATCHDOG_BUDGET:?})")
        } else {
            format!("{status}")
        }
    }
}

/// Wait until `path` exists. Markers are written immediately before the sleep
/// they gate, so no guessed delay decides when the run is interruptible; the
/// short settle only lets the `exec` after the marker take over. Unlike a bare
/// file poll, this also watches the run: when the child exits first, the wait
/// fails at once with its status and output instead of burning the budget
/// after a dead run.
fn wait_for_marker(
    watched: &WatchedChild,
    path: &Path,
    what: &str,
    diagnose: &dyn Fn() -> String,
) {
    let deadline = Instant::now() + MARKER_BUDGET;
    while !path.exists() {
        if let Some(status) = watched.poll_exit() {
            panic!(
                "the run exited ({}) before reaching {what}; output:\n{}",
                watched.exit_note(status),
                diagnose()
            );
        }
        if Instant::now() >= deadline {
            let reaped = watched.reap_bounded(REAP_BUDGET);
            panic!(
                "the run never reached {what} (run exit after SIGKILL: {reaped:?}); output:\n{}",
                diagnose()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
}

fn interrupt(watched: &WatchedChild) {
    let pid = watched.pid().expect("the acceptance run is already gone");
    // SAFETY: the pid belongs to a child this test spawned and has not yet reaped.
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0);
}

fn wait_for_exit(watched: &WatchedChild, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = watched.poll_exit() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Fail-safe end for a run that does not exit on time: SIGKILL it, reap it
/// with a bound, then panic with its output. This never blocks forever, so a
/// wedged run fails the test instead of hanging the harness.
fn abort_stuck(watched: &WatchedChild, diagnose: &dyn Fn() -> String, why: &str) -> ! {
    let reaped = watched.reap_bounded(REAP_BUDGET);
    watched.finish();
    panic!(
        "{why} (run exit after SIGKILL: {reaped:?}); output:\n{}",
        diagnose()
    );
}

/// Bounded drain of a finished run's piped stderr. Unlike `read_to_string`,
/// this never blocks to EOF: a surviving grandchild can hold the pipe open
/// after the run exits, so reads stop at the first moment with no data or at
/// the drain budget, whichever comes first.
fn drain_stderr(watched: &WatchedChild) -> String {
    let Some(mut stderr) = watched.take_stderr() else {
        return String::new();
    };
    // SAFETY: the descriptor is the live read end of the run's stderr pipe.
    unsafe {
        let flags = libc::fcntl(stderr.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(stderr.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = Instant::now() + OUTPUT_DRAIN_BUDGET;
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.len() >= 1_000_000 {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
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
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(directory.join("cleanup.program"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let watched = WatchedChild::spawn(child);
    let diagnose = || drain_stderr(&watched);
    wait_for_marker(&watched, started, "its main sleep", &diagnose);
    interrupt(&watched);
    wait_for_marker(&watched, entered, "its cleanup sleep", &diagnose);
    let second = Instant::now();
    interrupt(&watched);
    let Some(status) = wait_for_exit(&watched, EXIT_BUDGET) else {
        abort_stuck(
            &watched,
            &diagnose,
            "the second interrupt did not abort the cleanup sleep",
        )
    };
    let elapsed = second.elapsed();
    let stderr = drain_stderr(&watched);
    watched.finish();
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
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(directory.path().join("cleanup.program"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let watched = WatchedChild::spawn(child);
    let diagnose = || drain_stderr(&watched);
    wait_for_marker(&watched, &started, "its main sleep", &diagnose);
    interrupt(&watched);
    let Some(status) = wait_for_exit(&watched, EXIT_BUDGET) else {
        abort_stuck(&watched, &diagnose, "a single interrupt never finished cleanup")
    };
    let stderr = drain_stderr(&watched);
    watched.finish();
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
            std::ptr::null_mut(),
            std::ptr::null_mut(),
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

/// Owns the parent side of the PTY so every test path — including panics —
/// closes it exactly once instead of leaking the descriptor.
struct MasterFd(Option<RawFd>);

impl MasterFd {
    fn new(master: RawFd) -> Self {
        Self(Some(master))
    }

    fn get(&self) -> RawFd {
        self.0.unwrap_or(-1)
    }

    fn close(&mut self) {
        if let Some(master) = self.0.take() {
            // SAFETY: the parent-side descriptor is owned by this test.
            unsafe {
                libc::close(master);
            }
        }
    }
}

impl Drop for MasterFd {
    fn drop(&mut self) {
        self.close();
    }
}

/// Drain whatever the run printed to the slave without blocking: the master
/// is nonblocking, and after the child exits the slave is gone, so reads end
/// with `EAGAIN`/`EIO` instead of hanging. The total is capped so a wedged
/// writer cannot grow the drain without bound.
fn pty_output(master: RawFd) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        // SAFETY: chunk is a live buffer of its length; master is nonblocking.
        let read = unsafe { libc::read(master, chunk.as_mut_ptr().cast(), chunk.len()) };
        if read > 0 {
            bytes.extend_from_slice(&chunk[..read as usize]);
            if bytes.len() >= 1_000_000 {
                break;
            }
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
    let (child, master) = spawn_pty(
        directory.path(),
        &on_exit_program(&started, &entered, &finished),
    );
    let watched = WatchedChild::spawn(child);
    let mut master = MasterFd::new(master);
    let diagnose = || pty_output(master.get());
    wait_for_marker(&watched, &started, "its main sleep", &diagnose);
    pty_interrupt(master.get());
    wait_for_marker(&watched, &entered, "its cleanup sleep", &diagnose);
    let second = Instant::now();
    pty_interrupt(master.get());
    let Some(status) = wait_for_exit(&watched, EXIT_BUDGET) else {
        abort_stuck(
            &watched,
            &diagnose,
            "the second Ctrl-C did not abort the cleanup sleep",
        )
    };
    let elapsed = second.elapsed();
    // Drain after the exit so output collection cannot stretch the measured
    // second-interrupt-to-exit interval above.
    let output = pty_output(master.get());
    master.close();
    watched.finish();
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
