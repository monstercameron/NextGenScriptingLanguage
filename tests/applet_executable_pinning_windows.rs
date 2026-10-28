#![cfg(windows)]

//! Native Windows acceptance regressions for issue #514: manifest-selected
//! executables must not resolve against the runtime cwd/PATH shadows, and
//! manifest/policy `binary_sha256` pins must bind the executed file.
//!
//! Every positive test executes a real Windows binary: fixtures are copies of
//! the owned `native-fixture` PE, so `CARGO_BIN_EXE_native-fixture` must be
//! built (the `test-fixtures` feature). Escape tests refuse before any spawn;
//! symlink containment tests skip gracefully when the runner cannot create
//! symlinks (missing SeCreateSymbolicLink privilege), while the always-runnable
//! lexical, missing-file, and policy tests still pin the behavior.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_native-fixture"))
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
        // A policy without an explicit mode defaults to untrusted and demands
        // OS sandboxing. Trusted mode keeps the allowlist, pin, and preflight
        // checks these tests exercise without requiring a sandbox.
        command
            .arg("--capability-policy")
            .arg(policy)
            .args(["--execution-mode", "trusted"]);
    }
    command.output().unwrap()
}

/// Copy the owned fixture PE so the test executes a real Windows binary.
fn place_fixture(dest: &Path) {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::copy(fixture_exe(), dest).unwrap();
}

/// Plant a shadow that is not a valid PE: if legacy resolution ever selected
/// it, the spawn would fail instead of printing the expected marker.
fn place_broken(dest: &Path) {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(dest, b"not a windows executable").unwrap();
}

fn write_manifest(catalog: &Path, name: &str, manifest: &serde_json::Value) {
    fs::write(catalog.join(format!("{name}.json")), manifest.to_string()).unwrap();
}

fn write_policy(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
}

fn command_manifest(
    name: &str,
    binary: &str,
    arguments: serde_json::Value,
    pin: Option<&str>,
) -> serde_json::Value {
    let mut manifest = serde_json::json!({
        "name": name,
        "binary": binary,
        "arguments": arguments,
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "capabilities": ["process.exec"],
            "effects": []
        }
    });
    if let Some(digest) = pin {
        manifest["binary_sha256"] = serde_json::Value::String(digest.to_owned());
    }
    manifest
}

fn write_args(literals: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        literals
            .iter()
            .map(|literal| serde_json::json!({"literal": literal}))
            .collect(),
    )
}

#[test]
fn bare_name_path_ignores_cwd_and_relative_entries() {
    // A cwd-planted `probe.exe` must neither satisfy the lookup nor run: only
    // absolute PATH entries are searched, so the shadow (an invalid PE that
    // could never print REAL) cannot win over the real entry.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let real = base.join("real");
    let shadow = base.join("shadow");
    let catalog = base.join("catalog");
    for dir in [&real, &shadow, &catalog] {
        fs::create_dir(dir).unwrap();
    }
    place_fixture(&real.join("probe.exe"));
    place_broken(&shadow.join("probe.exe"));
    write_manifest(
        &catalog,
        "probe",
        &serde_json::json!({
            "name": "probe",
            "binary": "probe",
            "arguments": [{"literal": "write"}, {"literal": "REAL"}],
        }),
    );
    // Leading empty entry, `.`, a bare relative entry, a drive-relative
    // entry (`C:foo`), and a rooted entry without a drive (`\foo`) would all
    // resolve against the runtime cwd under the old lookup; all are ignored
    // now, so the shadow (an invalid PE) cannot win over the real entry.
    let path_value = format!(
        ";.;rel;C:no-such-drive-relative-514;\\no-such-rooted-514;{}",
        real.display()
    );
    let result = run_case(&shadow, &catalog, "run probe\n", Some(&path_value));
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"REAL");
}

