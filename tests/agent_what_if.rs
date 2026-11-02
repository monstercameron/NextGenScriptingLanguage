//! `execute run FILE --what-if` and `--confirm FINGERPRINT`: the run happens only
//! when the plan still matches the one that was reviewed.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[path = "common/staged_helpers.rs"]
mod staged_helpers;

/// The cargo-built model helper, staged as a private single-link copy: the
/// trusted-executable gate refuses the hardlinked `target/` original.
fn helper() -> &'static std::path::Path {
    staged_helpers::execute_model()
}
const SOURCE: &str = "run plain.helper\nthen save to \"out.txt\"\n";
const LOADER_OVERRIDES: [&str; 8] = [
    "LD_PRELOAD",
    "LD_AUDIT",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_IMAGE_SUFFIX",
];

fn clear_loader_overrides(command: &mut Command) {
    for name in LOADER_OVERRIDES {
        command.env_remove(name);
    }
}

fn write_manifests(directory: &Path) -> PathBuf {
    let manifests = directory.join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    let plain = serde_json::json!({
        "name": "plain.helper", "binary": helper(), "accepts_stdin": false,
        "parameters": {}, "arguments": [{ "literal": "--version" }]
    });
    std::fs::write(manifests.join("plain.helper.json"), plain.to_string()).unwrap();
    manifests
}

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    let manifests = write_manifests(directory);
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .args(arguments)
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if arguments
        .iter()
        .any(|argument| matches!(*argument, "--what-if" | "--confirm"))
    {
        #[cfg(unix)]
        command.env("XDG_STATE_HOME", directory.join("what-if-state"));
        #[cfg(windows)]
        command.env("LOCALAPPDATA", directory.join("what-if-local-app-data"));
    }
    clear_loader_overrides(&mut command);
    command.output().expect("start execute")
}

fn what_if(directory: &Path) -> Value {
    let output = execute(directory, &["tool.program", "--what-if"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON object")
}

fn fingerprint(report: &Value) -> String {
    report["fingerprint"].as_str().unwrap().to_owned()
}

#[test]
fn what_if_prints_the_plan_and_a_fingerprint_and_runs_nothing() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let report = what_if(directory.path());
    assert_eq!(report["protocol"], "execute.what_if/2");
    assert_eq!(report["plan"]["protocol"], "execute.explain/2");
    let fp = fingerprint(&report);
    assert_eq!(fp.len(), 64, "a SHA-256 hex digest");
    assert!(fp.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(
        !directory.path().join("out.txt").exists(),
        "what-if must not run the program"
    );
}

#[test]
fn the_fingerprint_is_stable_for_the_same_plan() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    assert_eq!(
        fingerprint(&what_if(directory.path())),
        fingerprint(&what_if(directory.path()))
    );
}

#[test]
fn confirming_the_reviewed_fingerprint_runs_the_program() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let fp = fingerprint(&what_if(directory.path()));
    let output = execute(directory.path(), &["tool.program", "--confirm", &fp]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        directory.path().join("out.txt").exists(),
        "a confirmed run must execute"
    );
}

#[test]
fn a_process_free_review_does_not_create_the_private_path_key() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("tool.program"),
        "use value \"ready\"\n",
    )
    .unwrap();
    let report = what_if(directory.path());
    assert!(report["settings"]["executables"]
        .as_object()
        .unwrap()
        .is_empty());
    assert!(!directory
        .path()
        .join("what-if-state/execute/what-if.key")
        .exists());
}

