#![cfg(target_os = "linux")]

// Regression test for issue #526: program output and trace writes must never
// set O_NONBLOCK on the shared stdout/stderr description while a native child
// inherits it. A native child writes 1 MB to the inherited stderr while
// `execute run --trace` emits trace lines and forwarded program output
// through the same pipe to a slow reader. The child fails the run (fixture
// exits 42 on O_NONBLOCK, 43 on any write error including EAGAIN) when the
// shared flags are corrupted.
use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_BYTES: usize = 1024 * 1024;

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
fn native_child_writes_megabyte_to_inherited_stderr_while_execute_streams() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = env!("CARGO_BIN_EXE_native-fixture").to_owned();
    let workflow = directory.path().join("inherited-stderr.program");
    fs::write(
        &workflow,
        format!(
            "run \"sh\" with arguments [\"-c\", \"exec \\\"$0\\\" stream-probe {CHILD_BYTES}\", {fixture:?}]\nrun printf with message \"output-line\"\n"
        ),
    )
    .unwrap();

    let (reader, writer) = pipe();
    // SAFETY: `writer` is the live write end of the test pipe; F_DUPFD_CLOEXEC
    // returns a second descriptor for the same open file description so both
    // of execute's streams share one pipe.
    let writer_copy = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    assert!(writer_copy >= 0, "duplicating test pipe writer failed");
    // SAFETY: `writer_copy` is the fresh descriptor returned above.
    let writer_copy = unsafe { OwnedFd::from_raw_fd(writer_copy) };
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--trace")
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::from(writer)))
        .stderr(Stdio::from(std::fs::File::from(writer_copy)))
        .spawn()
        .expect("starting execute with a shared slow pipe");

    let drain = std::thread::spawn(move || {
        let mut reader = std::fs::File::from(reader);
        let mut all_bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return all_bytes,
                Ok(count) => {
                    all_bytes.extend_from_slice(&chunk[..count]);
                    // Drain slowly so the pipe stays full while execute and
                    // the native child write through it concurrently.
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("draining shared pipe failed: {error}"),
            }
        }
    });

    // A bounded watchdog keeps a wedged run from hanging the suite.
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("execute did not finish before the watchdog");
            }
        }
    };
    let drained = drain.join().unwrap();
    let start = drained.len().saturating_sub(4096);
    let tail = String::from_utf8_lossy(&drained[start..]).into_owned();
    assert!(
        status.success(),
        "execute failed with shared slow pipe; trailing bytes: {tail}"
    );
    assert!(
        drained.windows(b"output-line".len()).any(|window| window == b"output-line"),
        "program output did not cross the shared pipe"
    );
    assert!(
        drained.windows(7).any(|window| window == b"\"event\""),
        "trace lines did not cross the shared pipe"
    );
    assert!(
        drained.iter().filter(|byte| **byte == b'o').count() >= CHILD_BYTES,
        "forwarded child stdout is short: {}",
        drained.len()
    );
    assert!(
        drained.iter().filter(|byte| **byte == b'e').count() >= CHILD_BYTES,
        "inherited child stderr is short: {}",
        drained.len()
    );
}
