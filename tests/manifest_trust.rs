#![cfg(unix)]
//! Adversarial regression coverage for the trusted-input ownership and link
//! gate (issue #450): the manifest directory, manifest files, module root and
//! bundled helper binaries decide what runs, so a group/world-writable path
//! or a symlinked manifest must fail closed before any native launch.
//!
//! The helper-binary refusal runs through a private copy of the built
//! binaries: mutating the shared `target` helper in place would race parallel
//! suites that legitimately launch `@execute-applet`. The refusal logic itself
//! is the same shared gate the manifest and module checks call.
use std::{
    fs,
    io::Read,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use flow::modules::{load_program_module, ProgramModule, PROGRAM_MODULE_INTERFACE_VERSION};

const PROBE_MANIFEST: &str =
    r#"{"name":"probe.ok","binary":"true","parameters":{},"arguments":[]}"#;
const HELPER_MANIFEST: &str =
    r#"{"name":"probe.helper","binary":"@execute-applet","parameters":{},"arguments":[]}"#;
const PROBE_PROGRAM: &str = "run probe.ok\n";
const HELPER_PROGRAM: &str = "run probe.helper\n";
const MODULE_SOURCE: &str =
    "define a program named describe\n  accept payload as json\n  then encode payload as json\n";

fn run_cli(workdir: &Path, script: &str, args: &[&str]) -> Output {
    let program = workdir.join("job.program");
    fs::write(&program, script).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(workdir)
        .arg(&program)
        .args(args)
        .arg("--trace")
        .arg("--command-timeout-ms")
        .arg("1000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_end(&mut stdout)
                .unwrap();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_end(&mut stderr)
                .unwrap();
            return Output {
                status,
                stdout,
                stderr,
            };
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let status = child.wait().unwrap();
            panic!(
                "trusted-input probe hung (status {status}); refusing paths must fail fast"
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn refuses(output: &Output, fragments: &[&str]) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    for fragment in fragments {
        assert!(error.contains(fragment), "{error}");
    }
    assert!(
        !error.contains("\"event\":\"spawn\""),
        "untrusted input launched a native stage: {error}"
    );
}

fn write_manifests(dir: &Path, entries: &[(&str, &str)]) -> PathBuf {
    let manifests = dir.join("manifests");
    fs::create_dir(&manifests).unwrap();
    for (name, body) in entries {
        fs::write(manifests.join(name), body).unwrap();
    }
    manifests
}

#[test]
fn owned_manifest_dir_still_runs_the_probe() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path(), &[("probe.json", PROBE_MANIFEST)]);
    let manifest_arg = manifests.to_string_lossy().into_owned();

    let output = run_cli(
        directory.path(),
        PROBE_PROGRAM,
        &["--manifest-dir", &manifest_arg],
    );

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn group_writable_manifest_dir_is_refused_with_the_path_named() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path(), &[("probe.json", PROBE_MANIFEST)]);
    fs::set_permissions(&manifests, fs::Permissions::from_mode(0o777)).unwrap();
    let manifest_arg = manifests.to_string_lossy().into_owned();

    let output = run_cli(
        directory.path(),
        PROBE_PROGRAM,
        &["--manifest-dir", &manifest_arg],
    );

    refuses(
        &output,
        &[
            "manifest directory",
            &manifests.display().to_string(),
            "group/world-writable",
        ],
    );
    assert_eq!(
        fs::metadata(&manifests).unwrap().permissions().mode() & 0o777,
        0o777
    );
}

#[test]
fn symlinked_manifest_entry_is_refused_with_the_link_named() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path(), &[("probe.json", PROBE_MANIFEST)]);
    let outside = directory.path().join("outside.json");
    fs::write(&outside, PROBE_MANIFEST).unwrap();
    symlink(&outside, manifests.join("link.json")).unwrap();
    let manifest_arg = manifests.to_string_lossy().into_owned();

    let output = run_cli(
        directory.path(),
        PROBE_PROGRAM,
        &["--manifest-dir", &manifest_arg],
    );

    refuses(&output, &["link.json", "symbolic link"]);
}

