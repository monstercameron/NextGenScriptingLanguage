use std::fs;
use std::process::Command;

#[test]
#[cfg(unix)]
fn checkpoint_refuses_an_explicitly_marked_runtime_output_before_creating_state() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "CANARY-CHECKPOINT-SECRET-315";
    fs::write(
        directory.path().join("secret.program"),
        format!(
            "run \"printf\" with arguments [\"%s\", \"{canary}\"] as token and keep secret\nuse value token\n"
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "secret.program", "--checkpoint", "ck"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("cannot be persisted"), "{error}");
    assert!(!error.contains(canary));
    assert!(!directory.path().join("ck").exists());
}

#[test]
#[cfg(unix)]
fn checkpoint_refuses_a_runtime_secret_forwarded_to_workflow_environment() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "run \"printf\" with arguments [\"%s\", \"CANARY-CHECKPOINT-ENV-315\"] as token and keep secret\nuse environment {\"TOKEN\": token}\nrun \"sh\" with arguments [\"-c\", \"touch checkpoint-effect\"]\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "secret.program", "--checkpoint", "checkpoint"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("cannot be persisted"), "{error}");
    assert!(!error.contains("CANARY-CHECKPOINT-ENV-315"));
    assert!(!directory.path().join("checkpoint").exists());
    assert!(!directory.path().join("checkpoint-effect").exists());
}

#[test]
#[cfg(unix)]
fn checkpoint_refuses_a_secret_reusable_program_before_inner_effects() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "CANARY-NESTED-CHECKPOINT-315";
    fs::write(directory.path().join("credential.txt"), canary).unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "define a program named reveal\n  run \"sh\" with arguments [\"-c\", \"touch nested-effect\"]\n  run \"cat\" with arguments [\"credential.txt\"]\nrun program reveal as token and keep secret\nuse value token\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "secret.program", "--checkpoint", "checkpoint"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("cannot be persisted"), "{error}");
    assert!(!error.contains(canary));
    assert!(!directory.path().join("checkpoint").exists());
    assert!(!directory.path().join("nested-effect").exists());
}

#[test]
fn check_rejects_secret_prompt_inputs_in_command_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "CANARY-PROMPT-ARG-315";
    fs::write(
        directory.path().join("secret.program"),
        "accept token as text from secret prompt \"Token?\"\nrun printf with message token\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "check",
            "secret.program",
            "--input",
            &format!("token={canary}"),
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("cannot be passed as a command argument"),
        "{error}"
    );
    assert!(!error.contains(canary));
}

#[test]
fn check_rejects_secret_pipeline_derived_bindings_in_command_arguments() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "accept token as text from secret prompt \"Token?\"\nuse value token\nthen \"cat\" as echoed\nrun printf with message echoed\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "check",
            "secret.program",
            "--input",
            "token=PIPELINE-CANARY-315",
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("secret binding \"echoed\""));
}

#[test]
fn check_rejects_bindings_derived_from_secret_environment_overlays() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "accept token as text from secret prompt \"Token?\"\nrun \"sh\" with arguments [\"-c\", \"printf '%s' \\\"$TOKEN\\\"\"] and environment {\"TOKEN\": token} as echoed\nrun printf with message echoed\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "secret.program"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("secret binding \"echoed\""));
}

#[test]
fn explain_withholds_literal_arguments_to_marked_secret_calls() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "CANARY-EXPLAIN-SECRET-315";
    fs::write(
        directory.path().join("secret.program"),
        format!("run printf with message \"{canary}\" as token and keep secret\nuse value token\n"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["explain", "secret.program"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(canary));
}

#[test]
fn cli_failure_diagnostic_redacts_a_secret_input() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "CANARY-FAIL-DIAGNOSTIC-315";
    fs::write(
        directory.path().join("secret.program"),
        "accept token as text from secret prompt \"Token?\"\nfail with message token\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            "secret.program",
            "--input",
            &format!("token={canary}"),
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(canary));
}

