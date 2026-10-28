#![cfg(unix)]
//! Adversarial regression coverage for the trusted-input ownership and link
//! gate (issue #450): the manifest directory, manifest files, module root and
//! bundled helper binaries decide what runs, so a group/world-writable,
//! hardlinked or setuid path, a symlinked manifest, or a writable ancestor
//! that could replace the verified leaf must fail closed before any native
//! launch. Explicit single-file and catalog aliases still resolve to a
//! verified target; embedded links inside directory scans stay refused.
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

use flow::manifest::Registry;
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
            panic!("trusted-input probe hung (status {status}); refusing paths must fail fast");
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

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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

    let output = run_cli(
        directory.path(),
        PROBE_PROGRAM,
        &["--manifest-dir", &file_arg],
    );

    refuses(
        &output,
        &[
            "module root",
            &parent.display().to_string(),
            "group/world-writable",
        ],
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
    fs::set_permissions(
        bin.join("execute-applet"),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
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
fn writable_parent_of_manifest_dir_is_refused_with_the_parent_named() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let manifests = write_manifests(&parent, &[("probe.json", PROBE_MANIFEST)]);
    // A group-writable parent lets a group member rename or replace the
    // verified leaf, so the ancestor check must fail closed and name it.
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o775)).unwrap();
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
            &parent.display().to_string(),
            "group/world-writable",
        ],
    );
}

#[test]
fn hardlinked_manifest_entry_is_refused_with_a_link_error() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path(), &[("probe.json", PROBE_MANIFEST)]);
    fs::hard_link(manifests.join("probe.json"), manifests.join("extra.json")).unwrap();
    let manifest_arg = manifests.to_string_lossy().into_owned();

    let output = run_cli(
        directory.path(),
        PROBE_PROGRAM,
        &["--manifest-dir", &manifest_arg],
    );

    // Both names now share one inode, so either may be reported; the gate
    // must refuse the extra link instead of trusting either name.
    refuses(&output, &["hardlink"]);
}

/// Whether this environment can create a setuid fixture: sandboxes may
/// silently strip privileged bits (observed on macOS: `chmod 4644` reports
/// success but the bit never lands). Callers skip instead of asserting a
/// gate they cannot feed.
fn try_make_setuid(path: &Path) -> bool {
    fs::set_permissions(path, fs::Permissions::from_mode(0o4644)).is_ok()
        && fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o6000 != 0)
}

#[test]
fn setuid_manifest_file_is_refused_with_the_file_named() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path(), &[("probe.json", PROBE_MANIFEST)]);
    if !try_make_setuid(&manifests.join("probe.json")) {
        eprintln!("SKIP: environment cannot create setuid fixtures");
        return;
    }
    let manifest_arg = manifests.to_string_lossy().into_owned();

    let output = run_cli(
        directory.path(),
        PROBE_PROGRAM,
        &["--manifest-dir", &manifest_arg],
    );

    refuses(&output, &["probe.json", "setuid"]);
}

#[test]
fn setuid_helper_binary_is_refused_with_the_helper_named() {
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
    // Skip where the environment strips privileged bits instead of asserting
    // a gate the fixture cannot feed (see `try_make_setuid`).
    let _ = fs::set_permissions(
        bin.join("execute-applet"),
        fs::Permissions::from_mode(0o4755),
    );
    if fs::symlink_metadata(bin.join("execute-applet"))
        .is_ok_and(|metadata| metadata.permissions().mode() & 0o4000 == 0)
    {
        eprintln!("SKIP: environment cannot create setuid fixtures");
        return;
    }
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
        &["bundled execute-applet helper", "execute-applet", "setuid"],
    );
}

#[test]
fn socket_module_source_is_refused_as_non_regular() {
    use std::os::unix::net::UnixListener;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("module-root");
    fs::create_dir(&root).unwrap();
    let applet_manifest = root.join("probe.json");
    fs::write(&applet_manifest, "{}").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    let source_path = root.join("src/library.program");
    let _socket = UnixListener::bind(&source_path).unwrap();
    let reference = ProgramModule {
        source: PathBuf::from("src/library.program"),
        entry: "describe".into(),
        interface_version: PROGRAM_MODULE_INTERFACE_VERSION,
        package_manifest: None,
        package_lock: None,
    };

    let error = format!(
        "{:#}",
        load_program_module(&reference, &applet_manifest, &root).unwrap_err()
    );

    assert!(
        error.contains("library.program") && error.contains("regular file"),
        "{error}"
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

fn write_catalog(dir: &Path, name: &str) -> PathBuf {
    let catalog = dir.join(name);
    let body = format!(
        "{{\"format\":\"execute.manifest-catalog\",\"version\":1,\"manifests\":[{PROBE_MANIFEST}]}}"
    );
    fs::write(&catalog, body).unwrap();
    catalog
}

#[test]
fn world_writable_catalog_is_refused_with_the_catalog_named() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = write_catalog(directory.path(), "applets.manifest-catalog");
    fs::set_permissions(&catalog, fs::Permissions::from_mode(0o666)).unwrap();

    let error = format!(
        "{:#}",
        Registry::load_catalog(&catalog)
            .err()
            .expect("world-writable catalogs must fail")
    );

    assert!(
        error.contains("manifest catalog")
            && error.contains("applets.manifest-catalog")
            && error.contains("group/world-writable"),
        "{error}"
    );
}

#[test]
fn catalog_symlink_alias_loads_its_verified_target() {
    // An explicitly named catalog may be a symlink alias (like a single
    // manifest file): the alias resolves to a trusted target and loads, while
    // embedded links inside directory scans stay refused.
    let directory = tempfile::tempdir().unwrap();
    let catalog = write_catalog(directory.path(), "applets.manifest-catalog");
    let alias = directory.path().join("alias.manifest-catalog");
    symlink(&catalog, &alias).unwrap();

    let packed = Registry::load_catalog(&alias).unwrap();

    assert!(packed.resolve("probe.ok").is_ok());
}

#[test]
fn catalog_in_writable_parent_is_refused_with_the_parent_named() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let catalog = write_catalog(&parent, "applets.manifest-catalog");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();

    let error = format!(
        "{:#}",
        Registry::load_catalog(&catalog)
            .err()
            .expect("catalogs under writable parents must fail")
    );

    assert!(
        error.contains(&parent.display().to_string()) && error.contains("group/world-writable"),
        "{error}"
    );
}

