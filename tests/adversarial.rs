#![cfg(unix)]
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output};

fn run(directory: &Path, catalog: &Path, source: &str) -> Output {
    let script = directory.join("case.program");
    fs::write(&script, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(script)
        .arg("--manifest-dir")
        .arg(catalog)
        .output()
        .unwrap()
}

#[test]
fn symlink_applet_identity_is_preserved_for_commands_and_listeners() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path();
    let worker = directory.join("multicall-worker");
    fs::write(&worker, "#!/bin/sh\nprintf '%s\\n' \"${0##*/}\"\n").unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&worker, directory.join("aliased")).unwrap();
    let manifest = directory.join("alias.json");
    fs::write(
        &manifest,
        r#"{"name":"aliased","binary":"./aliased","arguments":[]}"#,
    )
    .unwrap();
    for source in [
        "run aliased\n",
        "listen to aliased\nwhenever a line arrives as record\n  then save to \"record.txt\"\n",
    ] {
        let result = run(directory, &manifest, source);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"aliased\n");
    }
    assert_eq!(
        fs::read(directory.join("record.txt")).unwrap(),
        b"aliased\n"
    );
}

#[test]
fn literal_nul_path_is_rejected_before_an_earlier_save() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let result = run(
        temp.path(),
        &catalog,
        "run printf with message \"before\"\nthen save to \"must-not-exist\"\nread \"bad\\u0000path\"\n",
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("NUL"));
    assert!(!temp.path().join("must-not-exist").exists());
}

#[test]
fn impossible_reference_types_fail_before_an_earlier_save() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("manifests");
    fs::create_dir(&catalog).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests/printf.json"),
        catalog.join("printf.json"),
    )
    .unwrap();
    fs::write(
        catalog.join("number.json"),
        r#"{"name":"number","binary":"printf","parameters":{"value":{"type":"integer","required":true}},"arguments":[{"parameter":"value"}]}"#,
    )
    .unwrap();
    let result = run(
        temp.path(),
        &catalog,
        "run printf with message \"5\" as value\nthen save to \"must-not-exist\"\nthen number with value value\n",
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("integer"));
    assert!(!temp.path().join("must-not-exist").exists());
}

#[test]
fn cd_and_folder_directive_can_both_set_up_a_service() {
    let temp = tempfile::tempdir().unwrap();
    let nested = temp.path().join("sub/child");
    fs::create_dir_all(&nested).unwrap();
    let catalog = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let result = run(
        temp.path(),
        &catalog,
        "run cd with path \"sub\"\nuse folder \"child\"\nlisten to printf with message \"record\"\nwhenever a line arrives as record\n  run pwd\n",
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        format!("{}\n", fs::canonicalize(nested).unwrap().display()).as_bytes()
    );
}

#[test]
fn external_command_named_cd_cannot_masquerade_as_service_setup() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("cd.json");
    fs::write(&manifest, r#"{"name":"cd","binary":"pwd","arguments":[]}"#).unwrap();
    let script = temp.path().join("case.program");
    fs::write(&script, "run cd\nevery 1 second\n  run cd\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("check")
        .arg(script)
        .arg("--manifest-dir")
        .arg(manifest)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("cd builtin"));
}

#[test]
fn plain_tail_uses_unix_defaults_and_exits() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("lines.txt"), "one\ntwo\nthree\n").unwrap();
    let catalog = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    // Inspect argv before execution so a regression cannot launch endless -F.
    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let args = registry
        .resolve("tail")
        .unwrap()
        .build_arguments(&std::collections::BTreeMap::from([(
            "path".to_owned(),
            flow::ast::Value::String("lines.txt".to_owned()),
        )]))
        .unwrap();
    assert_eq!(args, ["--", "lines.txt"]);
    let result = run(temp.path(), &catalog, "run tail with path \"lines.txt\"\n");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"one\ntwo\nthree\n");
}

#[test]
fn generic_wc_cache_is_rejected_before_an_earlier_save() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("lines.txt"), "one\ntwo\n").unwrap();
    let catalog = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let result = run(temp.path(), &catalog,
        "run printf with message \"before\"\nthen save to \"must-not-exist\"\nrun wc with flags [\"lines.txt\"] and cache\n");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not declared cacheable"));
    assert!(!temp.path().join("must-not-exist").exists());
}

#[test]
fn command_and_parameter_names_can_overlap_modifier_keywords() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("assembler.json");
    // Use a harmless printf adapter to verify parsing and argv resolution for
    // the real Unix command name `as`, without assembling or changing files.
    fs::write(
        &manifest,
        r#"{
        "name":"as", "binary":"printf",
        "parameters":{"path":{"type":"string","required":true},"cache":{"type":"boolean"}},
        "arguments":[{"literal":"%s\\n"},{"parameter":"path"},
            {"when_present":"cache","append":[{"parameter":"cache"}]}]
    }"#,
    )
    .unwrap();
    let result = run(
        temp.path(),
        &manifest,
        "run as with path \"input.s\" and cache false as result\n",
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"input.s\nfalse\n");
}