#[test]
fn slash_binary_anchors_at_manifest_directory_not_runtime_cwd() {
    // `sub/tool` maps to `sub/tool.exe` inside the catalog. The runtime-cwd
    // shadow holds an invalid PE, so anchoring anywhere but the manifest
    // directory would fail instead of printing MANIFEST-DIR.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let shadow = base.join("shadow");
    let catalog = base.join("catalog");
    fs::create_dir(&shadow).unwrap();
    place_fixture(&catalog.join("sub").join("tool.exe"));
    place_broken(&shadow.join("sub").join("tool.exe"));
    write_manifest(
        &catalog,
        "tool",
        &serde_json::json!({
            "name": "tool",
            "binary": "sub/tool",
            "arguments": [{"literal": "write"}, {"literal": "MANIFEST-DIR"}],
        }),
    );
    let result = run_case(&shadow, &catalog, "run tool\n", None);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"MANIFEST-DIR");
}

#[test]
fn backslash_binary_anchors_at_manifest_directory_not_runtime_cwd() {
    // Backslash anchoring is Windows-only (`cfg(windows)`): `sub\tool` must
    // resolve inside the catalog on Windows even though the same manifest
    // text is a bare PATH name on Unix.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let shadow = base.join("shadow");
    let catalog = base.join("catalog");
    fs::create_dir(&shadow).unwrap();
    place_fixture(&catalog.join("sub").join("tool.exe"));
    place_broken(&shadow.join("sub").join("tool.exe"));
    write_manifest(
        &catalog,
        "tool",
        &serde_json::json!({
            "name": "tool",
            "binary": "sub\\tool",
            "arguments": [{"literal": "write"}, {"literal": "MANIFEST-DIR"}],
        }),
    );
    let result = run_case(&shadow, &catalog, "run tool\n", None);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"MANIFEST-DIR");
}

#[test]
fn dotdot_binary_cannot_escape_the_manifest_directory() {
    // `sub/../../evil` would resolve outside the catalog; the traversal
    // boundary refuses it before any spawn. The fixture's `late-write` mode
    // would create the marker if it ever ran, so its absence proves refusal.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    fs::create_dir(&catalog).unwrap();
    fs::create_dir(&workdir).unwrap();
    let marker = base.join("evil-ran");
    write_manifest(
        &catalog,
        "evil",
        &serde_json::json!({
            "name": "evil",
            "binary": "sub/../../evil",
            "arguments": [{"literal": "late-write"}, {"literal": marker.to_str().unwrap()}],
        }),
    );
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
fn manifest_binary_sha256_mismatch_refuses_launch() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    fs::create_dir(&catalog).unwrap();
    let pinned = catalog.join("pinned.exe");
    place_fixture(&pinned);
    let digest = flow::capabilities::sha256_file(&pinned).unwrap();
    write_manifest(
        &catalog,
        "pinned",
        &command_manifest(
            "pinned",
            pinned.to_str().unwrap(),
            write_args(&["write", "GOOD"]),
            Some(&digest),
        ),
    );
    let good = run_case(base, &catalog, "run pinned\n", None);
    assert!(
        good.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&good.stderr)
    );
    assert_eq!(good.stdout, b"GOOD");
    // Replacing the executable after pinning must refuse the launch, even
    // though the plan and the manifest text are unchanged.
    fs::write(&pinned, b"tampered windows executable").unwrap();
    let bad = run_case(base, &catalog, "run pinned\n", None);
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
    let pinned = catalog.join("pinned.exe");
    place_fixture(&pinned);
    let digest = flow::capabilities::sha256_file(&pinned)
        .unwrap()
        .to_uppercase();
    write_manifest(
        &catalog,
        "pinned",
        &command_manifest(
            "pinned",
            pinned.to_str().unwrap(),
            write_args(&["write", "GOOD"]),
            Some(&digest),
        ),
    );
    let good = run_case(base, &catalog, "run pinned\n", None);
    assert!(
        good.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&good.stderr)
    );
    assert_eq!(good.stdout, b"GOOD");
}

