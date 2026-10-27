#![cfg(feature = "model")]

//! Model-helper finite options are checked by the applet manifests before the
//! offline helper can be started.
use serde_json::json;
use std::{fs, path::Path, process::Output};

const EXECUTE: &str = env!("CARGO_BIN_EXE_execute");
const APIS: [&str; 2] = ["responses", "chat_completions"];
const EFFORTS: [&str; 6] = ["none", "low", "medium", "high", "xhigh", "max"];
const CREDENTIALS: [&str; 5] = [
    "OPENAI_API_KEY",
    "openai",
    "JEV_API_KEY",
    "TYPESAFE_API_KEY",
    "jev",
];
const PROXIES: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

fn source(operation: &str, api: &str, effort: &str) -> String {
    let mut call = format!(
        "{operation} with model {} and instruction {}",
        json!("gpt-6-luna"),
        json!("draft")
    );
    if operation == "llm.extract" {
        let schema = json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        });
        call.push_str(&format!(" and schema {}", json!(schema.to_string())));
    }
    call.push_str(&format!(
        " and api {} and effort {} as raw\n",
        json!(api),
        json!(effort)
    ));
    call
}

fn execute(dir: &Path, subcommand: &str, source: &str) -> Output {
    fs::write(dir.join("model.program"), source).unwrap();
    let mut command = std::process::Command::new(EXECUTE);
    command
        .arg(subcommand)
        .arg("model.program")
        .current_dir(dir)
        .env("EXECUTE_MODELS_OFFLINE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for name in CREDENTIALS.iter().chain(PROXIES.iter()) {
        command.env_remove(name);
    }
    command.output().expect("start execute")
}

#[test]
fn static_check_accepts_every_runtime_supported_api_and_effort() {
    let dir = tempfile::tempdir().unwrap();
    for operation in ["llm.ask", "llm.extract"] {
        for api in APIS {
            for effort in EFFORTS {
                let result = execute(dir.path(), "check", &source(operation, api, effort));
                assert!(
                    result.status.success(),
                    "{operation} rejected api={api:?}, effort={effort:?}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
        }
    }
}

#[test]
fn static_and_runtime_validation_reject_unsupported_model_options_offline() {
    let dir = tempfile::tempdir().unwrap();
    for operation in ["llm.ask", "llm.extract"] {
        for (api, effort, parameter) in
            [("bogus", "low", "api"), ("responses", "minimal", "effort")]
        {
            let program = source(operation, api, effort);
            let check = execute(dir.path(), "check", &program);
            assert!(
                !check.status.success(),
                "static check accepted {operation} {parameter}"
            );
            let check_stderr = String::from_utf8_lossy(&check.stderr);
            assert!(
                check_stderr.contains("must be one of"),
                "static check did not report the allowed values: {check_stderr}"
            );

            let run = execute(dir.path(), "run", &program);
            assert!(
                !run.status.success(),
                "runtime accepted {operation} {parameter}"
            );
            let run_stderr = String::from_utf8_lossy(&run.stderr);
            assert!(
                run_stderr.contains("must be one of"),
                "runtime did not reject the value at the manifest boundary: {run_stderr}"
            );
            assert!(
                !run_stderr.contains("execute-model:"),
                "invalid option reached the helper: {run_stderr}"
            );
        }

        // The highest supported effort and the alternate API must pass the
        // manifest boundary; offline mode then stops the helper before any I/O.
        let valid = execute(
            dir.path(),
            "run",
            &source(operation, "chat_completions", "max"),
        );
        assert!(!valid.status.success());
        let stderr = String::from_utf8_lossy(&valid.stderr);
        assert!(
            stderr.contains("execute-model: offline"),
            "valid options did not reach the offline helper for {operation}: {stderr}"
        );
    }
}