#[test]
fn single_file_manifest_symlink_alias_loads_its_verified_target() {
    // Same alias compatibility as catalogs: an explicitly named manifest file
    // may be a symlink to a trusted target.
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real.json");
    fs::write(&real, PROBE_MANIFEST).unwrap();
    let alias = directory.path().join("alias.json");
    symlink(&real, &alias).unwrap();

    let registry = Registry::load(&alias).unwrap();

    assert!(registry.resolve("probe.ok").is_ok());
}

#[test]
fn bundled_default_root_passes_the_same_gate() {
    let owned = tempfile::tempdir().unwrap();
    Registry::from_bundled(owned.path()).unwrap();

    let exposed = tempfile::tempdir().unwrap();
    fs::set_permissions(exposed.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let error = format!(
        "{:#}",
        Registry::from_bundled(exposed.path())
            .err()
            .expect("writable default roots must fail")
    );

    assert!(
        error.contains("manifest directory") && error.contains("group/world-writable"),
        "{error}"
    );
}

#[test]
fn manifest_directory_symlink_alias_loads_verified_canonical_target() {
    // A directory alias swap must resolve to the verified canonical target:
    // listing the canonical directory (not the alias) keeps the entries that
    // passed the gate identical to the entries that are read.
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path(), &[("probe.json", PROBE_MANIFEST)]);
    let alias = directory.path().join("manifests-alias");
    symlink(&manifests, &alias).unwrap();

    let registry = Registry::load(&alias).unwrap();

    assert!(registry.resolve("probe.ok").is_ok());
}

#[test]
fn single_file_alias_to_untrusted_target_is_refused() {
    // A path swap that points an alias at a writable target must fail closed
    // at the canonical target, naming the real file rather than the alias.
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real.json");
    fs::write(&real, PROBE_MANIFEST).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o666)).unwrap();
    let alias = directory.path().join("alias.json");
    symlink(&real, &alias).unwrap();

    let error = format!(
        "{:#}",
        Registry::load(&alias)
            .err()
            .expect("aliases to untrusted targets must fail")
    );

    assert!(
        error.contains("real.json") && error.contains("group/world-writable"),
        "{error}"
    );
}

#[test]
fn writable_manifest_named_binary_is_refused_before_launch() {
    // A trusted manifest naming a group/world-writable executable must be
    // refused at resolution time: the binary decides what runs, so it passes
    // the same gate as the manifest itself.
    let directory = tempfile::tempdir().unwrap();
    let tools = directory.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let executable = tools.join("probe-bin");
    fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o777)).unwrap();
    let manifest =
        r#"{"name":"probe.bin","binary":"tools/probe-bin","parameters":{},"arguments":[]}"#;
    let manifests = write_manifests(directory.path(), &[("bin.json", manifest)]);
    let manifest_arg = manifests.to_string_lossy().into_owned();

    let output = run_cli(
        directory.path(),
        "run probe.bin\n",
        &["--manifest-dir", &manifest_arg],
    );

    refuses(
        &output,
        &["executable", "probe-bin", "group/world-writable"],
    );
}

#[test]
fn writable_parent_of_manifest_named_binary_is_refused() {
    // An ancestor swap for the selected executable is the same attack as a
    // manifest ancestor swap: a writable parent could replace the verified
    // binary before it is launched.
    let directory = tempfile::tempdir().unwrap();
    let tools = directory.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let executable = tools.join("probe-bin");
    fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&tools, fs::Permissions::from_mode(0o777)).unwrap();
    let manifest =
        r#"{"name":"probe.bin","binary":"tools/probe-bin","parameters":{},"arguments":[]}"#;
    let manifests = write_manifests(directory.path(), &[("bin.json", manifest)]);
    let manifest_arg = manifests.to_string_lossy().into_owned();

    let output = run_cli(
        directory.path(),
        "run probe.bin\n",
        &["--manifest-dir", &manifest_arg],
    );

    refuses(
        &output,
        &[
            "executable",
            &tools.display().to_string(),
            "group/world-writable",
        ],
    );
}
