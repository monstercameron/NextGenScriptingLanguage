#[cfg(feature = "module-tools")]
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn execute() -> Command {
    Command::new(env!("CARGO_BIN_EXE_execute"))
}

fn output(work: &Path, args: &[&str]) -> Output {
    execute().current_dir(work).args(args).output().unwrap()
}

#[cfg(any(windows, feature = "tooling", feature = "module-tools"))]
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(feature = "module-tools")]
fn write_module_fixture(root: &Path) {
    let package = root.join("package");
    fs::create_dir_all(package.join("src")).unwrap();
    let source = include_str!("../examples/modules/records.program");
    fs::write(package.join("src/main.program"), source).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../examples/modules/package/program.modules.json"
    ))
    .unwrap();
    manifest["source"] = serde_json::json!("src/main.program");
    manifest["source_sha256"] =
        serde_json::json!(format!("{:x}", Sha256::digest(source.as_bytes())));
    fs::write(
        package.join("program.modules.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn check_json_marks_static_errors_not_ok_and_fails() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("bad.program"),
        "every 0 seconds\n  run \"printf\"\n",
    )
    .unwrap();
    let result = output(work.path(), &["check", "bad.program", "--json"]);
    let document: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(document["ok"], false);
    assert_eq!(document["diagnostics"][0]["severity"], "error");
    assert_eq!(result.status.code(), Some(1));
}

#[cfg(unix)]
#[cfg(feature = "tooling")]
#[test]
fn service_generate_forwards_private_answers_file_by_path() {
    use std::os::unix::fs::PermissionsExt;
    let work = tempfile::tempdir().unwrap();
    let program = work.path().join("service.program");
    fs::write(
        &program,
        "accept token as text from secret prompt \"Access token?\"\nevery 5 minutes\n  use value token\n",
    )
    .unwrap();
    let answers = work.path().join("answers.json");
    fs::write(&answers, r#"{"token":"CANARY-SVC-1"}"#).unwrap();
    fs::set_permissions(&answers, fs::Permissions::from_mode(0o600)).unwrap();
    let result = output(
        work.path(),
        &[
            "service",
            "generate",
            "systemd",
            "service.program",
            "--name",
            "com.test.service",
            "--answers",
            "answers.json",
        ],
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    let rendered = text(&result.stdout);
    assert!(rendered.contains("--answers"));
    assert!(rendered.contains("answers.json"));
    assert!(!rendered.contains("CANARY-SVC-1"));
}

#[cfg(windows)]
#[test]
fn service_generate_accepts_answers_option_and_rejects_nonprivate_acl() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("answers.json"),
        r#"{"token":"CANARY-SVC-1"}"#,
    )
    .unwrap();
    let result = output(
        work.path(),
        &[
            "service",
            "generate",
            "systemd",
            "service.program",
            "--answers",
            "answers.json",
        ],
    );
    assert!(!result.status.success());
    let error = text(&result.stderr);
    assert!(
        error.contains("private Windows DACL")
            || error.contains("another Windows principal")
            || error.contains("inherited Windows ACL"),
        "{error}"
    );
    assert!(!error.contains("unsupported service option --answers"));
}

#[cfg(feature = "module-tools")]
#[test]
fn modules_inspect_accepts_bare_manifest_filename() {
    let work = tempfile::tempdir().unwrap();
    write_module_fixture(work.path());
    let package = work.path().join("package");
    let result = output(&package, &["modules", "inspect", "program.modules.json"]);
    assert!(result.status.success(), "{}", text(&result.stderr));
}

#[cfg(feature = "module-tools")]
#[test]
fn module_lock_manifest_paths_use_forward_slashes_and_read_backslashes() {
    let work = tempfile::tempdir().unwrap();
    write_module_fixture(work.path());
    let lock = work.path().join("package/lock.json");
    let result = output(
        work.path(),
        &[
            "modules",
            "lock",
            "package/program.modules.json",
            "--root",
            ".",
            "--output",
            lock.to_str().unwrap(),
        ],
    );
    assert!(result.status.success(), "{}", text(&result.stderr));
    let raw = fs::read_to_string(&lock).unwrap();
    assert!(raw.contains("package/program.modules.json"));
    assert!(!raw.contains("package\\\\program.modules.json"));
    let mut lock_json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let manifest = lock_json["packages"]["examples.records"]["manifest"]
        .as_str()
        .unwrap()
        .replace('/', "\\");
    lock_json["packages"]["examples.records"]["manifest"] = serde_json::json!(manifest);
    fs::write(&lock, serde_json::to_vec_pretty(&lock_json).unwrap()).unwrap();
    let verified = output(
        work.path(),
        &[
            "modules",
            "verify",
            "package/program.modules.json",
            "--root",
            ".",
            "--lock",
            lock.to_str().unwrap(),
        ],
    );
    assert!(verified.status.success(), "{}", text(&verified.stderr));
}
