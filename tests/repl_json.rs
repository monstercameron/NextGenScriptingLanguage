use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn json_repl_returns_one_response_per_request_and_reports_block_spans() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    write_test_helper_manifest(&manifests);
    std::fs::write(directory.path().join("notes.txt"), "from-read").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--manifest-dir"])
        .arg(manifests)
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(
        input,
        "{{\"submit\":\"run ok.helper as value and keep status as exit_code\"}}"
    )
    .unwrap();
    writeln!(input, "{{\"submit\":\"run missing.helper\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"use value \\\"from-use-value\\\"\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"read file \\\"notes.txt\\\"\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"run ok.helper as discard\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"\"}}").unwrap();
    writeln!(input, "{{\"source\":true}}").unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 7);
    assert_eq!(responses[0]["status"], "ran", "{responses:?}");
    assert_eq!(responses[0]["bindings"]["value"]["type"], "string");
    assert_eq!(responses[0]["bindings"]["exit_code"]["type"], "integer");
    assert_eq!(responses[1]["status"], "rejected");
    assert!(responses[1]["diagnostics"][0]["span"]["block"].is_object());
    assert_eq!(responses[2]["stdout"], "from-use-value");
    assert_eq!(responses[3]["stdout"], "from-read");
    assert_eq!(responses[4]["stdout"], "", "{responses:?}");
    assert_eq!(responses[5]["stdout"], "", "{responses:?}");
    assert_eq!(responses[6]["source"], "run ok.helper as value and keep status as exit_code\nuse value \"from-use-value\"\nread file \"notes.txt\"\nrun ok.helper as discard\n");
}

#[test]
fn json_repl_captures_each_fresh_streamed_command_without_leaking_to_process_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let helper_output = write_test_helper_manifest(&manifests);
    let mut child = repl_child(directory.path(), &manifests, false);
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{{\"submit\":\"run ok.helper\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"run ok.helper\\nrun ok.helper\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"run ok.helper as discard\"}}").unwrap();
    writeln!(input, "{{\"submit\":\"\"}}").unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = parse_lines(&output.stdout);
    assert_eq!(responses.len(), 4);
    assert_eq!(responses[0]["stdout"], helper_output);
    assert_eq!(
        responses[1]["stdout"],
        format!("{helper_output}{helper_output}")
    );
    assert_eq!(responses[2]["stdout"], "", "{responses:?}");
    assert_eq!(responses[3]["stdout"], "", "{responses:?}");
}

#[test]
fn json_redaction_preserves_protocol_keys_and_redacts_marker_containing_secrets() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let marker_secret = "prefix[REDACTED]suffix";
    let arguments = if cfg!(windows) {
        serde_json::json!([{"literal":"/c"},{"literal":"echo %REPL_SECRET% %KEY_SECRET%"}])
    } else {
        serde_json::json!([{"literal":"-c"},{"literal":"printf '%s|%s' \"$REPL_SECRET\" \"$KEY_SECRET\""}])
    };
    let manifest = serde_json::json!({
        "name":"secret.echo", "binary":if cfg!(windows) {"cmd"} else {"/bin/sh"}, "arguments":arguments,
        "contract":{"version":1,"backend":"command","lifecycle":{"kind":"oneshot"},"environment":{"set":{"REPL_SECRET":marker_secret,"KEY_SECRET":"status"}}}
    });
    std::fs::write(manifests.join("secret.echo.json"), manifest.to_string()).unwrap();
    let policy = directory.path().join("policy.json");
    std::fs::write(
        &policy,
        r#"{"version":1,"secrets":{"environment":{"secret.echo":["REPL_SECRET","KEY_SECRET"]}}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--manifest-dir"])
        .arg(&manifests)
        .args(["--data-flow-policy"])
        .arg(&policy)
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{{\"submit\":\"run secret.echo\"}}").unwrap();
    writeln!(
        input,
        "{{\"submit\":\"use value \\\"{marker_secret}\\\"\"}}"
    )
    .unwrap();
    writeln!(input, "{{\"source\":true}}").unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8(output.stdout).unwrap();
    assert!(
        !raw.contains(marker_secret),
        "secret leaked in protocol output: {raw}"
    );
    let responses = parse_lines(raw.as_bytes());
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["status"], "ran", "{}", responses[0]);
    assert_eq!(
        responses[0]["stdout"],
        if cfg!(windows) {
            "[REDACTED] [REDACTED]\r\n"
        } else {
            "[REDACTED]|[REDACTED]"
        }
    );
    assert_eq!(responses[1]["status"], "ran", "{}", responses[1]);
    assert!(!responses[2]["source"]
        .as_str()
        .unwrap()
        .contains(marker_secret));
}

