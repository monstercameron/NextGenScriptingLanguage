//! Acceptance for issue #525: `--kill-grace-ms` wiring and bounded cleanup.
//!
//! Cross-platform strategy: CLI validation runs everywhere with portable
//! applets. Real signal-escalation behaviour (trap vs SIGKILL, ignorer bounds)
//! and the non-TTY `interactively` smoke test need a Unix shell, so they are
//! `#[cfg(unix)]` gated and compile to no-ops elsewhere.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn invoke(directory: &Path, script: &str, extra: &[&str]) -> Output {
    let path = directory.join("case.program");
    fs::write(&path, script).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory)
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests());
    command.args(extra);
    command.output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn kill_grace_rejects_bad_values_before_any_effect() {
    for extra in [
        vec!["--kill-grace-ms", "oops"],
        vec!["--kill-grace-ms", "1", "--kill-grace-ms", "2"],
    ] {
        let directory = tempfile::tempdir().unwrap();
        let output = invoke(directory.path(), "run printf with message \"x\"\n", &extra);
        assert!(!output.status.success(), "{extra:?}");
        let message = stderr(&output);
        assert!(message.contains("--kill-grace-ms"), "{extra:?}: {message}");
        assert!(
            !directory.path().join("effect.txt").exists(),
            "{extra:?} ran before validation"
        );
    }
}

#[test]
fn kill_grace_missing_value_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let output = invoke(
        directory.path(),
        "run printf with message \"x\"\n",
        &["--kill-grace-ms"],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--kill-grace-ms"));
}

#[test]
fn kill_grace_zero_is_accepted_and_runs() {
    let directory = tempfile::tempdir().unwrap();
    let output = invoke(
        directory.path(),
        "run printf with message \"x\"\n",
        &["--kill-grace-ms", "0"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout, b"x\n");
}

// The live host owns `--control` refusal: with the `live` feature the flag is
// refused by name (see also agent_run_control.rs, which is live-gated for the
// same reason). Without `live`, the binary fails earlier on the missing
// endpoint (baseline behavior, unchanged by #525). Both builds must refuse
// the combination without running the program.
#[cfg(feature = "live")]
#[test]
fn kill_grace_control_mode_refuses_the_flag_by_name() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("case.program");
    fs::write(&path, "run printf with message \"x\"\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--control")
        .arg("--kill-grace-ms")
        .arg("100")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--kill-grace-ms"));
}

#[cfg(not(feature = "live"))]
#[test]
fn kill_grace_control_mode_without_live_still_refuses_control() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("case.program");
    fs::write(&path, "run printf with message \"x\"\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--control")
        .arg("--kill-grace-ms")
        .arg("100")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("control endpoint"),
        "without live, --control must fail on the missing endpoint: {}",
        stderr(&output)
    );
    assert!(
        output.stdout.is_empty(),
        "a refused run must not print program output"
    );
}

#[cfg(unix)]
fn assert_owned_children_reaped(output: &Output) {
    for event in stderr(output)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event["event"] != "spawn" {
            continue;
        }
        let pid: i32 = event["pid"].as_u64().unwrap().try_into().unwrap();
        // Signal 0 delivers nothing; the pid comes from a completed spawn trace
        // record for a child this run started.
        assert_eq!(
            // SAFETY: signal 0 delivers nothing; pid is from a spawn trace.
            unsafe { libc::kill(pid, 0) },
            -1,
            "owned child {pid} leaked"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[cfg(unix)]
#[test]
fn trapped_cleanup_finishes_inside_the_default_grace() {
    let directory = tempfile::tempdir().unwrap();
    // Trap TERM, record the cleanup, then exit promptly so the default 2s
    // grace preserves the effect instead of SIGKILLing through it. The 1s
    // sleep inside the trap keeps the grace-0 test deterministic: an
    // immediate SIGKILL lands before the trap can write.
    let script = format!(
        "run \"sh\" with arguments [\"-c\", {}, \"trap-cleanup\", {}]\n",
        serde_json::to_string("trap 'sleep 1; touch cleaned.txt; exit 0' TERM; sleep 30").unwrap(),
        serde_json::to_string(&directory.path().display().to_string()).unwrap(),
    );
    let started = std::time::Instant::now();
    let output = invoke(
        directory.path(),
        &script,
        &["--command-timeout-ms", "1000", "--trace"],
    );
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("timed out"), "{}", stderr(&output));
    assert!(
        directory.path().join("cleaned.txt").exists(),
        "trapped TERM cleanup did not run inside the grace: {}",
        stderr(&output)
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "cleanup exceeded the bounded grace: {:?}",
        started.elapsed()
    );
    assert_owned_children_reaped(&output);
}

#[cfg(unix)]
#[test]
fn kill_grace_zero_skips_trapped_cleanup_but_still_reaps() {
    let directory = tempfile::tempdir().unwrap();
    let script = format!(
        "run \"sh\" with arguments [\"-c\", {}, \"trap-cleanup\", {}]\n",
        serde_json::to_string("trap 'sleep 1; touch cleaned.txt; exit 0' TERM; sleep 30").unwrap(),
        serde_json::to_string(&directory.path().display().to_string()).unwrap(),
    );
    let started = std::time::Instant::now();
    let output = invoke(
        directory.path(),
        &script,
        &[
            "--command-timeout-ms",
            "1000",
            "--kill-grace-ms",
            "0",
            "--trace",
        ],
    );
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("timed out"), "{}", stderr(&output));
    assert!(
        !directory.path().join("cleaned.txt").exists(),
        "grace 0 should SIGKILL before the TERM trap runs"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "grace 0 path was not bounded: {:?}",
        started.elapsed()
    );
    assert_owned_children_reaped(&output);
}

