use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn json_repl_returns_one_response_per_request_and_reports_block_spans() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let manifest = serde_json::json!({
        "name":"ok.helper", "binary":env!("CARGO_BIN_EXE_execute-model"),
        "accepts_stdin":false, "parameters":{}, "arguments":[{"literal":"--version"}],
        "contract":{"version":1,"output":{"type":"text","encoding":"raw"}}
    });
    std::fs::write(manifests.join("ok.helper.json"), manifest.to_string()).unwrap();
    std::fs::write(directory.path().join("notes.txt"), "from-read").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--manifest-dir"])
        .arg(manifests)
        .current_dir(directory.path())
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{{\"submit\":\"run ok.helper as value and keep status as exit_code\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"run missing.helper\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"use value \\\"from-use-value\\\"\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"read file \\\"notes.txt\\\"\"}}").unwrap();
    writeln!(input, "{{\"source\":true}}").unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let responses = String::from_utf8(output.stdout).unwrap().lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()).collect::<Vec<_>>();
    assert_eq!(responses.len(), 5);
    assert_eq!(responses[0]["status"], "ran", "{responses:?}");
    assert_eq!(responses[0]["bindings"]["value"]["type"], "string");
    assert_eq!(responses[0]["bindings"]["exit_code"]["type"], "integer");
    assert_eq!(responses[1]["status"], "rejected");
    assert!(responses[1]["diagnostics"][0]["span"]["block"].is_object());
    assert_eq!(responses[2]["stdout"], "from-use-value");
    assert_eq!(responses[3]["stdout"], "from-read");
    assert_eq!(responses[4]["source"], "run ok.helper as value and keep status as exit_code\nuse value \"from-use-value\"\nread file \"notes.txt\"\n");
}

#[test]
fn what_if_returns_a_plan_without_running_the_block() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let (binary, arguments) = if cfg!(windows) {
        ("cmd", vec!["/c".to_owned(), "type nul > marker.txt".to_owned()])
    } else {
        ("/bin/sh", vec!["-c".to_owned(), "touch marker.txt".to_owned()])
    };
    let manifest = serde_json::json!({
        "name":"side.effect", "binary":binary,
        "accepts_stdin":false, "parameters":{}, "arguments":arguments.iter().map(|arg| serde_json::json!({"literal":arg})).collect::<Vec<_>>()
    });
    std::fs::write(manifests.join("side.effect.json"), manifest.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--what-if", "--manifest-dir"])
        .arg(manifests).current_dir(directory.path())
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map(|mut child| {
            child.stdin.take().unwrap().write_all(b"{\"submit\":\"run side.effect\"}\n{\"source\":true}\n").unwrap();
            child.wait_with_output().unwrap()
        }).unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let responses = parse_lines(&output.stdout);
    let response = &responses[0];
    assert_eq!(response["status"], "planned");
    assert_eq!(response["plan"]["protocol"], "execute.explain/2");
    assert_eq!(responses[1]["source"], "run side.effect\n");
    assert!(!directory.path().join("marker.txt").exists());
}

#[test]
fn five_step_session_repairs_rejection_saves_checks_and_matches_output() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    write_version_manifest(&manifests);
    let saved = directory.path().join("saved.program");
    let mut child = repl_child(directory.path(), &manifests, false);
    let mut input = child.stdin.take().unwrap();
    // The rejection is intentionally partway through the session. The following
    // valid block repairs it; five valid statements end up in the saved source.
    let requests = [
        r#"{"submit":"run ok.helper as first"}"#,
        r#"{"submit":"run ok.helper as true"}"#,
        r#"{"submit":"run ok.helper as second"}"#,
        r#"{"submit":"run ok.helper as third"}"#,
        r#"{"submit":"run ok.helper as fourth"}"#,
        r#"{"submit":"run ok.helper as fifth"}"#,
        r#"{"save":"saved.program"}"#,
    ];
    for request in requests { writeln!(input, "{request}").unwrap(); }
    drop(input);
    let repl_output = child.wait_with_output().unwrap();
    assert!(repl_output.status.success(), "{}", String::from_utf8_lossy(&repl_output.stderr));
    let responses = parse_lines(&repl_output.stdout);
    assert_eq!(responses.len(), 7);
    assert_eq!(responses[0]["status"], "ran", "{responses:?}");
    assert_eq!(responses[1]["status"], "rejected");
    assert!(responses[1]["diagnostics"][0]["span"]["block"].is_object());
    assert_eq!(responses[2]["status"], "ran");
    assert_eq!(responses[5]["status"], "ran");
    assert_eq!(responses[6]["status"], "saved");

    let source = std::fs::read_to_string(&saved).unwrap();
    assert_eq!(source.lines().count(), 5);
    let check = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", saved.to_str().unwrap(), "--manifest-dir"])
        .arg(&manifests).current_dir(directory.path())
        .output().unwrap();
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([saved.to_str().unwrap(), "--manifest-dir"]).arg(&manifests)
        .current_dir(directory.path()).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    let session_output = responses.iter().filter_map(|response| response["stdout"].as_str())
        .collect::<String>();
    assert_eq!(String::from_utf8(run.stdout).unwrap(), session_output);
}