#[cfg(unix)]
#[test]
fn confirmation_fingerprints_the_executable_resolved_from_the_environment_path() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("fakebin");
    std::fs::create_dir(&bin).unwrap();
    let tool = bin.join("reviewed-tool");
    std::fs::write(&tool, "#!/bin/sh\nprintf 'first-version\\n'\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = serde_json::to_string(bin.to_str().unwrap()).unwrap();
    let source =
        format!("run \"reviewed-tool\" with arguments [] and environment {{\"PATH\": {path}}}\n");
    std::fs::write(directory.path().join("tool.program"), source).unwrap();

    let reviewed = what_if(directory.path());
    let command = reviewed["plan"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binary"] == "reviewed-tool")
        .expect("the direct executable appears in the plan");
    assert!(command["resolved_path"]
        .as_str()
        .unwrap()
        .ends_with("fakebin/reviewed-tool"));
    let reviewed_fingerprint = fingerprint(&reviewed);

    std::fs::write(&tool, "#!/bin/sh\nprintf 'second-version\\n'\n").unwrap();
    let output = execute(
        directory.path(),
        &["tool.program", "--confirm", &reviewed_fingerprint],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("plan changed since --what-if"));
}

#[cfg(unix)]
#[test]
fn what_if_and_confirm_accept_a_safely_inferred_computed_path() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("computed-bin");
    let tail_a = directory.path().join("computed-tail-a");
    let tail_b = directory.path().join("computed-tail-b");
    for path in [&bin, &tail_a, &tail_b] {
        std::fs::create_dir(path).unwrap();
    }
    let marker = directory.path().join("computed-path-ran");
    let tool = bin.join("reviewed-tool");
    std::fs::write(&tool, "#!/bin/sh\nsecondary-tool\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (tail, output) in [(&tail_a, "tail-a"), (&tail_b, "tail-b")] {
        let secondary = tail.join("secondary-tool");
        std::fs::write(
            &secondary,
            format!("#!/bin/sh\nprintf '{output}' > '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&secondary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let source_for = |tail: &Path| {
        let path = format!("{}:{}", bin.display(), tail.display());
        let path = serde_json::to_string(&path).unwrap();
        format!(
            "run \"printf\" with arguments [\"%s\", {path}] as computed_path\nuse environment {{\"PATH\": computed_path}}\nrun \"reviewed-tool\" with arguments []\n"
        )
    };
    let workflow = directory.path().join("tool.program");
    std::fs::write(&workflow, source_for(&tail_a)).unwrap();

    let reviewed = what_if(directory.path());
    let row = reviewed["plan"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binary"] == "reviewed-tool")
        .expect("the direct executable appears in the plan");
    assert_eq!(row["environment_tainted"], true);
    assert_eq!(row["environment_path_only"], true);
    let canonical_tool = tool.canonicalize().unwrap().to_string_lossy().into_owned();
    assert_eq!(row["resolved_path"].as_str(), Some(canonical_tool.as_str()));
    let executable = reviewed["settings"]["executables"]
        .as_object()
        .unwrap()
        .values()
        .find(|executable| executable["path"].as_str() == Some(canonical_tool.as_str()))
        .expect("the inferred PATH executable identity is covered");
    let reviewed_path_hmac = executable["effective_path_hmac_sha256"].clone();
    assert_eq!(reviewed_path_hmac.as_str().unwrap().len(), 64);
    assert!(
        !marker.exists(),
        "what-if must not run the planned executable"
    );

    std::fs::write(&workflow, source_for(&tail_b)).unwrap();
    let changed = what_if(directory.path());
    let changed_row = changed["plan"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binary"] == "reviewed-tool")
        .unwrap();
    let changed_executable = changed["settings"]["executables"]
        .as_object()
        .unwrap()
        .values()
        .find(|executable| executable["path"].as_str() == Some(canonical_tool.as_str()))
        .unwrap();
    assert_eq!(
        changed_row["resolved_path"].as_str(),
        Some(canonical_tool.as_str())
    );
    assert_ne!(
        changed_executable["effective_path_hmac_sha256"],
        reviewed_path_hmac
    );
    assert_ne!(fingerprint(&changed), fingerprint(&reviewed));

    let output = execute(
        directory.path(),
        &["tool.program", "--confirm", &fingerprint(&reviewed)],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("plan changed since --what-if"));
    assert!(!marker.exists(), "stale review must not launch the tool");

    let output = execute(
        directory.path(),
        &["tool.program", "--confirm", &fingerprint(&changed)],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        marker.exists(),
        "the reviewed executable should run on confirmation"
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "tail-b");
}

#[cfg(unix)]
#[test]
fn what_if_still_refuses_a_computed_path_with_a_sensitive_tail() {
    let directory = tempfile::tempdir().unwrap();
    let secret_bin = directory.path().join("secret-bin");
    std::fs::create_dir(&secret_bin).unwrap();
    std::fs::write(secret_bin.join("secret-only-tool"), "not executed").unwrap();
    let inherited_path = format!("/usr/bin:/bin:{}", secret_bin.display());
    let source = "accept oldpath as text from environment \"PATH\"\nrun \"printf\" with arguments [\"%s\", oldpath] as computed_path\nuse environment {\"PATH\": computed_path}\nrun \"secret-only-tool\" with arguments []\n";
    std::fs::write(directory.path().join("tool.program"), source).unwrap();
    let manifests = write_manifests(directory.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .args(["tool.program", "--what-if"])
        .arg("--manifest-dir")
        .arg(manifests)
        .current_dir(directory.path())
        .env("PATH", &inherited_path)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.env("XDG_STATE_HOME", directory.path().join("what-if-state"));
    clear_loader_overrides(&mut command);
    let output = command.output().expect("start execute");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("after a run-time environment change"),
        "{stderr}"
    );
    assert!(!stderr.contains(secret_bin.to_str().unwrap()));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret_bin.to_str().unwrap()));
}

#[cfg(unix)]
#[test]
fn same_primary_executable_with_a_changed_path_tail_invalidates_confirmation() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first-bin");
    let tail_a = directory.path().join("tail-a");
    let tail_b = directory.path().join("tail-b");
    for path in [&first, &tail_a, &tail_b] {
        std::fs::create_dir(path).unwrap();
    }
    let marker = directory.path().join("secondary-ran");
    let primary = first.join("reviewed-tool");
    std::fs::write(&primary, "#!/bin/sh\nsecondary-tool\n").unwrap();
    std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (directory, output) in [(&tail_a, "tail-a"), (&tail_b, "tail-b")] {
        let secondary = directory.join("secondary-tool");
        std::fs::write(
            &secondary,
            format!("#!/bin/sh\nprintf '{output}' > '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&secondary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let source_for = |tail: &Path| {
        let path = format!("{}:{}", first.to_string_lossy(), tail.to_string_lossy());
        let path = serde_json::to_string(&path).unwrap();
        format!("run \"reviewed-tool\" with arguments [] and environment {{\"PATH\": {path}}}\n")
    };
    let workflow = directory.path().join("tool.program");
    std::fs::write(&workflow, source_for(&tail_a)).unwrap();
    let reviewed = what_if(directory.path());
    let reviewed_row = reviewed["plan"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binary"] == "reviewed-tool")
        .unwrap();
    let reviewed_path = reviewed_row["resolved_path"].clone();
    let reviewed_executable = reviewed["settings"]["executables"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    let reviewed_digest = reviewed_executable["sha256"].clone();
    let reviewed_path_hmac = reviewed_executable["effective_path_hmac_sha256"].clone();
    let reviewed_fingerprint = fingerprint(&reviewed);
    let primary_path = primary
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(reviewed_path.as_str(), Some(primary_path.as_str()));
    assert_eq!(reviewed_path_hmac.as_str().unwrap().len(), 64);
    assert!(!reviewed.to_string().contains(tail_a.to_str().unwrap()));

    std::fs::write(&workflow, source_for(&tail_b)).unwrap();
    let changed = what_if(directory.path());
    let changed_row = changed["plan"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binary"] == "reviewed-tool")
        .unwrap();
    let changed_executable = changed["settings"]["executables"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(changed_row["resolved_path"], reviewed_path);
    assert_eq!(changed_executable["sha256"], reviewed_digest);
    assert_ne!(
        changed_executable["effective_path_hmac_sha256"],
        reviewed_path_hmac
    );
    assert_ne!(fingerprint(&changed), reviewed_fingerprint);

    let confirmed = execute(
        directory.path(),
        &["tool.program", "--confirm", &reviewed_fingerprint],
    );
    assert!(!confirmed.status.success());
    assert!(String::from_utf8_lossy(&confirmed.stderr).contains("plan changed since --what-if"));
    assert!(
        !marker.exists(),
        "stale confirmation must not launch the tool"
    );
}

#[cfg(unix)]
#[test]
fn changed_sensitive_environment_value_cannot_be_confirmed_with_a_resolved_path() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("fakebin");
    std::fs::create_dir(&bin).unwrap();
    let marker = directory.path().join("launched");
    let tool = bin.join("reviewed-tool");
    std::fs::write(
        &tool,
        format!("#!/bin/sh\nprintf launched > '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = serde_json::to_string(bin.to_str().unwrap()).unwrap();
    let write_source = |value: &str| {
        let value = serde_json::to_string(value).unwrap();
        let source = format!(
            "run \"reviewed-tool\" with arguments [] and environment {{\"PATH\": {path}, \"REVIEW_SWITCH\": {value}}}\n"
        );
        std::fs::write(directory.path().join("tool.program"), source).unwrap();
    };

    write_source("before");
    let reviewed = execute(directory.path(), &["tool.program", "--what-if"]);
    assert!(!reviewed.status.success());
    assert!(
        String::from_utf8_lossy(&reviewed.stderr).contains("after a run-time environment change")
    );

    write_source("after");
    let stale_fingerprint = "a".repeat(64);
    let confirmed = execute(
        directory.path(),
        &["tool.program", "--confirm", &stale_fingerprint],
    );
    assert!(!confirmed.status.success());
    assert!(
        String::from_utf8_lossy(&confirmed.stderr).contains("after a run-time environment change")
    );
    assert!(
        !marker.exists(),
        "confirmation must not launch the executable"
    );
}

#[test]
fn a_stale_fingerprint_is_refused_before_any_effect() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let stale = "0".repeat(64);
    let output = execute(directory.path(), &["tool.program", "--confirm", &stale]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("plan changed"));
    assert!(
        !directory.path().join("out.txt").exists(),
        "a refused run must not execute"
    );
}

#[test]
fn editing_the_program_after_review_invalidates_the_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let fp = fingerprint(&what_if(directory.path()));
    std::fs::write(
        directory.path().join("tool.program"),
        "run plain.helper\nthen save to \"other.txt\"\n",
    )
    .unwrap();
    let output = execute(directory.path(), &["tool.program", "--confirm", &fp]);
    assert!(
        !output.status.success(),
        "an edited program must not run on the old confirmation"
    );
    assert!(!directory.path().join("other.txt").exists());
    assert!(!directory.path().join("out.txt").exists());
}

#[test]
fn what_if_and_confirm_are_accepted_only_for_run() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    for arguments in [
        vec!["explain", "tool.program", "--what-if"],
        vec!["check", "tool.program", "--confirm", "abc"],
    ] {
        let output = execute(directory.path(), &arguments);
        assert!(!output.status.success(), "{arguments:?} must be rejected");
        assert!(output.stdout.is_empty(), "{arguments:?} printed output");
    }
}

#[test]
fn direct_arguments_are_reviewed_and_change_fingerprint() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tool.program");
    std::fs::write(&path, "run \"rm\" with arguments [\"-f\", \"a.txt\"]\n").unwrap();
    let first = what_if(directory.path());
    std::fs::write(&path, "run \"rm\" with arguments [\"-rf\", \"/home\"]\n").unwrap();
    let second = what_if(directory.path());
    assert_eq!(
        first["plan"]["steps"][0]["argv"],
        serde_json::json!(["-f", "a.txt"])
    );
    assert_eq!(
        second["plan"]["steps"][0]["argv"],
        serde_json::json!(["-rf", "/home"])
    );
    assert_ne!(fingerprint(&first), fingerprint(&second));
}

#[test]
fn secret_prompt_input_remains_in_review_settings_without_argv_expansion() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("tool.program"),
        "accept token as text from secret prompt \"Token?\"\nuse value token\n",
    )
    .unwrap();
    let output = execute(
        directory.path(),
        &[
            "tool.program",
            "--what-if",
            "--input",
            "token=private-canary-462",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-canary-462"));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["plan"]["steps"][0]["kind"], "use_value");
    assert!(report["plan"]["steps"][0]["argv"].is_null());
}

#[test]
fn review_versions_have_distinct_fingerprints_and_confirmation_requires_same_version() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let v2 = what_if(directory.path());
    let output = execute(
        directory.path(),
        &["tool.program", "--what-if", "--explain-version", "1"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v1: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v1["protocol"], "execute.what_if/1");
    assert_eq!(v1["plan"]["protocol"], "execute.explain/1");
    assert_ne!(fingerprint(&v1), fingerprint(&v2));
    let fp = fingerprint(&v1);
    let wrong_version = execute(directory.path(), &["tool.program", "--confirm", &fp]);
    assert!(!wrong_version.status.success());
    let same_version = execute(
        directory.path(),
        &["tool.program", "--confirm", &fp, "--explain-version", "1"],
    );
    assert!(
        same_version.status.success(),
        "{}",
        String::from_utf8_lossy(&same_version.stderr)
    );
}
