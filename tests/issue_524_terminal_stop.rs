//! A background native command that reads the controlling terminal is stopped by
//! SIGTTIN. The run must fail instead of hanging forever (issue #524). The test needs
//! a pty, so it runs on Unix only and is not exercised on Windows machines.
#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn open_terminal() -> (OwnedFd, OwnedFd) {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: openpty writes two descriptors into the locals above.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(result, 0, "openpty failed");
    // SAFETY: openpty returned two fresh descriptors that nothing else owns.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

#[test]
fn background_command_that_reads_the_terminal_fails_instead_of_hanging() {
    let directory = tempdir().unwrap();
    let program = directory.path().join("terminal_read.program");
    fs::write(
        &program,
        "run \"sh\" with arguments [\"-c\", \"read x </dev/tty; echo got $x\"]\n",
    )
    .unwrap();

    let (master, slave) = open_terminal();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .arg("run")
        .arg(&program)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    let slave_fd = slave.as_raw_fd();
    // SAFETY: the hook runs after fork and calls only async-signal-safe functions. It
    // gives the interpreter its own session and makes the pty its controlling terminal.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 || libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let started = Instant::now();
    let mut child = command.spawn().unwrap();
    drop(slave);

    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&output);
    let mut reader = fs::File::from(master.try_clone().unwrap());
    let reader_thread = std::thread::spawn(move || {
        let mut buffer = [0u8; 256];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buffer[..count]);
        }
    });

    // Without the fix the interpreter never exits here, so the bound is the hang detector.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the run hung on a terminal read instead of failing"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the failure took too long"
    );
    assert!(!status.success(), "a terminal read must fail the run");
    // The pty reader ends once every slave descriptor has closed, so all output is in.
    reader_thread.join().unwrap();
    let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
    assert!(
        text.contains("SIGTTIN"),
        "the failure must name the terminal stop, got: {text}"
    );
}
