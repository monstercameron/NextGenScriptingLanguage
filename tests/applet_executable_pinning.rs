#![cfg(unix)]

//! Acceptance regressions for issue #514: manifest-selected executables must
//! not resolve against the runtime cwd/PATH shadows, and a manifest-level
//! `binary_sha256` mismatch must refuse the launch. Policy tests prove the
//! preflight-checked executable is the executed executable: the grant
//! authorization and both pin types bind the manifest-directory file. Symlink
//! escapes (final-component and intermediate) are refused by canonical
//! confinement, while links resolving inside the catalog still run.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write_executable(path: &Path, marker: &str) {
    fs::write(path, format!("#!/bin/sh\nprintf '{marker}\\n'\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn run_case(
    workdir: &Path,
    catalog: &Path,
    source: &str,
    path_value: Option<&str>,
) -> std::process::Output {
    run_case_with_policy(workdir, catalog, source, path_value, None)
}

fn run_case_with_policy(
    workdir: &Path,
    catalog: &Path,
    source: &str,
    path_value: Option<&str>,
    policy: Option<&Path>,
) -> std::process::Output {
    let script = workdir.join("case.program");
    fs::write(&script, source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(workdir)
        .arg(&script)
        .arg("--manifest-dir")
        .arg(catalog);
    if let Some(path_value) = path_value {
        command.env("PATH", path_value);
    }
    if let Some(policy) = policy {
        // A policy without an explicit mode defaults to untrusted, which
        // demands OS sandboxing. Trusted mode keeps the allowlist, pin, and
        // preflight checks these tests exercise without requiring a sandbox.
        command
            .arg("--capability-policy")
            .arg(policy)
            .args(["--execution-mode", "trusted"]);
    }
    command.output().unwrap()
}

fn write_policy(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

const COMMAND_CONTRACT: &str = r#""contract":{"version":1,"backend":"command","lifecycle":{"kind":"oneshot"},"capabilities":["process.exec"],"effects":[]}"#;

fn write_command_manifest(catalog: &Path, name: &str, binary: &str, pin: Option<&str>) {
    let pin_field = pin
        .map(|digest| format!(r#","binary_sha256":"{digest}""#))
        .unwrap_or_default();
    fs::write(
        catalog.join(format!("{name}.json")),
        format!(
            r#"{{"name":"{name}","binary":"{binary}","arguments":[],{COMMAND_CONTRACT}{pin_field}}}"#
        ),
    )
    .unwrap();
}

#[test]
fn relative_path_entries_and_cwd_shadow_do_not_change_applet_binary() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let real = base.join("real");
    let shadow = base.join("shadow");
    let catalog = base.join("catalog");
    for dir in [&real, &shadow, &catalog] {
        fs::create_dir(dir).unwrap();
    }
    write_executable(&real.join("probe"), "REAL");
    write_executable(&shadow.join("probe"), "SHADOW");
    fs::write(
        catalog.join("probe.json"),
        r#"{"name":"probe","binary":"probe","arguments":[]}"#,
    )
    .unwrap();
    // Leading empty entry (from `:`) and `.` would both resolve against the
    // runtime cwd under the old lookup; both must be ignored now.
    let path_value = format!(":{}:.", real.display());
    let result = run_case(&shadow, &catalog, "run probe\n", Some(&path_value));
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"REAL\n");
}

#[test]
fn slash_binary_anchors_at_manifest_directory_not_runtime_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let shadow = base.join("shadow");
    let catalog = base.join("catalog");
    fs::create_dir(&shadow).unwrap();
    fs::create_dir(catalog.join("sub").as_path().parent().unwrap()).unwrap();
    fs::create_dir(catalog.join("sub")).unwrap();
    fs::create_dir(shadow.join("sub")).unwrap();
    write_executable(&catalog.join("sub").join("tool"), "MANIFEST-DIR");
    write_executable(&shadow.join("sub").join("tool"), "SHADOW");
    fs::write(
        catalog.join("tool.json"),
        r#"{"name":"tool","binary":"sub/tool","arguments":[]}"#,
    )
    .unwrap();
    let result = run_case(&shadow, &catalog, "run tool\n", None);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"MANIFEST-DIR\n");
}

#[test]
fn manifest_binary_sha256_mismatch_refuses_launch() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    fs::create_dir(&catalog).unwrap();
    let pinned: PathBuf = catalog.join("pinned");
    write_executable(&pinned, "GOOD");
    let digest = flow::capabilities::sha256_file(&pinned).unwrap();
    fs::write(
        catalog.join("pinned.json"),
        format!(
            r#"{{"name":"pinned","binary":"{path}","binary_sha256":"{digest}","arguments":[]}}"#,
            path = pinned.display()
        ),
    )
    .unwrap();
    let good = run_case(base, &catalog, "run pinned\n", None);
    assert!(
        good.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&good.stderr)
    );
    assert_eq!(good.stdout, b"GOOD\n");
    // Replacing the executable after pinning must refuse the launch, even
    // though the plan and the manifest text are unchanged.
    write_executable(&pinned, "REPLACED");
    let bad = run_case(base, &catalog, "run pinned\n", None);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("hash mismatch"),
        "stderr: {}",
        String::from_utf8_lossy(&bad.stderr)
    );
}

