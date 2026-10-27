#![cfg(target_os = "linux")]

// Regression test for issue #526: program output and trace writes must never
// set O_NONBLOCK on the shared stdout/stderr description while a native child
// inherits it. A native child writes 1 MB of framed records to the inherited
// stderr (and 1 MB to its captured stdout, which execute forwards) while
// `execute run --trace` emits trace lines and forwarded program output
// through the same pipe to a slow reader.
//
// Overlap is forced by backpressure, not by timing: 2 MB of probe output
// dwarfs the pipe buffer, so the writers must block while execute still has
// output to emit. The 2 ms drain pacing only keeps the reader slower than a
// writer burst; the latched `peak_while_alive` assertion below fails loudly
// if the expected contention window never materializes, instead of passing
// vacuously. Cleanup is bounded on every path: the execute watchdog kills and
// reaps the direct child, and the drain is collected with a timeout, so even
// a surviving orphaned descendant holding the pipe open fails the test
// instead of hanging the suite.
//
// Residual limits: the red signal stays probabilistic (a scheduler could in
// theory serialize the streams without overlap, and the overlap assertion
// turns that into a loud failure rather than a false pass), and an orphaned
// descendant on the watchdog path leaks until the test process exits. The
// test is Linux-only while the `stdio.rs` fix is Unix-wide.
use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CHILD_BYTES: usize = 1024 * 1024;
const FRAME_LEN: usize = 8192;
const EXPECTED_FRAMES: usize = CHILD_BYTES / FRAME_LEN;
const OUT_MAGIC: &[u8] = b"~526OUT~";
const ERR_MAGIC: &[u8] = b"~526ERR~";
// Bytes that must cross while execute is still alive to prove the streams
// actually contended for the full pipe.
const OVERLAP_BYTES: usize = 256 * 1024;
const EXECUTE_TIMEOUT: Duration = Duration::from_secs(30);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

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

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

#[test]
fn native_child_writes_megabyte_to_inherited_stderr_while_execute_streams() {
    assert!(
        CHILD_BYTES.is_multiple_of(FRAME_LEN),
        "framed records must divide the probe volume exactly"
    );
    let directory = tempfile::tempdir().unwrap();
    let fixture = env!("CARGO_BIN_EXE_native-fixture").to_owned();
    let status_path = directory.path().join("stream-probe.status");
    let workflow = directory.path().join("inherited-stderr.program");
    fs::write(
        &workflow,
        format!(
            "run \"sh\" with arguments [\"-c\", \"exec \\\"$0\\\" stream-probe {CHILD_BYTES} {probe_status:?}\", {fixture:?}]\nrun printf with message \"output-line\"\n",
            probe_status = status_path.to_string_lossy(),
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

    let progress = std::sync::Arc::new(AtomicUsize::new(0));
    let progress_writer = std::sync::Arc::clone(&progress);
    let (drain_sender, drain_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::fs::File::from(reader);
        let mut all_bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    let _ = drain_sender.send(all_bytes);
                    return;
                }
                Ok(count) => {
                    all_bytes.extend_from_slice(&chunk[..count]);
                    progress_writer.store(all_bytes.len(), Ordering::SeqCst);
                    // Pacing only: keep the reader slower than a writer burst
                    // so the pipe stays full. Overlap itself is enforced by
                    // total volume versus pipe capacity and verified by the
                    // `peak_while_alive` assertion below.
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("draining shared pipe failed: {error}"),
            }
        }
    });

    // A bounded watchdog keeps a wedged run from hanging the suite. The peak
    // counter latches how much crossed while execute was still alive, proving
    // the contention window existed instead of assuming it.
    let deadline = Instant::now() + EXECUTE_TIMEOUT;
    let mut peak_while_alive = 0usize;
    let status = loop {
        peak_while_alive = peak_while_alive.max(progress.load(Ordering::SeqCst));
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            None => {
                let _ = child.kill();
                let grace = Instant::now() + Duration::from_secs(5);
                while child.try_wait().unwrap().is_none() && Instant::now() < grace {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = child.try_wait();
                panic!("execute did not finish before the watchdog");
            }
        }
    };
    // Bounded on every path: if a descendant outlives execute and holds the
    // pipe open, this fails instead of joining a drain that never sees EOF.
    // The stranded drain thread dies with the test process.
    let drained: Vec<u8> = drain_receiver
        .recv_timeout(DRAIN_TIMEOUT)
        .unwrap_or_else(|_| {
            panic!(
                "shared pipe never drained after execute exited; \
             a surviving descendant may still hold the write end"
            )
        });
    let status_text = fs::read_to_string(&status_path).unwrap_or_default();
    let out_frames = count_occurrences(&drained, OUT_MAGIC);
    let err_frames = count_occurrences(&drained, ERR_MAGIC);
    let tail = String::from_utf8_lossy(&drained[drained.len().saturating_sub(512)..]).into_owned();
    assert!(
        status.success(),
        "execute failed with shared slow pipe; probe status: {status_text:?} \
         out_frames: {out_frames}/{EXPECTED_FRAMES} err_frames: {err_frames}/{EXPECTED_FRAMES} \
         trailing bytes: {tail}"
    );
    assert_eq!(
        status_text, "probe-526-ok\n",
        "native probe did not finish cleanly (42 is O_NONBLOCK, 43 is a \
         short/EAGAIN write); out_frames: {out_frames}/{EXPECTED_FRAMES} \
         err_frames: {err_frames}/{EXPECTED_FRAMES} tail: {tail}"
    );
    assert_eq!(
        out_frames, EXPECTED_FRAMES,
        "forwarded child stdout lost framed records"
    );
    assert_eq!(
        err_frames, EXPECTED_FRAMES,
        "inherited child stderr lost framed records"
    );
    assert!(
        drained
            .windows(b"output-line".len())
            .any(|window| window == b"output-line"),
        "program output did not cross the shared pipe"
    );
    assert!(
        drained.windows(7).any(|window| window == b"\"event\""),
        "trace lines did not cross the shared pipe"
    );
    assert!(
        peak_while_alive >= OVERLAP_BYTES,
        "slow reader never contended with a live execute (peak {peak_while_alive} \
         bytes while alive); overlap was not forced, so green proves nothing"
    );
}