#[test]
fn world_writable_manifest_file_is_refused_with_the_file_named() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path(), &[("probe.json", PROBE_MANIFEST)]);
    fs::set_permissions(
        manifests.join("probe.json"),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let manifest_arg = manifests.to_string_lossy().into_owned();

    let output = run_cli(
        directory.path(),
        PROBE_PROGRAM,
        &["--manifest-dir", &manifest_arg],
    );

    refuses(&output, &["probe.json", "group/world-writable"]);
}

#[test]
fn single_file_manifest_in_group_writable_parent_is_refused_as_module_root() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let file = parent.join("solo.json");
    fs::write(&file, PROBE_MANIFEST).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
    let file_arg = file.to_string_lossy().into_owned();

    let output = run_cli(directory.path(), PROBE_PROGRAM, &["--manifest-dir", &file_arg]);

    refuses(
        &output,
        &["module root", &parent.display().to_string(), "group/world-writable"],
    );
}

#[test]
fn fifo_manifest_entry_fails_closed_without_hanging() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path(), &[("probe.json", PROBE_MANIFEST)]);
    let fifo = manifests.join("trap.json");
    let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `path` is a CString built just above; CString::new rejected interior NULs, so
    // as_ptr is NUL-terminated, and the CString outlives the call. mkfifo only reads the path.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let manifest_arg = manifests.to_string_lossy().into_owned();

    let output = run_cli(
        directory.path(),
        PROBE_PROGRAM,
        &["--manifest-dir", &manifest_arg],
    );

    refuses(&output, &["trap.json", "regular file"]);
}

#[test]
fn group_writable_helper_binary_is_refused_with_the_helper_named() {
    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let installed = Path::new(env!("CARGO_BIN_EXE_execute"));
    let helper_source = installed.with_file_name("execute-applet");
    assert!(
        helper_source.is_file(),
        "test requires the built execute-applet helper beside {}",
        installed.display()
    );
    fs::copy(installed, bin.join("execute")).unwrap();
    fs::set_permissions(bin.join("execute"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::copy(&helper_source, bin.join("execute-applet")).unwrap();
    fs::set_permissions(bin.join("execute-applet"), fs::Permissions::from_mode(0o666)).unwrap();
    let manifests = write_manifests(directory.path(), &[("helper.json", HELPER_MANIFEST)]);
    let program = directory.path().join("helper.program");
    fs::write(&program, HELPER_PROGRAM).unwrap();

    let output = Command::new(bin.join("execute"))
        .current_dir(directory.path())
        .arg(&program)
        .arg("--manifest-dir")
        .arg(&manifests)
        .arg("--trace")
        .arg("--command-timeout-ms")
        .arg("1000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    refuses(
        &output,
        &[
            "bundled execute-applet helper",
            "execute-applet",
            "group/world-writable",
        ],
    );
}

#[test]
fn world_writable_module_source_is_refused_with_the_source_named() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("module-root");
    fs::create_dir(&root).unwrap();
    let applet_manifest = root.join("probe.json");
    fs::write(&applet_manifest, "{}").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    let source_path = root.join("src/library.program");
    fs::write(&source_path, MODULE_SOURCE).unwrap();
    let reference = ProgramModule {
        source: PathBuf::from("src/library.program"),
        entry: "describe".into(),
        interface_version: PROGRAM_MODULE_INTERFACE_VERSION,
        package_manifest: None,
        package_lock: None,
    };

    load_program_module(&reference, &applet_manifest, &root).unwrap();

    fs::set_permissions(&source_path, fs::Permissions::from_mode(0o666)).unwrap();
    let error = format!(
        "{:#}",
        load_program_module(&reference, &applet_manifest, &root).unwrap_err()
    );

    assert!(
        error.contains("library.program") && error.contains("group/world-writable"),
        "{error}"
    );
    assert_eq!(
        fs::metadata(&source_path).unwrap().permissions().mode() & 0o777,
        0o666
    );
}
