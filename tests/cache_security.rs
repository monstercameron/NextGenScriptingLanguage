#![cfg(unix)]
//! Real CLI cache boundaries with private files and bounded owned processes.
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

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
            .next()
            .unwrap()
            .unwrap()
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
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
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

#[test]
fn ordinary_existing_cache_is_reused_without_launching_or_changing_key_bytes() {
    let fixture = Fixture::new();
    let bytes = fs::read(&fixture.entry).unwrap();
    let output = fixture.run(None);
    assert!(output.status.success());
    assert_eq!(output.stdout, bytes);
    assert!(String::from_utf8_lossy(&output.stderr).contains("cache_hit"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("\"event\":\"spawn\""));
    assert_eq!(
        fs::read_dir(fixture.entry.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
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
    fixture.rejects(None, "not writable by other users");
    assert_eq!(
        fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o777
    );
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&fixture.entry, fs::Permissions::from_mode(0o666)).unwrap();
    fixture.rejects(None, "not writable by other users");
    assert_eq!(
        fs::metadata(&fixture.entry).unwrap().permissions().mode() & 0o777,
        0o666
    );
}
