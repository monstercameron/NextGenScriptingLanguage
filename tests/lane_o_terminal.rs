//! `interactively` terminal handoff. The pty test needs a Linux controlling
//! terminal and runs in Linux CI; the other test runs on every Unix.
#![cfg(unix)]

use std::fs;
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn write_program(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempdir().unwrap();
    let program = directory.path().join("interactive.program");
    fs::write(&program, text).unwrap();
    (directory, program)
}

#[test]
fn interactively_without_a_terminal_runs_the_command_normally() {
    let (_directory, program) =
        write_program("run \"sh\" with arguments [\"-c\", \"echo plain\"] interactively\n");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&program)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "plain\n");
}

#[cfg(target_os = "linux")]
mod pty {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

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

    fn wait_until(output: &Mutex<Vec<u8>>, needle: &[u8]) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if output
                .lock()
                .unwrap()
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn interactive_command_reads_the_terminal_and_the_parent_takes_it_back() {
        let (_directory, program) = write_program(concat!(
            "run \"sh\" with arguments [\"-c\", \"echo ready; read line; echo got:$line\"] interactively\n",
            "run \"sh\" with arguments [\"-c\", \"echo mid; sleep 3\"]\n",
        ));
        let (master, slave) = open_terminal();
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .arg("run")
            .arg(&program)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        let slave_fd = slave.as_raw_fd();
        // SAFETY: the hook runs after fork and calls only async-signal-safe
        // functions. It gives the workflow its own session and controlling terminal.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 || libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();

        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let mut reader = fs::File::from(master.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buffer = [0u8; 256];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buffer[..count]);
            }
        });

        // The interactive command has the terminal only after it is running.
        assert!(
            wait_until(&output, b"ready"),
            "interactive command never started"
        );
        let mut writer = fs::File::from(master.try_clone().unwrap());
        writer.write_all(b"hello\n").unwrap();

        // The next command starts only after the handoff has been returned, so the
        // terminal's foreground group must now be the workflow's own group again.
        assert!(
            wait_until(&output, b"mid"),
            "workflow never reached the next command"
        );
        // The test process has no controlling terminal, so tcgetpgrp cannot see this pty.
        // ps reports the foreground group of the workflow's own controlling terminal.
        let ps = std::process::Command::new("ps")
            .args(["-o", "tpgid=", "-p", &child.id().to_string()])
            .output()
            .unwrap();
        let foreground: i32 = String::from_utf8_lossy(&ps.stdout).trim().parse().unwrap();
        assert_eq!(foreground, child.id() as i32);

        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "workflow did not finish");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success());
        assert!(
            wait_until(&output, b"got:hello"),
            "interactive command did not read the terminal"
        );
    }
}
