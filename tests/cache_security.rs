//! Real CLI cache boundaries with private files and bounded owned processes.
//! The cache validation logic is portable; only the fixture-creation tests
//! that plant symlinks, FIFOs, or Unix permission bits stay Unix-only. The
//! pipe-draining harness uses reader threads instead of `fcntl` so it runs on
//! both platforms.
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const MAX_CAPTURE_BYTES: usize = 64 * 1024;

struct Fixture {
    directory: tempfile::TempDir,
    entry: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("input"), b"owned input\n").unwrap();
        fs::write(root.join("job.program"), "read \"input\"\nthen tr with from \"[:lower:]\" and to \"[:upper:]\" and cache\nthen save to \"later-output\"\n").unwrap();
        let mut fixture = Self {
            directory,
            entry: PathBuf::new(),
        };
        let output = fixture.run(None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"OWNED INPUT\n");
        fixture.entry = fs::read_dir(fixture.root().join(".flow-cache"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .find(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "bin")
            })
            .expect("cache entry")
            .path();
        fs::remove_file(fixture.root().join("later-output")).unwrap();
        fixture
    }
    fn root(&self) -> &Path {
        self.directory.path()
    }
    fn run(&self, cache: Option<&Path>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .current_dir(self.root())
            .arg("job.program")
            .arg("--manifest-dir")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
            .arg("--trace")
            .arg("--command-timeout-ms")
            .arg("1000")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = cache {
            command.arg("--cache-dir").arg(path);
        }
        let mut child = command.spawn().unwrap();
        // Drain both pipes while the child runs. Trace output can exceed the
        // platform pipe capacity; polling for exit before reading either pipe
        // would then block the CLI in its next trace write. Reader threads
        // keep the watchdog bounded; these fixtures spawn no pipe-inheriting
        // descendants, so joining after the child exits always terminates.
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out_handle = std::thread::spawn(move || read_all(stdout));
        let err_handle = std::thread::spawn(move || read_all(stderr));
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let (output, output_truncated) = out_handle.join().unwrap();
                let (errors, errors_truncated) = err_handle.join().unwrap();
                assert!(
                    !output_truncated && !errors_truncated,
                    "owned cache fixture exceeded its bounded output capture"
                );
                return Output {
                    status,
                    stdout: output,
                    stderr: errors,
                };
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let status = child.wait().unwrap();
                let (output, _) = out_handle.join().unwrap();
                let (errors, _) = err_handle.join().unwrap();
                let output = Output {
                    status,
                    stdout: output,
                    stderr: errors,
                };
                panic!(
                    "owned cache fixture blocked: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn rejects(&self, cache: Option<&Path>, message: &str) {
        let output = self.run(cache);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(message), "{error}");
        assert!(
            !error.contains("\"event\":\"spawn\""),
            "invalid cache launched a native stage: {error}"
        );
        assert!(!self.root().join("later-output").exists());
    }
}

/// Read a piped stream to EOF with the same bounded capture the `fcntl`
/// harness enforced: at most `MAX_CAPTURE_BYTES` retained, with a flag when
/// longer output is truncated away.
fn read_all(mut reader: impl Read) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return (bytes, truncated),
            Ok(count) => {
                let retained = count.min(MAX_CAPTURE_BYTES.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&chunk[..retained]);
                truncated |= retained != count;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("owned cache fixture pipe failed: {error}"),
        }
    }
}

#[test]
fn ordinary_existing_cache_is_reused_without_launching_or_changing_key_bytes() {
    let fixture = Fixture::new();
    let before = fs::read(&fixture.entry).unwrap();
    let output = fixture.run(None);
    assert!(output.status.success());
    // The entry file stores the authenticated envelope (magic + status +
    // payload; see `cache_output` in src/runtime.rs), while stdout replays
    // only the payload the first run produced.
    let payload = b"OWNED INPUT\n";
    assert_eq!(output.stdout, payload);
    assert!(
        before.len() > payload.len() && before.ends_with(payload),
        "entry file should wrap the payload in an envelope"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("cache_hit"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("\"event\":\"spawn\""));
    assert_eq!(fs::read(&fixture.entry).unwrap(), before);
    assert_eq!(
        fs::read_dir(fixture.entry.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "bin"))
            .count(),
        1
    );
}

#[test]
fn rewritten_cache_bytes_fail_closed_without_native_launch_or_later_save() {
    let fixture = Fixture::new();
    fs::write(&fixture.entry, b"rewritten cached output").unwrap();
    fixture.rejects(None, "integrity verification failed");
}

#[test]
fn missing_cache_tag_fails_closed_without_native_launch_or_later_save() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.entry.with_extension("hmac")).unwrap();
    fixture.rejects(None, "no integrity tag");
}

// Unix-only: plants a symlink cache entry, which Windows cannot create without
// extra privilege and which the Windows trust model judges by ACL, not links.
#[cfg(unix)]
#[test]
fn cache_symlink_never_exposes_target_or_runs_later_save() {
    let fixture = Fixture::new();
    let canary = fixture.root().join("private-canary");
    fs::write(&canary, b"owned private canary").unwrap();
    fs::remove_file(&fixture.entry).unwrap();
    symlink(&canary, &fixture.entry).unwrap();
    fixture.rejects(None, "regular file");
    assert_eq!(fs::read(canary).unwrap(), b"owned private canary");
}

// Unix-only: plants a FIFO cache entry via mkfifo; Windows has no FIFO files.
#[cfg(unix)]
#[test]
fn fifo_cache_entry_is_rejected_without_waiting_for_a_writer() {
    let fixture = Fixture::new();
    fs::remove_file(&fixture.entry).unwrap();
    let path = std::ffi::CString::new(fixture.entry.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `path` is a CString built just above; CString::new rejected interior NULs, so
    // as_ptr is NUL-terminated, and the CString outlives the call. mkfifo only reads the path.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    fixture.rejects(None, "regular file");
}

// Unix-only: swaps the cache directory for a symlink, exercising Unix link
// traversal; the Windows trust model judges directories by ACL instead.
#[cfg(unix)]
#[test]
fn linked_cache_directory_is_rejected_including_trailing_separators() {
    let fixture = Fixture::new();
    let cache = fixture.root().join(".flow-cache");
    let target = fixture.root().join("target");
    fs::rename(&cache, &target).unwrap();
    symlink(&target, &cache).unwrap();
    let before = fs::metadata(&target).unwrap().permissions().mode();
    for name in [".flow-cache", ".flow-cache/", ".flow-cache/."] {
        fixture.rejects(Some(Path::new(name)), "real directory");
    }
    assert_eq!(fs::metadata(target).unwrap().permissions().mode(), before);
}

// Unix-only: asserts the Unix mode-bit ownership contract (0o777/0o666);
// Windows judges cache privacy by ACL, not mode bits.
#[cfg(unix)]
#[test]
fn caches_writable_by_other_users_fail_closed_without_permission_changes() {
    let fixture = Fixture::new();
    let cache = fixture.root().join(".flow-cache");
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o777)).unwrap();
    fixture.rejects(
        None,
        "cache directory must be owned by the current user and private",
    );
    assert_eq!(
        fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o777
    );
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&fixture.entry, fs::Permissions::from_mode(0o666)).unwrap();
    fixture.rejects(
        None,
        "cached result must be owned by the current user and private",
    );
    assert_eq!(
        fs::metadata(&fixture.entry).unwrap().permissions().mode() & 0o777,
        0o666
    );
}
