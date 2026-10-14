#![cfg(unix)]
//! Run the authored model workflows against checked-in offline fixtures.
use serde_json::Value;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn wait_for_command(command: &mut Command, timeout: Duration) -> Output {
    use std::os::unix::process::CommandExt;
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("model workflow validation needs Python 3");
    let pid = child.id() as libc::pid_t;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    match receiver.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("failed waiting for Python runner: {error}"),
        Err(_) => {
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_secs(2));
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            panic!("model workflow runner exceeded {timeout:?}");
        }
    }
}

fn assert_sha256(value: &Value, field: &str) {
    let digest = value[field]
        .as_str()
        .unwrap_or_else(|| panic!("report field {field} is not a string: {value:#}"));
    assert_eq!(digest.len(), 64, "{field} is not a SHA-256 digest");
    assert!(
        digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid {field}: {digest}"
    );
}

#[test]
fn all_one_hundred_model_workflows_pass_with_local_fixtures() {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let runner = project.join("examples/models/run.py");
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("model-workflows.json");
    let output = wait_for_command(
        Command::new("python3")
            .arg(runner)
            .args(["--binary", env!("CARGO_BIN_EXE_execute")])
            .args([
                "--manifest-dir",
                project.join("manifests").to_str().unwrap(),
            ])
            .args(["--report", report.to_str().unwrap()]),
        Duration::from_secs(300),
    );
    assert!(
        output.status.success(),
        "runner failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap_or_else(|error| {
            panic!(
                "invalid model workflow report: {error}\n{}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    assert_eq!(report["scope"], "full-100");
    assert_eq!(report["workflows_run"], 100);
    assert_eq!(report["workflows_passed"], 100);
    assert!(
        report["cases_run"].as_u64().unwrap() >= 200,
        "each workflow needs at least two cases: {report:#}"
    );
    assert_eq!(report["cases_passed"], report["cases_run"]);
    let network_isolation = report["network_isolation"].as_str().unwrap_or("");
    assert!(!network_isolation.is_empty());
    assert_sha256(&report, "binary_sha256");
    assert_sha256(&report, "model_helper_sha256");
    assert_sha256(&report, "catalog_sha256");
    let results = report["results"].as_array().unwrap();
    assert_eq!(results.len(), 100);
    for workflow in results {
        assert_eq!(workflow["passed"], true, "{workflow:#}");
        assert_sha256(workflow, "source_sha256");
        assert_sha256(workflow, "fixture_sha256");
        assert!(
            workflow["cases"].as_array().unwrap().len() >= 2,
            "{workflow:#}"
        );
    }
}

#[test]
fn model_catalog_validation_regressions_pass() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/model_catalog.py");
    let output = wait_for_command(Command::new("python3").arg(script), Duration::from_secs(30));
    assert!(
        output.status.success(),
        "model catalog tests failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_example(name: &str, directory: &std::path::Path, inputs: &[String]) -> Output {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .arg(project.join("examples/models").join(name))
        .arg("--manifest-dir")
        .arg(project.join("manifests"))
        .env("EXECUTE_MODELS_OFFLINE", "1")
        .current_dir(directory);
    for input in inputs {
        command.arg("--input").arg(input);
    }
    wait_for_command(&mut command, Duration::from_secs(15))
}

#[test]
fn model_examples_refuse_report_paths_that_alias_evidence() {
    for name in [
        "001-incident-timeline.program",
        "051-document-heading-order-review.program",
        "076-closed-local-knowledge-answer.program",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("evidence.json");
        let original = b"preserve original evidence\n";
        std::fs::write(&source, original).unwrap();
        let output = run_example(
            name,
            directory.path(),
            &[
                format!("source_path={}", source.display()),
                format!("report_path={}", source.display()),
            ],
        );
        assert!(!output.status.success(), "{name}");
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("test"),
            "{name}: {:?}",
            output.stderr
        );
    }
}

fn copy_knowledge_fixtures(directory: &std::path::Path) {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let destination = directory.join("fixtures/076");
    std::fs::create_dir_all(&destination).unwrap();
    for entry in std::fs::read_dir(project.join("examples/models/fixtures/076")).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file());
        std::fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
    }
}

#[test]
fn knowledge_example_rejects_sibling_reference_prefixes_before_inference() {
    let directory = tempfile::tempdir().unwrap();
    copy_knowledge_fixtures(directory.path());
    let source = directory.path().join("fixtures/076/source-a.json");
    let mut document: Value = serde_json::from_slice(&std::fs::read(&source).unwrap()).unwrap();
    document["knowledge_passages"][0]["ref"] = "faq/fieldsense-3.2.evil/FAQ-14#pairing".into();
    std::fs::write(source, serde_json::to_vec(&document).unwrap()).unwrap();
    std::fs::create_dir(directory.path().join("out")).unwrap();
    let output = run_example(
        "076-closed-local-knowledge-answer.program",
        directory.path(),
        &[],
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"decision=review\nreason=outside_declared_reference_roots\nsource_id=MWI-076-A\n"
    );
}

#[test]
fn knowledge_example_refuses_empty_generated_evidence_quotes() {
    let directory = tempfile::tempdir().unwrap();
    copy_knowledge_fixtures(directory.path());
    let fixture = directory.path().join("fixtures/076/draft-a.json");
    let mut document: Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    let text = &mut document["response"]["output"][0]["content"][0]["text"];
    let mut value: Value = serde_json::from_str(text.as_str().unwrap()).unwrap();
    value["evidence_quote"] = "".into();
    *text = serde_json::to_string(&value).unwrap().into();
    std::fs::write(fixture, serde_json::to_vec(&document).unwrap()).unwrap();
    let output = run_example(
        "076-closed-local-knowledge-answer.program",
        directory.path(),
        &[],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("quote and reference must match"));
    assert!(!directory
        .path()
        .join("out/076-closed-local-knowledge-answer.md")
        .exists());
}