#[test]
fn json_repl_frames_manifest_inherited_stdout_as_response_output() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let (binary, arguments) = if cfg!(windows) {
        (
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
            serde_json::json!([{"literal":"/C"},{"literal":"echo FRAME-SENTINEL&echo ERR-SENTINEL 1>&2"}]),
        )
    } else {
        (
            "/bin/sh".to_owned(),
            serde_json::json!([{"literal":"-c"},{"literal":"printf FRAME-SENTINEL; printf ERR-SENTINEL >&2"}]),
        )
    };
    let manifest = serde_json::json!({
        "name":"fixture.inherit", "binary":binary,
        "accepts_stdin":false, "parameters":{}, "arguments":arguments,
        "stdio":{"stdout":"inherit","stderr":"inherit"}
    });
    std::fs::write(manifests.join("fixture.inherit.json"), manifest.to_string()).unwrap();
    let mut child = repl_child(directory.path(), &manifests, false);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"submit\":\"run fixture.inherit\"}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = parse_lines(&output.stdout);
    assert_eq!(
        responses.len(),
        1,
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(responses[0]["status"], "ran");
    let framed_stdout = responses[0]["stdout"].as_str().unwrap();
    assert!(
        framed_stdout == "FRAME-SENTINEL"
            || (cfg!(windows)
                && matches!(framed_stdout, "FRAME-SENTINEL\r\n" | "FRAME-SENTINEL\n")),
        "unexpected framed stdout: {framed_stdout:?}"
    );
    assert!(
        responses[0]["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("ERR-SENTINEL"),
        "unexpected framed stderr: {}",
        responses[0]
    );
    assert!(
        output.stderr.is_empty(),
        "child stderr escaped the protocol: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn failed_json_submission_returns_redacted_partial_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let secret = "partial-failure-secret";
    let (binary, arguments) = if cfg!(windows) {
        (
            "cmd",
            serde_json::json!([{"literal":"/c"},{"literal":"<nul set /p =%REPL_SECRET%& exit /b 7"}]),
        )
    } else {
        (
            "/bin/sh",
            serde_json::json!([{"literal":"-c"},{"literal":"printf %s \"$REPL_SECRET\"; exit 7"}]),
        )
    };
    let manifest = serde_json::json!({
        "name":"secret.fail", "binary":binary, "arguments":arguments,
        "contract":{"version":1,"backend":"command","lifecycle":{"kind":"oneshot"},"environment":{"set":{"REPL_SECRET":secret}}}
    });
    std::fs::write(manifests.join("secret.fail.json"), manifest.to_string()).unwrap();
    let policy = directory.path().join("policy.json");
    std::fs::write(
        &policy,
        r#"{"version":1,"secrets":{"environment":{"secret.fail":["REPL_SECRET"]}}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--manifest-dir"])
        .arg(&manifests)
        .args(["--data-flow-policy"])
        .arg(&policy)
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"submit\":\"run secret.fail\"}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8(output.stdout).unwrap();
    assert!(
        !raw.contains(secret),
        "secret leaked in protocol output: {raw}"
    );
    let responses = parse_lines(raw.as_bytes());
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["status"], "failed");
    assert_eq!(responses[0]["stdout"], "[REDACTED]");
}

#[test]
fn failed_multicommand_submission_keeps_ordered_redacted_partial_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let first_secret = "first-command-secret";
    let failed_secret = "failed-command-secret";
    for (name, env_name, secret, unix_script, windows_script) in [
        (
            "first.echo",
            "FIRST_SECRET",
            first_secret,
            "printf 'first=%s|' \"$FIRST_SECRET\"",
            "echo|set /p=first=%FIRST_SECRET%^|",
        ),
        (
            "last.fail",
            "FAILED_SECRET",
            failed_secret,
            "printf 'last=%s' \"$FAILED_SECRET\"; exit 7",
            "echo|set /p=last=%FAILED_SECRET%^| & exit /b 7",
        ),
    ] {
        let (binary, arguments) = shell_command(unix_script, windows_script);
        let environment = serde_json::Map::from_iter([(
            env_name.to_owned(),
            serde_json::Value::String(secret.to_owned()),
        )]);
        let manifest = serde_json::json!({
            "name":name, "binary":binary,
            "arguments":arguments.iter().map(|argument| serde_json::json!({"literal":argument})).collect::<Vec<_>>(),
            "contract":{"version":1,"backend":"command","lifecycle":{"kind":"oneshot"},"environment":{"set":environment}}
        });
        std::fs::write(manifests.join(format!("{name}.json")), manifest.to_string()).unwrap();
    }
    let policy = directory.path().join("policy.json");
    std::fs::write(
        &policy,
        r#"{"version":1,"secrets":{"environment":{"first.echo":["FIRST_SECRET"],"last.fail":["FAILED_SECRET"]}}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--manifest-dir"])
        .arg(&manifests)
        .args(["--data-flow-policy"])
        .arg(&policy)
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let request = serde_json::json!({"submit":"run first.echo\nrun last.fail"});
    writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8(output.stdout).unwrap();
    assert!(!raw.contains(first_secret));
    assert!(!raw.contains(failed_secret));
    let responses = parse_lines(raw.as_bytes());
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["status"], "failed");
    assert_eq!(responses[0]["stdout"], "first=[REDACTED]|last=[REDACTED]");
}

#[test]
fn failed_pipeline_submission_returns_redacted_terminal_partial_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let secret = "pipeline-failure-secret";
    let (first_binary, first_arguments) = shell_command(
        "printf ignored-pipeline-input",
        "echo ignored-pipeline-input",
    );
    let first = serde_json::json!({
        "name":"pipeline.first", "binary":first_binary,
        "arguments":first_arguments.iter().map(|argument| serde_json::json!({"literal":argument})).collect::<Vec<_>>(),
        "contract":{"version":1,"backend":"command","lifecycle":{"kind":"oneshot"}}
    });
    let (last_binary, last_arguments) = shell_command(
        "cat >/dev/null; printf 'pipeline=%s' \"$PIPELINE_SECRET\"; exit 7",
        "more >nul & echo|set /p=pipeline=%PIPELINE_SECRET%^| & exit /b 7",
    );
    let last = serde_json::json!({
        "name":"pipeline.last", "binary":last_binary, "accepts_stdin":true,
        "arguments":last_arguments.iter().map(|argument| serde_json::json!({"literal":argument})).collect::<Vec<_>>(),
        "contract":{"version":1,"backend":"command","lifecycle":{"kind":"oneshot"},"environment":{"set":{"PIPELINE_SECRET":secret}}}
    });
    std::fs::write(manifests.join("pipeline.first.json"), first.to_string()).unwrap();
    std::fs::write(manifests.join("pipeline.last.json"), last.to_string()).unwrap();
    let policy = directory.path().join("policy.json");
    std::fs::write(
        &policy,
        r#"{"version":1,"secrets":{"environment":{"pipeline.last":["PIPELINE_SECRET"]}}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--manifest-dir"])
        .arg(&manifests)
        .args(["--data-flow-policy"])
        .arg(&policy)
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"submit\":\"run pipeline.first\\npipe to pipeline.last\"}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8(output.stdout).unwrap();
    assert!(!raw.contains(secret));
    let responses = parse_lines(raw.as_bytes());
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["status"], "failed", "{responses:?}");
    assert_eq!(
        responses[0]["stdout"], "pipeline=[REDACTED]",
        "{responses:?}"
    );
}