/// Negative control for the probe oracle above: proves the `F_GETFL` flag
/// check and raw-`write` EAGAIN detection fire on this kernel without execute,
/// sleeps, or pipes under test. If this control fails, a green main test
/// would be meaningless; if it passes, a green main test cannot be explained
/// by a broken detector.
#[test]
fn fcntl_and_eagain_oracle_fires_on_nonblocking_pipe() {
    let (reader, writer) = pipe();
    // SAFETY: `writer` is the live write end of a test pipe; F_GETFL only
    // reads its status flags.
    let before = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
    assert!(before >= 0, "F_GETFL failed on a test pipe");
    assert_eq!(
        before & libc::O_NONBLOCK,
        0,
        "test pipe unexpectedly starts nonblocking"
    );
    let set_flags =
        // SAFETY: writer is live, F_SETFL changes its flags, and the read end stays open.
        unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, before | libc::O_NONBLOCK) };
    assert_eq!(set_flags, 0, "F_SETFL failed on a test pipe");
    // SAFETY: `writer` is still open; F_GETFL only reads its status flags.
    let flagged = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
    assert!(
        flagged >= 0 && flagged & libc::O_NONBLOCK != 0,
        "F_GETFL did not report O_NONBLOCK after F_SETFL"
    );
    let chunk = [0x41u8; 8192];
    let mut saw_eagain = false;
    for _ in 0..16_384 {
        // SAFETY: `chunk` is live for its length and `writer` is open; the
        // call changes no status flags.
        let count = unsafe { libc::write(writer.as_raw_fd(), chunk.as_ptr().cast(), chunk.len()) };
        if count < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error();
            assert_eq!(
                errno,
                Some(libc::EAGAIN),
                "nonblocking pipe write failed with errno {errno:?}, not EAGAIN"
            );
            saw_eagain = true;
            break;
        }
        assert!(count > 0, "pipe write made no progress");
    }
    assert!(
        saw_eagain,
        "nonblocking pipe never returned EAGAIN; the 43 detector has nothing to catch"
    );
    drop(writer);
    drop(reader);
}