#[cfg(unix)]
#[test]
fn ignoring_child_is_still_reaped_within_a_small_grace() {
    let directory = tempfile::tempdir().unwrap();
    // A true ignorer: the shell traps TERM and never exits on its own, so only
    // the final SIGKILL ends it. (A bare `trap '' TERM; sleep 30` would exit as
    // soon as its sleep child dies and would not test the grace at all.)
    let script = format!(
        "run \"sh\" with arguments [\"-c\", {}, \"ignore-term\", {}]\n",
        serde_json::to_string("trap '' TERM; while true; do sleep 1; done").unwrap(),
        serde_json::to_string(&directory.path().display().to_string()).unwrap(),
    );
    let started = std::time::Instant::now();
    let output = invoke(
        directory.path(),
        &script,
        &[
            "--command-timeout-ms",
            "500",
            "--kill-grace-ms",
            "300",
            "--trace",
        ],
    );
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("timed out"), "{}", stderr(&output));
    // Grace (300ms) plus the bounded drop must end the ignorer promptly;
    // 30s of sleep must never be waited out.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "ignoring child was not bounded: {:?}",
        started.elapsed()
    );
    assert_owned_children_reaped(&output);
}

#[cfg(unix)]
#[test]
fn interactive_without_a_tty_completes_without_hanging() {
    // Piped stdin is not a terminal, so the handoff takes the no-PTY branch
    // (no foreground juggling, no termios save). The run must still complete.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("case.program");
    fs::write(
        &path,
        "run \"sh\" with arguments [\"-c\", \"exit 0\"] interactively\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

// Three true TERM-ignorers in one pipe group share a single grace wait: the
// abort cost is one grace, not three. With a serial per-child wait this takes
// at least 3 x grace after the timeout; with the common deadline it takes one.
#[cfg(unix)]
#[test]
fn parallel_ignorers_share_one_grace_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let ignorer = serde_json::to_string("trap '' TERM; while true; do sleep 1; done").unwrap();
    let script = format!(
        "run \"sh\" with arguments [\"-c\", {ignorer}]\npipe to \"sh\" with arguments [\"-c\", {ignorer}]\npipe to \"sh\" with arguments [\"-c\", {ignorer}]\n",
    );
    let started = Instant::now();
    let output = invoke(
        directory.path(),
        &script,
        &[
            "--command-timeout-ms",
            "500",
            "--kill-grace-ms",
            "2000",
            "--trace",
        ],
    );
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("timed out"), "{}", stderr(&output));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "three ignorers waited serially instead of sharing one grace: {:?}",
        started.elapsed()
    );
    assert_owned_children_reaped(&output);
}

// SIGINT to the runner must reach the child's group as SIGINT (not SIGTERM),
// so an INT trap runs. This covers the escalation branch timeouts never take.
#[cfg(unix)]
#[test]
fn sigint_reaches_the_trapped_group_as_sigint() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("case.program");
    fs::write(
        &path,
        "run \"sh\" with arguments [\"-c\", \"trap 'touch int-trapped.txt; exit 0' INT; touch started.txt; sleep 30\"]\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !directory.path().join("started.txt").exists() {
        assert!(Instant::now() < deadline, "the run never reached its sleep");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: the pid belongs to the child spawned above and is not reaped yet.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "SIGINT cancellation waited out the trapped child"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(130), "SIGINT must exit 130");
    assert!(
        directory.path().join("int-trapped.txt").exists(),
        "the INT trap never ran: the group got SIGTERM instead of SIGINT"
    );
}