#[test]
fn policy_preflight_checks_the_manifest_dir_executable_not_the_cwd_shadow() {
    // A cwd-planted `sub/tool` must neither satisfy the policy preflight nor
    // run: the grant authorization and the manifest pin both bind the
    // manifest-directory file, which is also the file launch executes.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let shadow = base.join("shadow");
    let catalog = base.join("catalog");
    fs::create_dir(&shadow).unwrap();
    fs::create_dir(catalog.join("sub").as_path().parent().unwrap()).unwrap();
    fs::create_dir(catalog.join("sub")).unwrap();
    fs::create_dir(shadow.join("sub")).unwrap();
    write_executable(&catalog.join("sub").join("tool"), "MANIFEST-DIR");
    write_executable(&shadow.join("sub").join("tool"), "SHADOW");
    let digest = flow::capabilities::sha256_file(&catalog.join("sub").join("tool")).unwrap();
    write_command_manifest(&catalog, "tool", "sub/tool", Some(&digest));
    let policy = base.join("policy.json");
    write_policy(
        &policy,
        r#"{"version":1,"applets":{"tool":{"capabilities":["process.exec"]}}}"#,
    );
    let result = run_case_with_policy(&shadow, &catalog, "run tool\n", None, Some(&policy));
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"MANIFEST-DIR\n");
}

#[test]
fn manifest_pin_matching_only_the_cwd_shadow_refuses_launch() {
    // Adversarial mirror: a pin that matches the cwd shadow but not the
    // manifest-directory file must refuse, proving the checked file is the
    // executed file even though a matching file exists in the cwd.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let shadow = base.join("shadow");
    let catalog = base.join("catalog");
    fs::create_dir(&shadow).unwrap();
    fs::create_dir(catalog.join("sub").as_path().parent().unwrap()).unwrap();
    fs::create_dir(catalog.join("sub")).unwrap();
    fs::create_dir(shadow.join("sub")).unwrap();
    write_executable(&catalog.join("sub").join("tool"), "MANIFEST-DIR");
    write_executable(&shadow.join("sub").join("tool"), "SHADOW");
    let shadow_digest = flow::capabilities::sha256_file(&shadow.join("sub").join("tool")).unwrap();
    write_command_manifest(&catalog, "tool", "sub/tool", Some(&shadow_digest));
    let result = run_case(&shadow, &catalog, "run tool\n", None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("hash mismatch"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn manifest_pin_and_policy_pin_must_both_match_the_executed_file() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    fs::create_dir(&catalog).unwrap();
    let pinned: PathBuf = catalog.join("pinned");
    write_executable(&pinned, "GOOD");
    let digest = flow::capabilities::sha256_file(&pinned).unwrap();
    write_command_manifest(
        &catalog,
        "pinned",
        &pinned.display().to_string(),
        Some(&digest),
    );
    let policy = base.join("policy.json");
    // Both pins name the executed file: the launch proceeds.
    write_policy(
        &policy,
        &format!(
            r#"{{"version":1,"applets":{{"pinned":{{"capabilities":["process.exec"],"binary_sha256":"{digest}"}}}}}}"#
        ),
    );
    let good = run_case_with_policy(base, &catalog, "run pinned\n", None, Some(&policy));
    assert!(
        good.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&good.stderr)
    );
    assert_eq!(good.stdout, b"GOOD\n");
    // The manifest pin still names the executed file but the policy pin names
    // another file: preflight and launch disagree with the grant, so the
    // launch is refused.
    write_executable(&catalog.join("other"), "OTHER");
    let other_digest = flow::capabilities::sha256_file(&catalog.join("other")).unwrap();
    write_policy(
        &policy,
        &format!(
            r#"{{"version":1,"applets":{{"pinned":{{"capabilities":["process.exec"],"binary_sha256":"{other_digest}"}}}}}}"#
        ),
    );
    let bad = run_case_with_policy(base, &catalog, "run pinned\n", None, Some(&policy));
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("hash mismatch"),
        "stderr: {}",
        String::from_utf8_lossy(&bad.stderr)
    );
}

