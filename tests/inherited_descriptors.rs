#![cfg(target_os = "linux")]

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

fn duplicate_high(fd: i32) -> OwnedFd {
    // SAFETY: F_DUPFD_CLOEXEC reads only the source descriptor and returns a new descriptor.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 20) };
    assert!(
        copy >= 0,
        "duplicating test descriptor: {}",
        io::Error::last_os_error()
    );
    // SAFETY: `copy` is a newly allocated descriptor owned by this test.
    unsafe { OwnedFd::from_raw_fd(copy) }
}

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

fn manifests() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

#[test]
fn execute_does_not_pass_an_inherited_writer_to_a_child() {
    let directory = tempfile::tempdir().unwrap();
    let workflow = directory.path().join("inherited-fd.program");
    fs::write(
        &workflow,
        "run \"sh\" with arguments [\"-c\", \"test ! -e /proc/self/fd/7\"]\n",
    )
    .unwrap();

    let (reader, writer) = pipe();
    let inherited_writer = duplicate_high(writer.as_raw_fd());
    let inherited_fd = inherited_writer.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory.path())
        .args(["run"])
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(manifests());
    // SAFETY: `inherited_fd` remains live until spawn returns. dup2 onto fd 7 clears
    // FD_CLOEXEC on that target, simulating a descriptor passed by the invoking service.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(inherited_fd, 7) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command
        .spawn()
        .expect("starting execute with fd 7 inherited");
    drop(writer);
    drop(inherited_writer);
    let output = child.wait_with_output().expect("waiting for execute");
    assert!(
        output.status.success(),
        "workflow child saw fd 7 or execution failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut byte = 0u8;
    // SAFETY: `reader` is the live read end of the pipe. EOF proves no descendant kept fd 7.
    let bytes_read = unsafe { libc::read(reader.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
    assert_eq!(bytes_read, 0, "a child kept the inherited pipe writer open");
}
