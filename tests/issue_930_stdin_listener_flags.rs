#![cfg(unix)]

// Regression test for issue #930: `listen to stdin` must not set O_NONBLOCK on the inherited
// stdin description. That description is shared with the parent shell and with any other process
// holding the same pipe, and the only restore runs in a Drop that SIGKILL skips. The listener
// here is killed with SIGKILL while the test keeps a duplicate of the same pipe end, so the
// duplicate shows whatever flags the killed listener left on the shared description.
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::Duration;

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut descriptors = [-1; 2];
    // SAFETY: `descriptors` points to two writable integers for pipe.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: pipe initialized both entries with fresh descriptors.
    unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    }
}

fn status_flags(descriptor: &OwnedFd) -> libc::c_int {
    // SAFETY: F_GETFL only reads the status flags of a live descriptor.
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL failed on the surviving pipe end");
    flags
}

#[test]
fn killed_stdin_listener_leaves_shared_stdin_blocking() {
    let directory = tempfile::tempdir().unwrap();
    let workflow = directory.path().join("stdin-listener.program");
    std::fs::write(
        &workflow,
        "listen to stdin\nwhenever a line arrives as event\n  then encode event as json\n",
    )
    .unwrap();

    let (reader, writer) = pipe();
    // A raw descriptor has no `Write`; a `File` over the write end does.
    let mut writer = std::fs::File::from(writer);
    // SAFETY: `reader` is the live read end of the test pipe; F_DUPFD_CLOEXEC returns a second
    // descriptor for the same open file description, which stands in for the surviving end.
    let surviving = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    assert!(surviving >= 0, "duplicating the test pipe reader failed");
    // SAFETY: `surviving` is the fresh descriptor returned above.
    let surviving = unsafe { OwnedFd::from_raw_fd(surviving) };
    assert_eq!(
        status_flags(&surviving) & libc::O_NONBLOCK,
        0,
        "test pipe unexpectedly starts nonblocking"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(directory.path())
        .stdin(Stdio::from(std::fs::File::from(reader)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("starting a stdin listener");

    // Feed one record so the listener is past startup and reading. The writer stays open, so the
    // listener keeps waiting for more input until it is killed.
    writer.write_all(b"hello\n").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        child.try_wait().unwrap().is_none(),
        "stdin listener exited before it was killed"
    );

    child.kill().expect("sending SIGKILL to the stdin listener");
    child.wait().unwrap();

    assert_eq!(
        status_flags(&surviving) & libc::O_NONBLOCK,
        0,
        "a killed `listen to stdin` service left O_NONBLOCK set on the shared stdin description"
    );
    drop(writer);
}