#[test]
fn manifest_pin_accepts_uppercase_hex() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    fs::create_dir(&catalog).unwrap();
    let pinned: PathBuf = catalog.join("pinned");
    write_executable(&pinned, "GOOD");
    let digest = flow::capabilities::sha256_file(&pinned)
        .unwrap()
        .to_uppercase();
    write_command_manifest(
        &catalog,
        "pinned",
        &pinned.display().to_string(),
        Some(&digest),
    );
    let good = run_case(base, &catalog, "run pinned\n", None);
    assert!(
        good.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&good.stderr)
    );
    assert_eq!(good.stdout, b"GOOD\n");
}

#[test]
fn dotdot_binary_cannot_escape_the_manifest_directory() {
    // `sub/../../evil` would resolve outside the catalog; the traversal
    // boundary refuses it before any filesystem access, so the planted file
    // never runs.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    fs::create_dir(&catalog).unwrap();
    fs::create_dir(&workdir).unwrap();
    let marker = base.join("evil-ran");
    fs::write(
        base.join("evil"),
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(base.join("evil"), fs::Permissions::from_mode(0o700)).unwrap();
    write_command_manifest(&catalog, "evil", "sub/../../evil", None);
    let result = run_case(&workdir, &catalog, "run evil\n", None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("escapes its manifest directory"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!marker.exists(), "the escaped executable must not run");
}

#[test]
fn final_symlink_cannot_escape_the_manifest_directory() {
    // `sub/link` is lexically contained, but the final component is a
    // symlink to an executable outside the catalog. Canonical confinement
    // must refuse it before any filesystem execution, so the planted file
    // never runs.
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    let outside = base.join("outside");
    for dir in [&catalog, &workdir, &outside] {
        fs::create_dir(dir).unwrap();
    }
    fs::create_dir(catalog.join("sub")).unwrap();
    let marker = base.join("evil-ran");
    let evil = outside.join("evil");
    fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    fs::set_permissions(&evil, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&evil, catalog.join("sub").join("link")).unwrap();
    write_command_manifest(&catalog, "evil", "sub/link", None);
    let result = run_case(&workdir, &catalog, "run evil\n", None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("escapes its manifest directory"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!marker.exists(), "the escaped executable must not run");
}

#[test]
fn intermediate_symlink_cannot_escape_the_manifest_directory() {
    // Here `sub` itself is a symlink to a directory outside the catalog, so
    // the lexically contained `sub/tool` resolves outside it. The lexical
    // check cannot see this; only canonical confinement catches it.
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    let outside = base.join("outside");
    for dir in [&catalog, &workdir, &outside] {
        fs::create_dir(dir).unwrap();
    }
    let marker = base.join("evil-ran");
    fs::write(
        outside.join("tool"),
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(outside.join("tool"), fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&outside, catalog.join("sub")).unwrap();
    write_command_manifest(&catalog, "evil", "sub/tool", None);
    let result = run_case(&workdir, &catalog, "run evil\n", None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("escapes its manifest directory"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!marker.exists(), "the escaped executable must not run");
}

#[test]
fn symlink_inside_the_manifest_directory_still_runs() {
    // Canonical confinement is not a blanket symlink ban: a link that
    // resolves to a file still inside the catalog keeps working.
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    fs::create_dir(&catalog).unwrap();
    fs::create_dir(&workdir).unwrap();
    fs::create_dir(catalog.join("real")).unwrap();
    fs::create_dir(catalog.join("sub")).unwrap();
    write_executable(&catalog.join("real").join("tool"), "MANIFEST-DIR");
    symlink(
        Path::new("..").join("real").join("tool"),
        catalog.join("sub").join("link"),
    )
    .unwrap();
    write_command_manifest(&catalog, "tool", "sub/link", None);
    let result = run_case(&workdir, &catalog, "run tool\n", None);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"MANIFEST-DIR\n");
}

#[test]
fn manifest_binary_sha256_rejects_malformed_and_helper_pins() {
    let bad_digest =
        r#"{"name":"x","binary":"echo","binary_sha256":"not-a-digest","arguments":[]}"#;
    assert!(flow::manifest::Registry::from_sources(&[bad_digest.to_owned()]).is_err());
    let helper_pin = r#"{"name":"x","binary":"@execute-applet","binary_sha256":"0000000000000000000000000000000000000000000000000000000000000000","arguments":[]}"#;
    assert!(flow::manifest::Registry::from_sources(&[helper_pin.to_owned()]).is_err());
}

#[test]
fn dangling_final_symlink_refuses_launch() {
    // The link itself is lexically inside the catalog, but its target does not
    // exist. Canonical confinement must fail closed before any execution.
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    fs::create_dir(&catalog).unwrap();
    fs::create_dir(&workdir).unwrap();
    fs::create_dir(catalog.join("sub")).unwrap();
    symlink(
        base.join("no-such-target"),
        catalog.join("sub").join("link"),
    )
    .unwrap();
    write_command_manifest(&catalog, "evil", "sub/link", None);
    let result = run_case(&workdir, &catalog, "run evil\n", None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("manifest directory"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn dangling_intermediate_symlink_refuses_launch() {
    // `sub` is a symlink to a directory that does not exist, so `sub/tool`
    // cannot be confined to the catalog. The launch is refused without
    // executing anything.
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    fs::create_dir(&catalog).unwrap();
    fs::create_dir(&workdir).unwrap();
    symlink(base.join("no-such-dir"), catalog.join("sub")).unwrap();
    write_command_manifest(&catalog, "evil", "sub/tool", None);
    let result = run_case(&workdir, &catalog, "run evil\n", None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("manifest directory"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn directory_as_applet_binary_refuses_launch() {
    // A catalog-relative binary that resolves to a directory (not a file) is
    // refused after confinement instead of being executed.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    fs::create_dir(&catalog).unwrap();
    fs::create_dir(&workdir).unwrap();
    fs::create_dir(catalog.join("sub")).unwrap();
    fs::create_dir(catalog.join("sub").join("tool")).unwrap();
    write_command_manifest(&catalog, "evil", "sub/tool", None);
    let result = run_case(&workdir, &catalog, "run evil\n", None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("is not an executable file"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn bare_name_with_only_relative_path_entries_is_not_found() {
    // With only relative PATH entries, the hardened lookup ignores them all,
    // so a cwd-planted executable can never win and the lookup fails closed.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    fs::create_dir(&catalog).unwrap();
    write_executable(&base.join("probe"), "SHADOW");
    fs::write(
        catalog.join("probe.json"),
        r#"{"name":"probe","binary":"probe","arguments":[]}"#,
    )
    .unwrap();
    let result = run_case(base, &catalog, "run probe\n", Some("rel:.:bin"));
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("not found in PATH"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn slash_relative_binary_without_manifest_origin_is_refused() {
    // Synthetic registries record no manifest origin, so a `/`-bearing binary
    // would anchor at whatever the runtime cwd happens to be. Resolution
    // refuses instead of running a cwd-planted file.
    let temp = tempfile::tempdir().unwrap();
    let shadow = temp.path().join("shadow");
    fs::create_dir_all(shadow.join("sub")).unwrap();
    write_executable(&shadow.join("sub").join("tool"), "SHADOW");
    let registry = flow::manifest::Registry::from_sources(&[
        r#"{"name":"tool","binary":"sub/tool","arguments":[]}"#.to_owned(),
    ])
    .unwrap();
    let program = flow::parser::parse("run tool\n").unwrap();
    let options = flow::runtime::RunOptions {
        max_output_bytes: 65_536,
        command_timeout: Some(std::time::Duration::from_secs(30)),
        ..flow::runtime::RunOptions::new(shadow)
    };
    let error = flow::runtime::run(&program, &registry, &options)
        .err()
        .expect("the unresolved applet must be refused");
    assert!(
        format!("{error:#}").contains("without a manifest origin"),
        "unexpected error: {error:#}"
    );
}