#[test]
#[cfg(unix)]
fn cli_failure_diagnostic_redacts_transformed_secret_stderr() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "FAIL-CANARY-TRANSFORMED-315";
    fs::write(
        directory.path().join("secret.program"),
        "accept token as text from secret prompt \"Token?\"\nrun \"sh\" with arguments [\"-c\", \"printf '%s' \\\"$TOKEN\\\" | head -c 12 >&2; exit 1\"] and environment {\"TOKEN\": token}\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            "secret.program",
            "--json-errors",
            "--input",
            &format!("token={canary}"),
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(!diagnostic.contains(&canary[..12]), "{diagnostic}");
    assert!(!diagnostic.contains(canary), "{diagnostic}");
    let object: serde_json::Value =
        serde_json::from_str(diagnostic.lines().last().unwrap()).unwrap();
    assert_eq!(object["code"], "command_failed", "{object}");
}

#[test]
#[cfg(unix)]
fn cli_redacted_runtime_error_preserves_signal_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "accept token as text from secret prompt \"Token?\"\nrun \"sh\" with arguments [\"-c\", \"kill -TERM $$\"] and environment {\"TOKEN\": token}\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            "secret.program",
            "--input",
            "token=SIGNAL-STATUS-315",
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(143));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SIGNAL-STATUS-315"));
}

#[test]
fn check_rejects_secret_environment_inputs_in_command_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "CANARY-ENV-ARG-315";
    fs::write(
        directory.path().join("secret.program"),
        "accept token as text from environment \"RUNTIME_TOKEN_315\"\nrun printf with message token\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "secret.program"])
        .current_dir(directory.path())
        .env("RUNTIME_TOKEN_315", canary)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("cannot be passed as a command argument"),
        "{error}"
    );
    assert!(!error.contains(canary));
}

#[test]
fn check_rejects_catch_records_from_secret_failure_flows_in_command_arguments() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "accept token as text from secret prompt \"Token?\"\ntry\n  use value token\n  then \"sh\" with arguments [\"-c\", \"head -c 12 >&2; exit 1\"]\ncatch caught\n  run printf with message caught\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "secret.program"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("secret binding \"caught\""));
}

#[test]
fn check_rejects_secret_task_group_aggregate_in_command_arguments() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "parallel at most 1 as results\n  run printf with message \"GROUP-SECRET\" as token and keep secret\nrun printf with message results\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "secret.program"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("secret binding \"results\""));
}

#[test]
fn secret_task_group_values_remain_available_to_explicit_stdin_flow() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "TASK-INTERNAL-SECRET-315";
    fs::write(
        directory.path().join("secret.program"),
        format!(
            "parallel at most 1 as results\n  run printf with message \"{canary}\" as token and keep secret\nthen encode results as json\nthen \"cat\"\n"
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "secret.program"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let bytes = value["tasks"][0]["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|byte| byte.as_u64().unwrap() as u8)
        .collect::<Vec<_>>();
    assert!(String::from_utf8(bytes).unwrap().contains(canary));
}

#[test]
fn check_rejects_secret_foreach_items_in_command_arguments() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "accept tokens as json from environment \"RUNTIME_TOKENS\"\nfor each item in tokens\n  run printf with message item\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "secret.program", "--input", "tokens=[\"PRIVATE\"]"])
        .current_dir(directory.path())
        .env("RUNTIME_TOKENS", "[\"PRIVATE\"]")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("secret binding \"item\""),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_rejects_secret_json_projections_in_command_arguments() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "run printf with message \"{\\\"token\\\":\\\"JSON-SECRET\\\"}\" as raw and keep secret\nuse value raw\nthen parse json as parsed\ntake text at \"/token\" from parsed as token\nrun printf with message token\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "secret.program"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("secret binding \"token\""),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_rejects_secret_input_forwarded_into_reusable_program_stdin() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "define a program named worker\n  read from stdin as message\n  run printf with message message\naccept token as text from secret prompt \"Token?\"\nuse value token\nthen program worker\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "secret.program", "--input", "token=PRIVATE"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("secret binding \"message\""));
}

#[test]
fn check_rejects_secret_inputs_forwarded_to_reusable_program_calls() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "define a program named worker\n  accept value as text\n  run printf with message value\naccept token as text from secret prompt \"Token?\"\nrun program worker with value token\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "secret.program", "--input", "token=private"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("cannot be passed as a command argument"),
        "{error}"
    );
    assert!(!error.contains("private"));
}