// Killing an interactive child that switched the terminal to raw mode must
// still restore the saved settings on teardown (issue #525). Linux-gated like
// the pty handoff test in lane_o_terminal.rs: pty tests run in Linux CI, while
// the non-TTY `interactively` smoke test above keeps Unix coverage.
#[cfg(target_os = "linux")]
#[test]
fn killed_interactive_child_restores_terminal_settings() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    assert_eq!(
        // SAFETY: plain out-fds; defaults for name, termios and window size.
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0,
        "openpty failed: pty tests need a Linux controlling terminal (Linux CI)"
    );
    // SAFETY: openpty succeeded above, so master is a live owned fd.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: termios is plain data; all-zero is a valid bit pattern.
    let mut before: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        // SAFETY: master is a live open pty fd; tcgetattr writes only into the live termios.
        unsafe { libc::tcgetattr(master.as_raw_fd(), &mut before) },
        0
    );
    // SAFETY: ptsname's pointer is valid until the next call; copy it now.
    let slave_name = unsafe {
        let name = libc::ptsname(master.as_raw_fd());
        assert!(!name.is_null());
        std::ffi::CStr::from_ptr(name).to_owned()
    };
    let mut slave_path = slave_name.to_bytes().to_vec();
    slave_path.push(0);

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("case.program");
    fs::write(
        &path,
        "run \"sh\" with arguments [\"-c\", \"stty raw -echo; touch started.txt; sleep 30\"] interactively\n",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        // SAFETY: ownership of the slave moves into the child as its stdin.
        .stdin(unsafe { std::process::Stdio::from_raw_fd(slave) })
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: the closure runs between fork and exec; it calls only async-signal-safe functions.
    unsafe {
        command.pre_exec(move || {
            // Become a session leader and take the pty as the controlling
            // terminal, like a real terminal login. Only async-signal-safe
            // calls and no allocation inside the forked child.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = libc::open(slave_path.as_ptr() as *const libc::c_char, libc::O_RDWR);
            if fd == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(fd);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !directory.path().join("started.txt").exists() {
        assert!(Instant::now() < deadline, "the run never reached its sleep");
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: termios is plain data; all-zero is a valid bit pattern.
    let mut current: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        // SAFETY: master is a live open pty fd; tcgetattr writes only into the live termios.
        unsafe { libc::tcgetattr(master.as_raw_fd(), &mut current) },
        0
    );
    assert_eq!(
        current.c_lflag & libc::ICANON,
        0,
        "the child never put the terminal in raw mode"
    );
    // SAFETY: the pid belongs to the child spawned above and is not reaped yet.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "teardown hung instead of restoring the terminal"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(143));
    // SAFETY: termios is plain data; all-zero is a valid bit pattern.
    let mut after: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        // SAFETY: master is a live open pty fd; tcgetattr writes only into the live termios.
        unsafe { libc::tcgetattr(master.as_raw_fd(), &mut after) },
        0
    );
    assert_eq!(
        termios_bytes(&after),
        termios_bytes(&before),
        "interactive teardown left raw mode behind"
    );
}

#[cfg(target_os = "linux")]
fn termios_bytes(settings: &libc::termios) -> Vec<u8> {
    // SAFETY: termios is plain data; the slice only reads it.
    unsafe {
        std::slice::from_raw_parts(
            settings as *const libc::termios as *const u8,
            std::mem::size_of::<libc::termios>(),
        )
        .to_vec()
    }
}

// Portable coverage for the Windows abort path (also runs on Unix): three
// native sleeps in one pipe group must stay bounded by one shared grace plus
// teardown, whether the polite break lands or the job object ends stragglers.
#[test]
fn portable_sleep_pipeline_stays_bounded_with_grace() {
    let directory = tempfile::tempdir().unwrap();
    // Forward slashes survive the program parser on every platform; a raw
    // Windows path would arrive with backslashes (see lane_o_parallel_resume).
    let fixture =
        serde_json::to_string(&env!("CARGO_BIN_EXE_native-fixture").replace('\\', "/")).unwrap();
    let source = format!(
        "run {fixture} with arguments [\"sleep\", \"30000\"]\npipe to {fixture} with arguments [\"sleep\", \"30000\"]\npipe to {fixture} with arguments [\"sleep\", \"30000\"]\n",
    );
    let started = Instant::now();
    let output = invoke(
        directory.path(),
        &source,
        &[
            "--command-timeout-ms",
            "500",
            "--kill-grace-ms",
            "500",
            "--trace",
        ],
    );
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("timed out"), "{}", stderr(&output));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "sleep pipeline was not bounded: {:?}",
        started.elapsed()
    );
    #[cfg(unix)]
    assert_owned_children_reaped(&output);
}
