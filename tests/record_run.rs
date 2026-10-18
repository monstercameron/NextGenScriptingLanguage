//! `execute run --record DIR` and `execute show-run`: a failed run can be diagnosed
//! from its record, the record is private and bounded, and declared secrets never
//! appear in it.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap()
}

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

/// The executable path as program text. Forward slashes need no escaping.
fn executable_text() -> String {
    env!("CARGO_BIN_EXE_execute").replace('\\', "/")
}

/// A program whose second step fails: this executable rejects an unknown option.
fn failing_program() -> String {
    format!(
        "run printf with message \"before\"\nrun \"{}\" with arguments [\"--no-such-option\"]\n",
        executable_text()
    )
}

/// Every file under `directory`, with its relative path.
fn files_under(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = fs::read(&path).unwrap();
                found.push((path.strip_prefix(directory).unwrap().to_path_buf(), bytes));
            }
        }
    }
    found
}

#[test]
fn a_failed_run_is_diagnosed_from_its_record_alone() {
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("failing.program"), failing_program()).unwrap();
    let record = work.path().join("record");

    let run = execute(
        work.path(),
        &[
            "run",
            "failing.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(!run.status.success(), "the program should fail");
    assert!(
        record.join("run.json").is_file(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(record.join("trace.jsonl").is_file());
    assert!(record.join("program.program").is_file());

    let document: Value =
        serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
    assert_eq!(document["protocol"], "execute.record/1");
    assert_eq!(document["result"]["ok"], false);
    assert_eq!(document["steps"], 2);
    assert_eq!(document["truncated"], false);

    let summary = execute(
        work.path(),
        &["show-run", record.to_str().unwrap(), "--json"],
    );
    assert!(
        summary.status.success(),
        "{}",
        String::from_utf8_lossy(&summary.stderr)
    );
    let report: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert_eq!(report["protocol"], "execute.show-run/1");
    assert_eq!(report["ok"], false);
    assert_eq!(report["steps"], 2);
    assert_eq!(report["failed_step"]["index"], 1);
    assert_eq!(report["failed_step"]["line"], 2);
    assert_eq!(report["failed_step"]["status"], 1);
    assert_eq!(report["failed_step"]["kind"], "failure");
    assert!(report["failed_step"]["certainty"].is_string());
    assert_eq!(report["slowest_steps"].as_array().unwrap().len(), 2);

    let text = execute(work.path(), &["show-run", record.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("failed step: #1 line 2"), "{text}");
}

#[test]
fn a_successful_run_records_each_step_output_by_hash() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"hello\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hello\n");

    let document: Value =
        serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
    assert_eq!(document["result"]["ok"], true);
    // Unconsumed output streams to the terminal as it is produced, so the final
    // value is empty. The bytes are kept with the step that produced them.
    assert_eq!(document["result"]["output"]["bytes"], 0);
    assert_eq!(
        document["result"]["output"]["sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );

    let trace = fs::read_to_string(record.join("trace.jsonl")).unwrap();
    let step: Value = serde_json::from_str(trace.lines().next().unwrap()).unwrap();
    assert_eq!(step["line"], 1);
    assert_eq!(step["status"], 0);
    assert_eq!(step["stdout"]["bytes"], 6);
    assert!(step["wall_us"].is_u64());
    let blob = step["stdout"]["blob"].as_str().unwrap();
    assert_eq!(fs::read(record.join(blob)).unwrap(), b"hello\n");
}

#[test]
fn sensitive_prompt_values_are_redacted_everywhere_in_the_record() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-SECRET-12345";
    fs::write(
        work.path().join("secret.program"),
        "accept token as text from secret prompt \"Access token?\"\nrun printf with message token\n",
    )
    .unwrap();
    let answers = work.path().join("answers.json");
    fs::write(&answers, serde_json::json!({"token": canary}).to_string()).unwrap();
    let record = work.path().join("record");
    let _run = execute(
        work.path(),
        &[
            "run",
            "secret.program",
            "--answers",
            answers.to_str().unwrap(),
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(record.join("trace.jsonl").is_file());
    for (_, bytes) in files_under(&record) {
        assert!(!String::from_utf8_lossy(&bytes).contains(canary));
    }
}

#[test]
fn sensitive_environment_values_are_redacted_everywhere_in_the_record() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-ENV-777";
    fs::write(
        work.path().join("environment.program"),
        "accept key as text from environment \"API_KEY\"\nrun printf with message key\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let _run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .arg("run")
        .arg("environment.program")
        .arg("--record")
        .arg(&record)
        .arg("--manifest-dir")
        .arg(manifests())
        .env("API_KEY", canary)
        .output()
        .unwrap();
    assert!(record.join("trace.jsonl").is_file());
    for (_, bytes) in files_under(&record) {
        assert!(!String::from_utf8_lossy(&bytes).contains(canary));
    }
}

#[test]
fn an_existing_non_empty_record_directory_is_not_reused() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"x\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    fs::create_dir(&record).unwrap();
    fs::write(record.join("keep.txt"), "mine").unwrap();
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("not empty"));
    assert_eq!(fs::read_to_string(record.join("keep.txt")).unwrap(), "mine");
}

#[cfg(unix)]
#[test]
fn record_files_are_private_to_the_owner() {
    use std::os::unix::fs::PermissionsExt;
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"x\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let directory_mode = fs::metadata(&record).unwrap().permissions().mode() & 0o777;
    assert_eq!(directory_mode, 0o700);
    for name in ["run.json", "trace.jsonl", "program.program"] {
        let mode = fs::metadata(record.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
}

#[test]
fn declared_secret_environment_values_never_appear_in_the_record() {
    let work = tempfile::tempdir().unwrap();
    let sentinel = "record-fixture-secret-must-not-appear";
    // Both shells expand the variable into the child's own stdout.
    let (binary, arguments) = if cfg!(windows) {
        (
            "cmd",
            serde_json::json!([{"literal": "/c"}, {"literal": "echo %RECORD_SENTINEL%"}]),
        )
    } else {
        (
            "/bin/sh",
            serde_json::json!([{"literal": "-c"}, {"literal": "printf '%s\\n' \"$RECORD_SENTINEL\""}]),
        )
    };
    let manifest = serde_json::json!({
        "name": "secret.echo",
        "binary": binary,
        "arguments": arguments,
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "environment": {"set": {"RECORD_SENTINEL": sentinel}}
        }
    });
    let manifest_dir = work.path().join("manifests");
    fs::create_dir(&manifest_dir).unwrap();
    fs::write(manifest_dir.join("secret.echo.json"), manifest.to_string()).unwrap();
    let policy = work.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"version":1,"secrets":{"environment":{"secret.echo":["RECORD_SENTINEL"]}}}"#,
    )
    .unwrap();
    // The policy loader accepts only an owner-private file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&policy, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(work.path().join("secret.program"), "run secret.echo\n").unwrap();
    let record = work.path().join("record");

    let run = execute(
        work.path(),
        &[
            "run",
            "secret.program",
            "--manifest-dir",
            manifest_dir.to_str().unwrap(),
            "--data-flow-policy",
            policy.to_str().unwrap(),
            "--record",
            record.to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    // The program still delivers the secret to its own output; only the record redacts it.
    assert!(String::from_utf8_lossy(&run.stdout).contains(sentinel));

    for (name, bytes) in files_under(&record) {
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(sentinel),
            "{} contains the secret",
            name.display()
        );
    }
    let summary = execute(work.path(), &["show-run", record.to_str().unwrap()]);
    assert!(!String::from_utf8_lossy(&summary.stdout).contains(sentinel));
    let document: Value =
        serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
    assert_eq!(
        document["redaction"]["declared_secret_names"][0],
        "RECORD_SENTINEL"
    );

    // The output of the secret-environment applet is withheld: no bytes, no hash, and
    // a marker naming the policy.
    let trace = fs::read_to_string(record.join("trace.jsonl")).unwrap();
    let secret_step: Value = trace
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|step| step["applets"][0] == "secret.echo")
        .expect("the secret applet step");
    assert!(secret_step["stdout"]["blob"].is_null(), "{secret_step}");
    assert_eq!(secret_step["stdout"]["withheld"], "policy");
    assert!(
        secret_step["stdout"]["sha256"].is_null(),
        "a withheld output has no hash"
    );
    assert_eq!(secret_step["stderr"]["tail"], "");
    assert_eq!(document["terminal"]["withheld"], "policy");
}