#[test]
fn all_json_commands_work_and_declared_secrets_are_redacted() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let secret = "repl-protocol-secret-value-that-is-well-over-sixty-characters-and-would-leak-with-a-prefix";
    let (binary, arguments) = if cfg!(windows) {
        ("cmd", serde_json::json!([{"literal":"/c"},{"literal":"echo %REPL_SECRET%"}]))
    } else {
        ("/bin/sh", serde_json::json!([{"literal":"-c"},{"literal":r#"printf %s "$REPL_SECRET""#}]))
    };
    let manifest = serde_json::json!({
        "name":"secret.echo", "binary":binary, "arguments":arguments,
        "contract":{"version":1,"backend":"command","lifecycle":{"kind":"oneshot"},"environment":{"set":{"REPL_SECRET":secret}},"output":{"type":"text","encoding":"raw"}}
    });
    std::fs::write(manifests.join("secret.echo.json"), manifest.to_string()).unwrap();
    let policy = directory.path().join("policy.json");
    std::fs::write(&policy, r#"{"version":1,"secrets":{"environment":{"secret.echo":["REPL_SECRET"]}}}"#).unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--manifest-dir"]).arg(&manifests)
        .args(["--data-flow-policy"]).arg(&policy)
        .current_dir(directory.path()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        r#"{"submit":"run secret.echo as secret_value"}"#,
        r#"{"explain":true}"#,
        r#"{"bindings":true}"#,
        r#"{"source":true}"#,
        r#"{"undo":1}"#,
        r#"{"source":true}"#,
        r#"{"submit":"run secret.echo as secret_value"}"#,
        r#"{"save":"secret.program"}"#,
    ] { writeln!(input, "{request}").unwrap(); }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let responses = parse_lines(&output.stdout);
    assert_eq!(responses.len(), 8);
    assert_eq!(responses[0]["status"], "ran", "{responses:?}");
    // A typed binding consumes applet stdout; it is exposed only as a redacted
    // binding preview, not as terminal stdout.
    let expected_stdout = "";
    assert_eq!(responses[0]["stdout"], expected_stdout);
    assert_eq!(responses[0]["bindings"]["secret_value"]["type"], "string");
    assert_eq!(responses[1]["status"], "ok");
    assert_eq!(responses[2]["status"], "ok");
    assert_eq!(responses[2]["bindings"]["secret_value"]["preview"], "\"[REDACTED]\"");
    assert_eq!(responses[3]["source"], "run secret.echo as secret_value\n");
    assert_eq!(responses[4]["status"], "undone");
    assert_eq!(responses[5]["source"], "");
    assert_eq!(responses[7]["status"], "saved");
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!responses[1].to_string().contains(secret));
}

#[test]
fn oversized_request_is_rejected_and_the_next_line_is_still_processed() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    write_version_manifest(&manifests);
    let mut child = repl_child(directory.path(), &manifests, false);
    let mut input = child.stdin.take().unwrap();
    input.write_all(&vec![b' '; 1024 * 1024 + 1]).unwrap();
    input.write_all(b"\n{\"source\":true}\n").unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses = parse_lines(&output.stdout);
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["status"], "error");
    assert_eq!(responses[1]["source"], "");
}

fn write_version_manifest(manifests: &std::path::Path) {
    let manifest = serde_json::json!({
        "name":"ok.helper", "binary":env!("CARGO_BIN_EXE_execute-model"),
        "accepts_stdin":false, "parameters":{}, "arguments":[{"literal":"--version"}],
        "contract":{"version":1,"output":{"type":"text","encoding":"raw"}}
    });
    std::fs::write(manifests.join("ok.helper.json"), manifest.to_string()).unwrap();
}

fn repl_child(directory: &std::path::Path, manifests: &std::path::Path, what_if: bool) -> std::process::Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.args(["repl", "--json"]);
    if what_if { command.arg("--what-if"); }
    command.arg("--manifest-dir").arg(manifests).current_dir(directory)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

fn parse_lines(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8(bytes.to_vec()).unwrap().lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()).collect()
}
