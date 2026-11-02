#![cfg(unix)]
//! Real CLI cache boundaries with private files and bounded owned processes.
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::{symlink, PermissionsExt},
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;

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
        // would then block the CLI in its next trace write. Nonblocking reads
        // keep the watchdog bounded even if an unexpected descendant inherits
        // either pipe after the owned CLI child is killed.
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        set_nonblocking(&stdout).unwrap();
        set_nonblocking(&stderr).unwrap();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let mut output_truncated = false;
        let mut errors_truncated = false;
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            drain_pipe(&mut stdout, &mut output, &mut output_truncated).unwrap();
            drain_pipe(&mut stderr, &mut errors, &mut errors_truncated).unwrap();
            if let Some(status) = child.try_wait().unwrap() {
                drain_pipe(&mut stdout, &mut output, &mut output_truncated).unwrap();
                drain_pipe(&mut stderr, &mut errors, &mut errors_truncated).unwrap();
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
                drain_pipe(&mut stdout, &mut output, &mut output_truncated).unwrap();
                drain_pipe(&mut stderr, &mut errors, &mut errors_truncated).unwrap();
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

fn set_nonblocking(file: &impl AsRawFd) -> io::Result<()> {
    let descriptor = file.as_raw_fd();
    // SAFETY: `file` is borrowed for this whole call, so `descriptor` is an open fd owned by
    // that value. F_GETFL takes no pointer argument and only reads the status flags.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `flags` is the successful F_GETFL result checked just above (`flags == -1`
    // returns early), OR'd with O_NONBLOCK. F_SETFL takes an int and no pointer, and `file`
    // is still borrowed, so the fd remains open.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_pipe(reader: &mut impl Read, bytes: &mut Vec<u8>, truncated: &mut bool) -> io::Result<()> {
    let mut chunk = [0; 4096];
    let mut drained = 0;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                let retained = count.min(MAX_CAPTURE_BYTES.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&chunk[..retained]);
                *truncated |= retained != count;
                drained += count;
                if drained >= MAX_DRAIN_BYTES_PER_POLL {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
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