#[test]
fn policy_preflight_checks_the_manifest_dir_executable_not_the_cwd_shadow() {
    // A cwd-planted `sub/tool.exe` must neither satisfy the policy preflight
    // nor run: the grant authorization and the manifest pin both bind the
    // manifest-directory file, which is also the file launch executes.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let shadow = base.join("shadow");
    let catalog = base.join("catalog");
    fs::create_dir(&shadow).unwrap();
    place_fixture(&catalog.join("sub").join("tool.exe"));
    place_broken(&shadow.join("sub").join("tool.exe"));
    let tool = catalog.join("sub").join("tool.exe");
    let digest = flow::capabilities::sha256_file(&tool).unwrap();
    let manifest = command_manifest(
        "tool",
        "sub/tool",
        write_args(&["write", "MANIFEST-DIR"]),
        Some(&digest),
    );
    write_manifest(&catalog, "tool", &manifest);
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
    assert_eq!(result.stdout, b"MANIFEST-DIR");
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
    place_fixture(&catalog.join("sub").join("tool.exe"));
    place_broken(&shadow.join("sub").join("tool.exe"));
    let shadow_digest =
        flow::capabilities::sha256_file(&shadow.join("sub").join("tool.exe")).unwrap();
    write_manifest(
        &catalog,
        "tool",
        &command_manifest(
            "tool",
            "sub/tool",
            write_args(&["write", "MANIFEST-DIR"]),
            Some(&shadow_digest),
        ),
    );
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
    let pinned = catalog.join("pinned.exe");
    place_fixture(&pinned);
    let digest = flow::capabilities::sha256_file(&pinned).unwrap();
    write_manifest(
        &catalog,
        "pinned",
        &command_manifest(
            "pinned",
            pinned.to_str().unwrap(),
            write_args(&["write", "GOOD"]),
            Some(&digest),
        ),
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
    assert_eq!(good.stdout, b"GOOD");
    // The manifest pin still names the executed file but the policy pin names
    // another file: preflight and launch disagree with the grant, so the
    // launch is refused.
    place_fixture(&catalog.join("other.exe"));
    let other_digest = flow::capabilities::sha256_file(&catalog.join("other.exe")).unwrap();
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

/// Symlink creation on Windows needs SeCreateSymbolicLink privilege (or
/// Developer Mode). When the runner cannot create one, the reparse test
/// skips: the lexical, missing-file, and policy tests above still pin the
/// behavior on every runner.
fn try_symlink_file(original: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_file(original, link) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipping symlink containment test: cannot create symlink: {error}");
            false
        }
    }
}

fn try_symlink_dir(original: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_dir(original, link) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipping symlink containment test: cannot create symlink: {error}");
            false
        }
    }
}