#[test]
fn what_if_returns_a_plan_without_running_the_block() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let (binary, arguments) = if cfg!(windows) {
        (
            "cmd",
            vec!["/c".to_owned(), "type nul > marker.txt".to_owned()],
        )
    } else {
        (
            "/bin/sh",
            vec!["-c".to_owned(), "touch marker.txt".to_owned()],
        )
    };
    let manifest = serde_json::json!({
        "name":"side.effect", "binary":binary,
        "accepts_stdin":false, "parameters":{}, "arguments":arguments.iter().map(|arg| serde_json::json!({"literal":arg})).collect::<Vec<_>>()
    });
    std::fs::write(manifests.join("side.effect.json"), manifest.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--what-if", "--manifest-dir"])
        .arg(manifests)
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"{\"submit\":\"run side.effect\"}\n{\"source\":true}\n")
                .unwrap();
            child.wait_with_output().unwrap()
        })
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    write_test_helper_manifest(&manifests);
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
    for request in requests {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let repl_output = child.wait_with_output().unwrap();
    assert!(
        repl_output.status.success(),
        "{}",
        String::from_utf8_lossy(&repl_output.stderr)
    );
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
        .arg(&manifests)
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([saved.to_str().unwrap(), "--manifest-dir"])
        .arg(&manifests)
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let session_output = responses
        .iter()
        .filter_map(|response| response["stdout"].as_str())
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
        (
            "cmd",
            serde_json::json!([{"literal":"/c"},{"literal":"echo %REPL_SECRET%"}]),
        )
    } else {
        (
            "/bin/sh",
            serde_json::json!([{"literal":"-c"},{"literal":r#"printf %s "$REPL_SECRET""#}]),
        )
    };
    let manifest = serde_json::json!({
        "name":"secret.echo", "binary":binary, "arguments":arguments,
        "contract":{"version":1,"backend":"command","lifecycle":{"kind":"oneshot"},"environment":{"set":{"REPL_SECRET":secret}},"output":{"type":"text","encoding":"raw"}}
    });
    std::fs::write(manifests.join("secret.echo.json"), manifest.to_string()).unwrap();
    let policy = directory.path().join("policy.json");
    std::fs::write(
        &policy,
        r#"{"version":1,"secrets":{"environment":{"secret.echo":["REPL_SECRET"]}}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["repl", "--json", "--manifest-dir"])
        .arg(&manifests)
        .args(["--data-flow-policy"])
        .arg(&policy)
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let secret_source = serde_json::json!({"submit":format!("use value \"{secret}\"")}).to_string();
    let secret_path = serde_json::json!({"save":secret}).to_string();
    let no_op = serde_json::json!({"submit":"run secret.echo as discard"}).to_string();
    for request in [
        r#"{"submit":"run secret.echo as secret_value"}"#,
        r#"{"explain":true}"#,
        r#"{"bindings":true}"#,
        r#"{"source":true}"#,
        r#"{"undo":1}"#,
        r#"{"source":true}"#,
        r#"{"submit":"run secret.echo as secret_value"}"#,
        r#"{"save":"secret.program"}"#,
        &secret_source,
        r#"{"source":true}"#,
        &secret_path,
        &secret_path,
        &no_op,
        r#"{"undo":1}"#,
        r#"{"source":true}"#,
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = parse_lines(&output.stdout);
    assert_eq!(responses.len(), 15);
    assert_eq!(responses[0]["status"], "ran", "{responses:?}");
    // A typed binding consumes applet stdout; it is exposed only as a redacted
    // binding preview, not as terminal stdout.
    let expected_stdout = "";
    assert_eq!(responses[0]["stdout"], expected_stdout);
    assert_eq!(responses[0]["bindings"]["secret_value"]["type"], "string");
    assert_eq!(responses[1]["status"], "ok");
    assert_eq!(responses[2]["status"], "ok");
    assert_eq!(
        responses[2]["bindings"]["secret_value"]["preview"],
        "\"[REDACTED]\""
    );
    assert_eq!(responses[3]["source"], "run secret.echo as secret_value\n");
    assert_eq!(responses[4]["status"], "undone");
    assert_eq!(responses[5]["source"], "");
    assert_eq!(responses[7]["status"], "saved");
    assert_eq!(responses[8]["stdout"], "[REDACTED]");
    assert!(responses[9]["source"]
        .as_str()
        .unwrap()
        .contains("use value \"[REDACTED]\""));
    assert_eq!(responses[10]["path"], "[REDACTED]");
    assert_eq!(responses[11]["status"], "error");
    assert!(!responses[11].to_string().contains(secret));
    assert_eq!(responses[12]["stdout"], "", "{responses:?}");
    assert!(responses[13]["source"]
        .as_str()
        .unwrap()
        .contains("use value \"[REDACTED]\""));
    assert!(responses[14]["source"]
        .as_str()
        .unwrap()
        .contains("use value \"[REDACTED]\""));
    assert!(std::fs::read_to_string(directory.path().join(secret))
        .unwrap()
        .contains(secret));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(responses
        .iter()
        .all(|response| !response.to_string().contains(secret)));
}

#[test]
fn oversized_request_is_rejected_and_the_next_line_is_still_processed() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    write_test_helper_manifest(&manifests);
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

fn write_test_helper_manifest(manifests: &std::path::Path) -> String {
    let (binary, arguments) = if cfg!(windows) {
        ("cmd", vec!["/c".to_owned(), "echo repl-helper".to_owned()])
    } else {
        (
            "/bin/sh",
            vec!["-c".to_owned(), "printf '%s' repl-helper".to_owned()],
        )
    };
    let output = Command::new(binary).args(&arguments).output().unwrap();
    assert!(output.status.success());
    let expected_output = String::from_utf8(output.stdout).unwrap();
    let arguments = arguments
        .iter()
        .map(|argument| serde_json::json!({"literal": argument}))
        .collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "name":"ok.helper", "binary":binary,
        "accepts_stdin":false, "parameters":{}, "arguments":arguments,
        "contract":{"version":1,"output":{"type":"text","encoding":"raw"}}
    });
    std::fs::write(manifests.join("ok.helper.json"), manifest.to_string()).unwrap();
    expected_output
}

fn repl_child(
    directory: &std::path::Path,
    manifests: &std::path::Path,
    what_if: bool,
) -> std::process::Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.args(["repl", "--json"]);
    if what_if {
        command.arg("--what-if");
    }
    command
        .arg("--manifest-dir")
        .arg(manifests)
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn shell_command(unix_script: &str, windows_script: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec!["/c".to_owned(), windows_script.to_owned()],
        )
    } else {
        (
            "/bin/sh".to_owned(),
            vec!["-c".to_owned(), unix_script.to_owned()],
        )
    }
}

fn parse_lines(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect()
}
