//! Redirection and append are persistence writes. Under a capability policy they
//! are refused before any effect unless the target has a write grant, and a
//! redirect target that is a link is refused rather than followed.
#[cfg(unix)]
use flow::capabilities::HostGrant;
use flow::capabilities::{preflight, AppletGrant, CapabilityPolicy, FilesystemGrant};
use flow::manifest::Registry;
use flow::{parser, runtime};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn grant(capabilities: &[&str], write_roots: Vec<PathBuf>) -> AppletGrant {
    AppletGrant {
        capabilities: capabilities
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        binary_sha256: None,
        environment: BTreeSet::new(),
        filesystem: FilesystemGrant {
            write: write_roots,
            ..FilesystemGrant::default()
        },
        network: false,
        network_origins: BTreeSet::new(),
        sandbox_required: false,
        resource_limits: None,
    }
}

/// A named applet whose binary is this test's own executable. It never runs in
/// these tests, because each refusal happens before the command starts.
fn registry_for(binary: &Path) -> Registry {
    let source = serde_json::json!({
        "name": "fixture.echo",
        "binary": binary,
        "arguments": [],
        "contract": {"version": 1}
    });
    Registry::from_sources(&[source.to_string()]).unwrap()
}

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        ..runtime::RunOptions::new(cwd.to_path_buf())
    }
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_execute"))
}

#[test]
fn stderr_redirect_without_a_write_grant_is_refused_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let registry = registry_for(&binary());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.echo".into(), grant(&["process.exec"], Vec::new()))]),
    }));
    // Trusted mode, so the filesystem grant check is what refuses (untrusted mode
    // refuses earlier on platforms without an OS sandbox).
    registry.set_host_state(Some(flow::capabilities::ExecutionMode::Trusted));
    let program = parser::parse("run fixture.echo and stderr to \"x\"\n").unwrap();
    let error = preflight(&program, &registry, directory.path()).unwrap_err();
    assert!(format!("{error:#}").contains("filesystem"), "{error:#}");
    assert!(!directory.path().join("x").exists());
}

#[test]
fn append_without_a_write_grant_is_refused_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let registry = registry_for(&binary());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.echo".into(), grant(&["process.exec"], Vec::new()))]),
    }));
    let program = parser::parse("run fixture.echo\nthen append to \"x\"\n").unwrap();
    assert!(preflight(&program, &registry, directory.path()).is_err());
    assert!(!directory.path().join("x").exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_redirect_target_is_refused_and_not_written() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("allowed");
    fs::create_dir(&allowed).unwrap();
    let real = allowed.join("real.txt");
    fs::write(&real, b"keep me").unwrap();
    symlink(&real, allowed.join("link.txt")).unwrap();

    let registry = registry_for(&binary());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([(
            "fixture.echo".into(),
            grant(&["process.exec", "filesystem.write"], vec![allowed.clone()]),
        )]),
    }));
    let program = parser::parse("run fixture.echo and stderr to \"allowed/link.txt\"\n").unwrap();
    assert!(runtime::run(&program, &registry, &options(directory.path())).is_err());
    assert_eq!(fs::read(&real).unwrap(), b"keep me");
}

#[cfg(unix)]
#[test]
fn a_symlinked_append_target_is_refused_and_not_written() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("allowed");
    fs::create_dir(&allowed).unwrap();
    let real = allowed.join("real.txt");
    fs::write(&real, b"keep me").unwrap();
    symlink(&real, allowed.join("link.txt")).unwrap();

    let registry = registry_for(&binary());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([(
            "fixture.echo".into(),
            grant(&["process.exec", "filesystem.write"], vec![allowed.clone()]),
        )]),
    }));
    let program = parser::parse("run fixture.echo\nthen append to \"allowed/link.txt\"\n").unwrap();
    assert!(runtime::run(&program, &registry, &options(directory.path())).is_err());
    assert_eq!(fs::read(&real).unwrap(), b"keep me");
}

#[test]
fn a_hard_linked_redirect_target_is_refused_and_not_written() {
    // A second name for the same file would let a write through one name change
    // the other. This runs without a policy: policy paths with a Windows drive
    // prefix are not supported, and the link check does not depend on a grant.
    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("allowed");
    fs::create_dir(&allowed).unwrap();
    let real = allowed.join("real.txt");
    fs::write(&real, b"keep me").unwrap();
    fs::hard_link(&real, allowed.join("alias.txt")).unwrap();

    let registry = registry_for(&binary());
    let program = parser::parse("run fixture.echo and stderr to \"allowed/alias.txt\"\n").unwrap();
    let error = runtime::run(&program, &registry, &options(directory.path()))
        .err()
        .expect("hard-linked target must be refused");
    assert!(format!("{error:#}").contains("hard-linked"), "{error:#}");
    assert_eq!(fs::read(&real).unwrap(), b"keep me");
}

#[cfg(unix)]
#[test]
fn a_new_redirect_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("allowed");
    fs::create_dir(&allowed).unwrap();
    let registry = registry_for(&binary());
    let applet_grant = grant(&["process.exec"], Vec::new());
    assert!(applet_grant.filesystem.read.is_empty());
    assert!(applet_grant.filesystem.write.is_empty());
    registry.set_host_state(Some(CapabilityPolicy {
        host: Some(HostGrant {
            capabilities: BTreeSet::from(["filesystem.write".into()]),
            filesystem: FilesystemGrant {
                write: vec![allowed.clone()],
                ..FilesystemGrant::default()
            },
        }),
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.echo".into(), applet_grant)]),
    }));
    // The applet has no filesystem effect in its contract, so it must not
    // inherit the host's write grant or use that directory as its sandbox cwd.
    // Run it from the read-only system runtime and let the host redirect its
    // stderr to the separately authorized absolute target.
    let target = allowed.join("new.txt");
    let target_source = target.to_str().expect("temporary path must be UTF-8");
    let program = parser::parse(&format!(
        "run fixture.echo and stderr to {:?}\n",
        target_source
    ))
    .unwrap();
    let result = runtime::run(&program, &registry, &options(Path::new("/usr/bin")));
    if let Err(error) = result {
        let message = format!("{error:#}");
        if message.contains("bubblewrap could not establish the required namespaces") {
            assert!(
                !target.exists(),
                "an unavailable required sandbox must fail before creating the redirect target"
            );
            eprintln!("redirect mode skipped: sandbox namespace probe unavailable");
            return;
        }
        assert!(
            message.contains("applet \"fixture.echo\" failed (exit status: 1)"),
            "redirect run failed unexpectedly: {message}"
        );
    }
    let mode = fs::metadata(&target).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}