#[test]
fn final_symlink_cannot_escape_the_manifest_directory() {
    // `sub/link` is lexically contained, but the final component is a symlink
    // to an executable outside the catalog. Canonical confinement must refuse
    // it before any spawn, so the planted file never runs.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    let outside = base.join("outside");
    for dir in [&catalog, &workdir, &outside] {
        fs::create_dir(dir).unwrap();
    }
    fs::create_dir(catalog.join("sub")).unwrap();
    place_fixture(&outside.join("evil.exe"));
    if !try_symlink_file(
        &outside.join("evil.exe"),
        &catalog.join("sub").join("link.exe"),
    ) {
        return;
    }
    let marker = base.join("evil-ran");
    write_manifest(
        &catalog,
        "evil",
        &command_manifest(
            "evil",
            "sub/link",
            write_args(&["late-write", marker.to_str().unwrap()]),
            None,
        ),
    );
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
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    let outside = base.join("outside");
    for dir in [&catalog, &workdir, &outside] {
        fs::create_dir(dir).unwrap();
    }
    place_fixture(&outside.join("tool.exe"));
    if !try_symlink_dir(&outside, &catalog.join("sub")) {
        return;
    }
    let marker = base.join("evil-ran");
    write_manifest(
        &catalog,
        "evil",
        &command_manifest(
            "evil",
            "sub/tool",
            write_args(&["late-write", marker.to_str().unwrap()]),
            None,
        ),
    );
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
fn dangling_symlink_refuses_launch() {
    // The link itself is lexically inside the catalog, but its target does not
    // exist. Canonical confinement must fail closed before any execution.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    fs::create_dir(&catalog).unwrap();
    fs::create_dir(&workdir).unwrap();
    fs::create_dir(catalog.join("sub")).unwrap();
    if !try_symlink_file(
        &base.join("no-such-target.exe"),
        &catalog.join("sub").join("link.exe"),
    ) {
        return;
    }
    write_manifest(
        &catalog,
        "evil",
        &command_manifest("evil", "sub/link", write_args(&["write", "EVIL"]), None),
    );
    let result = run_case(&workdir, &catalog, "run evil\n", None);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("manifest directory"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn missing_catalog_binary_fails_closed() {
    // No symlink privilege is needed here: a catalog-relative binary with no
    // file behind it is the always-runnable dangling case and must refuse.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    fs::create_dir(&catalog).unwrap();
    fs::create_dir(&workdir).unwrap();
    fs::create_dir(catalog.join("sub")).unwrap();
    write_manifest(
        &catalog,
        "evil",
        &command_manifest("evil", "sub/tool", write_args(&["write", "EVIL"]), None),
    );
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
    fs::create_dir_all(catalog.join("sub").join("tool.exe")).unwrap();
    write_manifest(
        &catalog,
        "evil",
        &command_manifest("evil", "sub/tool", write_args(&["write", "EVIL"]), None),
    );
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
    // so a planted executable can never win and the lookup fails closed even
    // though an executable with that name sits in the runtime cwd.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    fs::create_dir(&catalog).unwrap();
    place_fixture(&base.join("probe.exe"));
    write_manifest(
        &catalog,
        "probe",
        &serde_json::json!({
            "name": "probe",
            "binary": "probe",
            "arguments": [{"literal": "write"}, {"literal": "SHADOW"}],
        }),
    );
    let result = run_case(base, &catalog, "run probe\n", Some("rel"));
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("not found in PATH"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn symlink_inside_the_manifest_directory_still_runs() {
    // Canonical confinement is not a blanket symlink ban: a link that
    // resolves to a file still inside the catalog keeps working.
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let catalog = base.join("catalog");
    let workdir = base.join("work");
    fs::create_dir(&catalog).unwrap();
    fs::create_dir(&workdir).unwrap();
    fs::create_dir(catalog.join("real")).unwrap();
    fs::create_dir(catalog.join("sub")).unwrap();
    place_fixture(&catalog.join("real").join("tool.exe"));
    if !try_symlink_file(
        &catalog.join("real").join("tool.exe"),
        &catalog.join("sub").join("link.exe"),
    ) {
        return;
    }
    write_manifest(
        &catalog,
        "tool",
        &command_manifest(
            "tool",
            "sub/link",
            write_args(&["write", "MANIFEST-DIR"]),
            None,
        ),
    );
    let result = run_case(&workdir, &catalog, "run tool\n", None);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"MANIFEST-DIR");
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
fn slash_relative_binary_without_manifest_origin_is_refused() {
    // Synthetic registries record no manifest origin, so a separator-bearing
    // binary would anchor at whatever the runtime cwd happens to be.
    // Resolution refuses instead of running a cwd-planted file.
    let temp = tempfile::tempdir().unwrap();
    let shadow = temp.path().join("shadow");
    fs::create_dir_all(shadow.join("sub")).unwrap();
    place_fixture(&shadow.join("sub").join("tool.exe"));
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
