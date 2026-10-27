//! `execute explain FILE --json` through the real binary. The plan is derived
//! from parsed steps and resolved manifests; nothing runs.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const HELPER: &str = env!("CARGO_BIN_EXE_execute-model");

fn write_manifests(directory: &Path) -> PathBuf {
    let manifests = directory.join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    // No contract: effects must stay undeclared (null) in the plan.
    let plain = serde_json::json!({
        "name": "plain.helper",
        "binary": HELPER,
        "accepts_stdin": false,
        "parameters": { "text": { "type": "string" } },
        "arguments": [{ "literal": "--version" }, { "when_present": "text", "append": [{ "literal": "--text" }, { "parameter": "text" }] }]
    });
    std::fs::write(manifests.join("plain.helper.json"), plain.to_string()).unwrap();
    let nested = serde_json::json!({
        "name": "nested.helper", "binary": HELPER, "accepts_stdin": false,
        "parameters": {"data": {"type": "json", "required": true}},
        "arguments": [{"json_parameter": "data"}]
    });
    std::fs::write(manifests.join("nested.helper.json"), nested.to_string()).unwrap();
    // A contract that declares a loopback network effect and a write effect.
    let declared = serde_json::json!({
        "name": "net.helper",
        "binary": HELPER,
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [{ "literal": "--version" }],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": { "kind": "oneshot" },
            "capabilities": ["network.loopback", "fs.write.workspace"],
            "effects": ["network.loopback", "fs.write.workspace"]
        }
    });
    std::fs::write(manifests.join("net.helper.json"), declared.to_string()).unwrap();
    // The same shape as the repository's builtin `cd` manifest.
    let cd = serde_json::json!({
        "name": "cd",
        "binary": "cd",
        "builtin": "cd",
        "parameters": { "path": { "type": "string", "required": true } },
        "arguments": [{ "parameter": "path" }]
    });
    std::fs::write(manifests.join("cd.json"), cd.to_string()).unwrap();
    // A stdin-accepting stage with a declared network effect, for pipe groups.
    let stage = serde_json::json!({
        "name": "stdin.helper",
        "binary": HELPER,
        "accepts_stdin": true,
        "parameters": {},
        "arguments": [{ "literal": "--version" }],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": { "kind": "oneshot" },
            "capabilities": ["network.loopback"],
            "effects": ["network.loopback"]
        }
    });
    std::fs::write(manifests.join("stdin.helper.json"), stage.to_string()).unwrap();
    manifests
}

fn explain(directory: &Path, program: &str, extra: &[&str]) -> Output {
    let manifests = write_manifests(directory);
    std::fs::write(directory.join("plan.program"), program).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .arg("explain")
        .arg("plan.program")
        .args(extra)
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.output().expect("start execute")
}

#[test]
fn unresolved_direct_executable_is_false_in_json_plan() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path());
    std::fs::write(
        directory.path().join("plan.program"),
        "run \"execute-missing-510-fixture\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["explain", "plan.program", "--json", "--manifest-dir"])
        .arg(manifests)
        .current_dir(directory.path())
        .env("PATH", directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .expect("start execute");
    let object = plan(&output);
    assert_eq!(object["steps"][0]["binary"], "execute-missing-510-fixture");
    assert_eq!(object["steps"][0]["resolved"], false);
    assert!(object["steps"][0]["resolved_path"].is_null());
}

#[cfg(unix)]
#[test]
fn direct_executable_resolution_uses_the_walkers_known_folder() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path());
    let scripts = directory.path().join("scripts");
    std::fs::create_dir(&scripts).unwrap();
    let bin = directory.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let printf = bin.join("printf");
    std::fs::write(&printf, "placeholder; explain must not execute this file\n").unwrap();
    std::fs::set_permissions(&printf, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        scripts.join("plan.program"),
        "use folder \"bin\"\nrun \"printf\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "explain",
            "scripts/plan.program",
            "--json",
            "--manifest-dir",
        ])
        .arg(manifests)
        .current_dir(directory.path())
        .env("PATH", ".")
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .expect("start execute");
    let object = plan(&output);
    let invocation = rows_of_kind(&object, "invoke")[0];
    assert_eq!(invocation["resolved"], true);
    assert_eq!(
        invocation["resolved_path"],
        std::fs::canonicalize(printf)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
}

fn plan(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("--json prints one object")
}

#[test]
fn json_plan_has_one_row_per_invocation_with_resolved_argv_and_declared_effects() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "run net.helper\nrun plain.helper with text \"hello\"\n",
        &["--json"],
    );
    let object = plan(&output);
    assert_eq!(object["protocol"], "execute.explain/2");
    let steps = object["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2, "{steps:?}");

    let declared = &steps[0];
    assert_eq!(declared["applet"], "net.helper");
    assert_eq!(declared["argv"], serde_json::json!(["--version"]));
    assert_eq!(
        declared["effects"],
        serde_json::json!(["network.loopback", "fs.write.workspace"])
    );
    assert_eq!(declared["network"], true);
    assert_eq!(declared["writes"], true);

    let plain = &steps[1];
    assert_eq!(plain["applet"], "plain.helper");
    assert_eq!(
        plain["argv"],
        serde_json::json!(["--version", "--text", "hello"])
    );
    assert!(
        plain["effects"].is_null(),
        "undeclared effects must be null"
    );
    assert!(plain["network"].is_null());
    assert!(plain["writes"].is_null());
}

#[test]
fn listener_producer_is_an_invocation_row() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "listen to \"sudo\" with arguments [\"rm\", \"-rf\", \"/important\"]\nwhenever a line arrives as msg\n  run \"printf\" with arguments [\"x\"]\n",
        &["--json"],
    );
    let plan = plan(&output);
    let producer = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["kind"] == "listen_source")
        .expect("listener producer row");
    assert_eq!(producer["line"], 1);
    assert_eq!(producer["binary"], "sudo");
    assert_eq!(producer["privileged"], true);
    assert_eq!(producer["undeclared"], true);
    assert_eq!(plan["summary"]["privileged"], true);
    assert!(plan["summary"]["executables"]
        .as_array()
        .unwrap()
        .contains(&"sudo".into()));
}

#[test]
fn explain_withholds_literals_from_an_explicitly_secret_output() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "CANARY-EXPLAIN-SECRET-315";
    let output = explain(
        directory.path(),
        &format!("run \"printf\" with arguments [\"%s\", \"{canary}\"] as token and keep secret\n"),
        &["--json"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains(canary));
    assert!(text.contains("withheld"), "{text}");
}

#[test]
fn program_module_body_effects_are_expanded() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    std::fs::write(
        manifests.join("body.program"),
        "define a program named helper\n  accept note as text\n  run \"sudo\" with arguments [\"rm\", \"-rf\", \"/important\"]\n",
    )
    .unwrap();
    let manifest = serde_json::json!({
        "name": "fx.helper", "binary": "@program-module", "accepts_stdin": false,
        "parameters": {"note": {"type": "string", "required": true}}, "arguments": [],
        "contract": {
            "version": 1, "backend": "program_module", "program": {
                "source": "body.program", "entry": "helper", "interface_version": 1
            }, "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw"}, "capabilities": [], "effects": []
        }
    });
    std::fs::write(manifests.join("fx.helper.json"), manifest.to_string()).unwrap();
    let program = directory.path().join("caller.program");
    std::fs::write(
        &program,
        "### execute-policy: allow none\nrun fx.helper with note \"hi\" as r\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("explain")
        .arg(&program)
        .arg("--json")
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let sudo = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["binary"] == "sudo")
        .unwrap();
    assert_eq!(sudo["privileged"], true);
    assert_eq!(sudo["undeclared"], true);
}

#[test]
fn argv_shows_unresolved_and_supplied_input_values() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "accept message as text\nrun plain.helper with text message\n",
        &["--json"],
    );
    let object = plan(&output);
    let steps = object["steps"].as_array().unwrap();
    let invocation = steps
        .iter()
        .find(|row| row["applet"] == "plain.helper")
        .expect("plain.helper row");
    assert_eq!(
        invocation["argv"],
        serde_json::json!(["--version", "--text", {"ref":"message"}])
    );
    assert_eq!(
        invocation["parameters"]["text"],
        serde_json::json!({"ref":"message"})
    );
    let supplied = plan(&explain(
        directory.path(),
        "accept message as text\nrun plain.helper with text message\n",
        &["--json", "--input", "message=hello"],
    ));
    assert_eq!(
        supplied["steps"][0]["argv"],
        serde_json::json!(["--version", "--text", "hello"])
    );
}

#[test]
fn summary_lists_executables_writes_network_and_undeclared_count() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "run net.helper\nrun plain.helper\n",
        &["--json"],
    );
    let summary = &plan(&output)["summary"];
    assert_eq!(summary["invocations"], 2);
    assert_eq!(summary["network"], true);
    assert_eq!(summary["writes"], true);
    assert_eq!(summary["effects_declared"], false);
    assert_eq!(summary["undeclared_invocations"], 1);
    let executables = summary["executables"].as_array().unwrap();
    assert_eq!(
        executables.len(),
        1,
        "both manifests share one helper binary: {executables:?}"
    );
    assert_eq!(summary["privileged"], false);
    assert_eq!(summary["privileged_invocations"], 0);
}

#[test]
fn save_steps_count_as_writes_and_other_steps_have_no_effects() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "accept message as text\nrun plain.helper\nthen save to \"out.txt\"\n",
        &["--json"],
    );
    let object = plan(&output);
    let steps = object["steps"].as_array().unwrap();
    assert!(steps
        .iter()
        .any(|row| row["kind"] == "save" && row["writes"] == true));
    // `accept` declares an input rather than a step, so this program has no
    // "other" row. Any "other" row that does appear must carry no effects.
    assert!(steps
        .iter()
        .all(|row| row["kind"] != "other" || row["effects"].is_null()));
    assert_eq!(object["summary"]["writes"], true);
}

#[test]
fn text_explain_renders_ast_values_without_debug_wrappers() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        r#"accept name as text with default "w"
use folder "work"
read "in.txt" as data
use value "hello"
then save to "out.txt"
then append to "log.txt"
run "printf" with arguments [name, data]
use value {"outer": ["line\nbreak", "quote\"slash\\"]} as nested
if data is "a"
  use value "x"
repeat 2 times
  use value "y"
"#,
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "default \"w\"",
        "use folder \"work\"",
        "read \"in.txt\"",
        "use value \"hello\"",
        "save finite output to \"out.txt\"",
        "append finite output to \"log.txt\"",
        "parameters {arguments: [name, data]} (resolve named results at runtime)",
        "bind value {\"outer\": [\"line\\nbreak\", \"quote\\\"slash\\\\\"]} as nested",
        "if data is \"a\"",
        "loop repeat 2;",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
    for leaked in ["String(\"", "Reference(\"", "Repeat {", "Integer("] {
        assert!(
            !text.contains(leaked),
            "debug AST rendering {leaked:?} in:\n{text}"
        );
    }
}

#[test]
fn text_explain_renders_dynamic_pointers_and_redirect_paths_once() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        r#"accept pointer as text with default "/field"
use value {"field": "value"} as data
take text at pointer from data as selected
run "printf" with arguments ["x"] and stderr to "err.txt"
"#,
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("dynamic pointer pointer from data"), "{text}");
    assert!(
        text.contains("stderr replaces file \"err.txt\"; stdout is unchanged"),
        "{text}"
    );
    assert!(!text.contains("Reference(\"pointer\")"), "{text}");
    assert!(!text.contains("file \\\"err.txt\\\""), "{text}");
}

#[test]
fn text_explain_redacts_sensitive_input_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        r#"accept token as text from secret prompt "Token?" with default "secret-default-canary-223"
use value token
"#,
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("default [redacted]"), "{text}");
    assert!(!text.contains("secret-default-canary-223"), "{text}");
}

#[test]
fn text_explain_is_unchanged_without_the_flag() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(directory.path(), "run plain.helper\n", &[]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        !text.trim_start().starts_with('{'),
        "text mode printed JSON: {text}"
    );
}

#[test]
fn text_explain_escapes_terminal_controls_and_bidi_in_program_text() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run \"printf\" with arguments [\"\\u001b[31m\\u009b31m\\u202e\\n\\t\"] and environment {\"KEY\\u202e\\u2028\\u2029\": \"hidden\"}\n";
    let output = explain(directory.path(), source, &[]);
    assert!(
        output.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        !text.contains('\u{001b}'),
        "raw ESC in explain output: {text:?}"
    );
    assert!(
        !text.contains('\u{009b}'),
        "raw C1 CSI in explain output: {text:?}"
    );
    assert!(
        !text.contains('\u{202e}'),
        "raw bidi override in explain output: {text:?}"
    );
    assert!(
        !text.contains('\u{2028}') && !text.contains('\u{2029}'),
        "raw Unicode line separator in explain output: {text:?}"
    );
    assert_eq!(
        text.matches('\n').count(),
        4,
        "unexpected injected line: {text:?}"
    );
    assert!(text.contains(r"\u{1b}[31m\u{9b}31m\u{202e}"), "{text:?}");
    assert!(text.contains(r"KEY\u{202e}\u{2028}\u{2029}"), "{text:?}");
    assert!(
        !text.contains("hidden"),
        "environment values must stay hidden: {text:?}"
    );
}

#[test]
fn json_is_rejected_for_run_and_the_check_json_contract_is_intact() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path());
    std::fs::write(directory.path().join("ok.program"), "run plain.helper\n").unwrap();
    let run_json = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg("ok.program")
        .arg("--json")
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!run_json.status.success(), "run must not accept --json");

    let check = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("check")
        .arg("ok.program")
        .arg("--json")
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let object: Value = serde_json::from_slice(&check.stdout).expect("check --json object");
    assert_eq!(object["protocol"], "execute.check/1");
}

// ---------------------------------------------------------------------------
// Fail-closed classification: every step kind is named, and called definitions
// are walked. A plan may claim `effects_declared` only when nothing is unknown.
// ---------------------------------------------------------------------------

fn rows_of_kind<'a>(object: &'a Value, kind: &str) -> Vec<&'a Value> {
    object["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == kind)
        .collect()
}

#[test]
fn every_new_step_kind_is_classified_with_its_effect() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept message as text\n\
use folder \"sub\"\n\
read \"input.txt\"\n\
run plain.helper with text \"x\" and environment {\"K\": \"v\"}\n\
run plain.helper interactively\n\
run plain.helper and stderr to \"err.txt\"\n\
run plain.helper\n\
then append to \"log.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));

    let folder = rows_of_kind(&object, "use_folder");
    assert_eq!(folder.len(), 1, "{object}");
    assert_eq!(folder[0]["cwd_change"], true);
    assert_eq!(folder[0]["path"], "sub");

    let read = rows_of_kind(&object, "read");
    assert_eq!(read.len(), 1, "{object}");
    assert_eq!(read[0]["reads"], true);
    assert_eq!(read[0]["path"], "input.txt");

    let environment = rows_of_kind(&object, "environment");
    assert_eq!(environment.len(), 1, "{object}");
    assert_eq!(environment[0]["modifies_next_invocation"], true);

    let interactive = rows_of_kind(&object, "interactive");
    assert_eq!(interactive.len(), 1, "{object}");
    assert_eq!(interactive[0]["modifies_next_invocation"], true);

    let redirect = rows_of_kind(&object, "redirect");
    assert_eq!(redirect.len(), 1, "{object}");
    assert_eq!(redirect[0]["writes"], true);
    assert_eq!(redirect[0]["path"], "sub/err.txt");
    assert_eq!(redirect[0]["modifies_next_invocation"], true);

    let append = rows_of_kind(&object, "append");
    assert_eq!(append.len(), 1, "{object}");
    assert_eq!(append[0]["writes"], true);
    assert_eq!(append[0]["path"], "sub/log.txt");

    let summary = &object["summary"];
    assert_eq!(summary["writes"], true);
    assert_eq!(summary["cwd_changes"], true);
    assert_eq!(summary["reads"], true);
    assert_eq!(summary["modifies_next_invocation"], true);
    let paths: Vec<&str> = summary["write_paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        paths.contains(&"sub/err.txt") && paths.contains(&"sub/log.txt"),
        "{paths:?}"
    );
}

#[test]
fn runtime_destination_of_a_write_is_reported_not_guessed() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept target as text\nrun plain.helper\nthen save to target\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert_eq!(save.len(), 1, "{object}");
    assert_eq!(save[0]["writes"], true);
    assert!(
        save[0]["path"].is_null(),
        "runtime path must not be guessed"
    );
    assert_eq!(save[0]["path_is_runtime"], true);
    assert_eq!(object["summary"]["runtime_write_paths"], 1);
    assert_eq!(object["summary"]["writes"], true);
}

#[test]
fn called_definitions_are_walked_and_their_undeclared_steps_count() {
    let directory = tempfile::tempdir().unwrap();
    let source =
        "define a program named inner\n  run net.helper\n  run plain.helper\nrun program inner\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));

    let calls = rows_of_kind(&object, "call_program");
    assert_eq!(calls.len(), 1, "{object}");
    assert_eq!(calls[0]["resolved"], true);

    let nested: Vec<&Value> = object["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["scope"] == "inner")
        .collect();
    assert_eq!(
        nested.len(),
        2,
        "both definition steps must be walked: {object}"
    );

    let summary = &object["summary"];
    assert_eq!(summary["invocations"], 2);
    assert_eq!(
        summary["network"], true,
        "a called definition's network effect counts"
    );
    assert_eq!(summary["undeclared_invocations"], 1);
    assert_eq!(
        summary["effects_declared"], false,
        "an undeclared step inside a call must not look declared"
    );
}

#[test]
fn recursive_definition_is_never_reported_as_fully_declared() {
    let directory = tempfile::tempdir().unwrap();
    let source = "define a program named again\n  run program again\nrun program again\n";
    let output = explain(directory.path(), source, &["--json"]);
    if output.status.success() {
        let object: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(object["summary"]["effects_declared"], false, "{object}");
        assert!(
            object["summary"]["undeclared_invocations"]
                .as_u64()
                .unwrap()
                >= 1
        );
    } else {
        assert!(
            output.stdout.is_empty(),
            "a rejected cycle must not print a plan"
        );
    }
}

// ---------------------------------------------------------------------------
// Relative call offsets, environment taint, and working-directory tracking.
// ---------------------------------------------------------------------------

fn rows_with_applet<'a>(object: &'a Value, applet: &str) -> Vec<&'a Value> {
    object["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["applet"] == applet)
        .collect()
}

#[test]
fn a_call_at_a_nonzero_index_walks_its_definition_once_and_keeps_later_steps() {
    let directory = tempfile::tempdir().unwrap();
    let source = "define a program named inner\n  run net.helper\nrun plain.helper\nrun program inner\nrun plain.helper\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));

    assert_eq!(
        rows_with_applet(&object, "net.helper").len(),
        1,
        "the definition body must appear exactly once: {object}"
    );
    let plain = rows_with_applet(&object, "plain.helper");
    assert_eq!(
        plain.len(),
        2,
        "both top-level steps around the call must be listed: {object}"
    );
    assert!(plain
        .iter()
        .any(|row| row["line"] == 5 && row["scope"].is_null()));
    assert_eq!(object["summary"]["invocations"], 3);
    assert_eq!(object["summary"]["network"], true);
}

#[test]
fn an_environment_change_that_can_alter_the_program_undeclares_the_next_command() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run net.helper and environment {\"PYTHONPATH\": \"/x\"}\nrun net.helper\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let rows = rows_with_applet(&object, "net.helper");
    assert_eq!(rows.len(), 2, "{object}");
    assert!(
        rows[0]["effects"].is_null(),
        "tainted command must be undeclared: {object}"
    );
    assert_eq!(rows[0]["undeclared"], true);
    assert_eq!(rows[0]["environment_tainted"], true);
    // The taint applies to that command only; the next one keeps its declaration.
    assert_eq!(rows[1]["network"], true);
    assert_eq!(rows[1]["undeclared"], false);
    assert_eq!(object["summary"]["effects_declared"], false);
    assert_eq!(object["summary"]["environment_tainted_invocations"], 1);
}

#[test]
fn every_loader_or_interpreter_name_and_every_runtime_value_taints_the_next_command() {
    let directory = tempfile::tempdir().unwrap();
    for source in [
        "run net.helper and environment {\"PATH\": \"/tmp/bin\"}\n",
        "run net.helper and environment {\"LD_PRELOAD\": \"/tmp/x.so\"}\n",
        "run net.helper and environment {\"DYLD_INSERT_LIBRARIES\": \"/tmp/x.dylib\"}\n",
        "run net.helper and environment {\"NODE_OPTIONS\": \"--require /tmp/x.js\"}\n",
        "run net.helper and environment {\"PERL5LIB\": \"/tmp/lib\"}\n",
        "run net.helper and environment {\"RUBYOPT\": \"-r/tmp/x\"}\n",
        "run net.helper and without environment [\"PYTHONPATH\"]\n",
        "accept value as text\nrun net.helper and environment {\"SAFE_NAME\": value}\n",
    ] {
        let object = plan(&explain(directory.path(), source, &["--json"]));
        let rows = rows_with_applet(&object, "net.helper");
        assert!(
            rows[0]["effects"].is_null(),
            "not undeclared for {source:?}: {object}"
        );
        assert_eq!(rows[0]["environment_tainted"], true, "{source:?}");
    }
}

#[test]
fn a_harmless_environment_change_keeps_the_declared_effects() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run net.helper and environment {\"LANG\": \"C\", \"TZ\": \"UTC\"}\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let rows = rows_with_applet(&object, "net.helper");
    assert_eq!(rows[0]["network"], true, "{object}");
    assert_eq!(rows[0]["environment_tainted"], false);
    assert_eq!(rows[0]["environment_sensitive"], false);
    assert_eq!(object["summary"]["effects_declared"], true);
}

#[test]
fn a_relative_write_is_reported_under_the_folder_it_is_in() {
    let directory = tempfile::tempdir().unwrap();
    let source = "use folder \"sub\"\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert_eq!(save[0]["path"], "sub/out.txt", "{object}");
    assert_eq!(save[0]["escapes_cwd"], false);
    assert_eq!(save[0]["path_is_runtime"], false);
}

#[test]
fn a_write_that_climbs_out_of_the_program_folder_is_flagged() {
    let directory = tempfile::tempdir().unwrap();
    let source = "use folder \"sub\"\nrun plain.helper\nthen save to \"../../escape.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert_eq!(save[0]["escapes_cwd"], true, "{object}");
    assert_eq!(object["summary"]["escaping_write_paths"], 1);
}

#[test]
fn an_absolute_write_destination_is_flagged_as_escaping() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run plain.helper\nthen save to \"/tmp/absolute.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert_eq!(save[0]["escapes_cwd"], true, "{object}");
    assert_eq!(save[0]["path"], "/tmp/absolute.txt");
}

#[test]
fn a_runtime_folder_makes_later_relative_writes_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let source =
        "accept target as text\nuse folder target\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert!(save[0]["path"].is_null(), "{object}");
    assert_eq!(save[0]["path_is_runtime"], true);
    assert_eq!(object["summary"]["runtime_write_paths"], 1);
}

#[test]
fn the_builtin_cd_moves_the_known_folder_and_a_called_program_restores_it() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run cd with path \"sub\"\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert_eq!(save[0]["path"], "sub/out.txt", "{object}");

    let source = "define a program named deep\n  use folder \"deep\"\nrun program deep\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    // A folder change inside a called body may not happen on every path, so it
    // stays unknown after the call returns. The caller's folder is not restored.
    assert!(save[0]["path"].is_null(), "{object}");
    assert_eq!(save[0]["path_is_runtime"], true, "{object}");
}

// ---------------------------------------------------------------------------
// Fail-closed environment: only an inert allowlist keeps effects declared, and
// every stage of a tainted pipeline group is undeclared.
// ---------------------------------------------------------------------------

#[test]
fn only_inert_environment_names_keep_the_declared_effects() {
    let directory = tempfile::tempdir().unwrap();
    for source in [
        "run net.helper and environment {\"LANG\": \"C\"}\n",
        "run net.helper and environment {\"lang\": \"C\"}\n",
        "run net.helper and environment {\"TZ\": \"UTC\"}\n",
        "run net.helper and environment {\"LC_ALL\": \"C\"}\n",
        "run net.helper and environment {\"TERM\": \"dumb\", \"NO_COLOR\": \"1\"}\n",
    ] {
        let object = plan(&explain(directory.path(), source, &["--json"]));
        let rows = rows_with_applet(&object, "net.helper");
        assert_eq!(
            rows[0]["environment_tainted"], false,
            "{source:?}: {object}"
        );
        assert_eq!(rows[0]["undeclared"], false, "{source:?}: {object}");
        assert_eq!(rows[0]["network"], true, "{source:?}: {object}");
    }
}

#[test]
fn every_other_environment_name_taints_the_next_command() {
    let directory = tempfile::tempdir().unwrap();
    for source in [
        "run net.helper and environment {\"PATH\": \"/tmp/bin\"}\n",
        "run net.helper and environment {\"GIT_DIR\": \"/tmp/repo\"}\n",
        "run net.helper and environment {\"NODE_OPTIONS\": \"--require /tmp/x.js\"}\n",
        "run net.helper and environment {\"HOME\": \"/tmp/home\"}\n",
        "run net.helper and environment {\"FOO\": \"bar\"}\n",
        "run net.helper and environment {\"path\": \"/tmp/bin\"}\n",
        "run net.helper and environment {\"LANG\": \"C\", \"FOO\": \"bar\"}\n",
        "run net.helper and without environment [\"HOME\"]\n",
    ] {
        let object = plan(&explain(directory.path(), source, &["--json"]));
        let rows = rows_with_applet(&object, "net.helper");
        assert!(
            rows[0]["effects"].is_null(),
            "{source:?} must be undeclared: {object}"
        );
        assert_eq!(rows[0]["undeclared"], true, "{source:?}");
        assert_eq!(rows[0]["environment_tainted"], true, "{source:?}");
    }
}

#[test]
fn an_environment_change_cannot_reach_a_pipeline_group_so_its_stages_stay_declared() {
    // The language attaches `and environment` only to a single command, so no
    // environment change can taint a pipeline stage. Both forms are rejected by
    // the parser, and no plan is printed for them.
    let directory = tempfile::tempdir().unwrap();
    for source in [
        "run net.helper and environment {\"FOO\": \"x\"}\npipe to stdin.helper\n",
        "run net.helper\npipe to stdin.helper and environment {\"FOO\": \"x\"}\n",
    ] {
        let output = explain(directory.path(), source, &["--json"]);
        assert!(!output.status.success(), "{source:?} must be rejected");
        assert!(output.stdout.is_empty(), "{source:?} printed a plan");
    }
}

#[test]
fn a_clean_pipeline_keeps_every_stage_declared() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run net.helper\npipe to stdin.helper\nrun net.helper\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let stage = rows_with_applet(&object, "stdin.helper");
    assert_eq!(stage.len(), 1, "{object}");
    assert_eq!(stage[0]["environment_tainted"], false, "{object}");
    assert_eq!(stage[0]["network"], true, "{object}");
    assert_eq!(stage[0]["undeclared"], false, "{object}");
    assert_eq!(object["summary"]["environment_tainted_invocations"], 0);
}

// ---------------------------------------------------------------------------
// Advisory folder tracking: nesting, builtin detection, and lexical escapes.
// ---------------------------------------------------------------------------

#[test]
fn a_folder_change_nested_in_a_branch_makes_later_relative_writes_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let source =
        "if true is true\n  use folder \"sub\"\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert!(save[0]["path"].is_null(), "{object}");
    assert_eq!(save[0]["path_is_runtime"], true);
}

#[test]
fn a_cd_inside_a_called_body_makes_later_relative_writes_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let source = "define a program named deep\n  run cd with path \"deep\"\nrun program deep\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert!(save[0]["path"].is_null(), "{object}");
    assert_eq!(save[0]["path_is_runtime"], true);
}

#[test]
fn a_nested_folder_change_is_not_undone_by_a_later_top_level_change() {
    let directory = tempfile::tempdir().unwrap();
    let source = "if true is true\n  use folder \"sub\"\nuse folder \"other\"\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert!(
        save[0]["path"].is_null(),
        "the folder stays unknown to the end: {object}"
    );
    assert_eq!(save[0]["path_is_runtime"], true);
}

#[test]
fn a_builtin_cd_with_a_runtime_destination_makes_the_folder_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept folder as text\nrun cd with path folder\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert!(save[0]["path"].is_null(), "{object}");
    assert_eq!(save[0]["path_is_runtime"], true);
}

#[test]
fn cd_is_detected_by_the_builtin_field_not_by_its_name() {
    // A native command named `cd` with no builtin field must not move the folder.
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    let native_cd = serde_json::json!({
        "name": "cd",
        "binary": HELPER,
        "accepts_stdin": false,
        "parameters": { "path": { "type": "string", "required": true } },
        "arguments": [{ "literal": "--version" }]
    });
    std::fs::write(manifests.join("cd.json"), native_cd.to_string()).unwrap();
    let plain = serde_json::json!({
        "name": "plain.helper",
        "binary": HELPER,
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [{ "literal": "--version" }]
    });
    std::fs::write(manifests.join("plain.helper.json"), plain.to_string()).unwrap();
    std::fs::write(
        directory.path().join("plan.program"),
        "run cd with path \"sub\"\nrun plain.helper\nthen save to \"out.txt\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("explain")
        .arg("plan.program")
        .arg("--json")
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let object = plan(&output);
    let save = rows_of_kind(&object, "save");
    assert_eq!(
        save[0]["path"], "out.txt",
        "a non-builtin `cd` must not move the folder: {object}"
    );
    assert_eq!(save[0]["path_is_runtime"], false);
}

#[test]
fn a_parent_component_is_flagged_even_when_it_normalises_inside_the_folder() {
    let directory = tempfile::tempdir().unwrap();
    let source = "use folder \"sub\"\nrun plain.helper\nthen save to \"sub/../out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert_eq!(save[0]["escapes_cwd"], true, "{object}");
    assert_eq!(
        save[0]["path"], "sub/../out.txt",
        "the path is reported as written: {object}"
    );
    assert_eq!(object["summary"]["escaping_write_paths"], 1);
}

#[test]
fn a_plain_relative_write_is_not_flagged() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run plain.helper\nthen save to \"./out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert_eq!(save[0]["escapes_cwd"], false, "{object}");
    assert_eq!(save[0]["path"], "out.txt");
}

#[test]
fn a_folder_change_that_climbs_out_makes_the_folder_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let source = "use folder \"../up\"\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let folder = rows_of_kind(&object, "use_folder");
    assert_eq!(folder[0]["escapes_cwd"], true, "{object}");
    let save = rows_of_kind(&object, "save");
    assert_eq!(save[0]["path_is_runtime"], true, "{object}");
}

// ---------------------------------------------------------------------------
// Home paths are absolute for this analysis: never joined to the folder.
// ---------------------------------------------------------------------------

#[test]
fn a_home_relative_write_is_flagged_as_escaping_and_reported_literally() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run plain.helper\nthen save to \"~/out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let save = rows_of_kind(&object, "save");
    assert_eq!(save[0]["escapes_cwd"], true, "{object}");
    assert_eq!(save[0]["path"], "~/out.txt", "{object}");
    assert_eq!(save[0]["path_is_runtime"], false);
    assert_eq!(object["summary"]["escaping_write_paths"], 1);
}

#[test]
fn a_home_folder_makes_later_relative_writes_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let source = "use folder \"~/project\"\nrun plain.helper\nthen save to \"out.txt\"\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let folder = rows_of_kind(&object, "use_folder");
    assert_eq!(folder[0]["escapes_cwd"], true, "{object}");
    let save = rows_of_kind(&object, "save");
    assert!(
        save[0]["path"].is_null(),
        "a home folder must not be joined: {object}"
    );
    assert_eq!(save[0]["path_is_runtime"], true);
}

#[test]
fn a_privileged_invocation_is_marked_in_the_row_and_the_summary() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run \"sudo\" with arguments [\"-n\", \"true\"]\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let rows = object["steps"].as_array().unwrap();
    let invocation = rows
        .iter()
        .find(|row| row["binary"] == "sudo")
        .expect("sudo row");
    assert_eq!(invocation["privileged"], true, "{object}");
    assert_eq!(object["summary"]["privileged"], true);
    assert_eq!(object["summary"]["privileged_invocations"], 1);
}

#[test]
fn direct_arguments_are_visible() {
    let directory = tempfile::tempdir().unwrap();
    let object = plan(&explain(
        directory.path(),
        "run \"rm\" with arguments [\"-rf\", \"/home\"]\n",
        &["--json"],
    ));
    assert_eq!(
        object["steps"][0]["argv"],
        serde_json::json!(["-rf", "/home"])
    );
}

#[test]
fn sensitive_inputs_are_not_expanded_into_invocation_argv() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "accept token as text from secret prompt \"Token?\"\nuse value token\n",
        &["--json", "--input", "token=private-canary-462"],
    );
    let object = plan(&output);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-canary-462"));
    assert_eq!(object["steps"][0]["kind"], "use_value");
    assert!(object["steps"][0]["argv"].is_null());
}

#[test]
fn public_environment_inputs_are_expanded_but_sensitive_ones_are_withheld() {
    let directory = tempfile::tempdir().unwrap();
    let public_value = "/public-home-explain";
    let public = explain(
        directory.path(),
        "accept home_dir as text from public environment \"HOME\"\nrun \"printf\" with arguments [\"%s\", home_dir]\n",
        &["--json", "--input", "home_dir=/public-home-explain"],
    );
    let public_plan = plan(&public);
    assert_eq!(
        public_plan["steps"][0]["argv"],
        serde_json::json!(["%s", public_value])
    );
    assert!(String::from_utf8_lossy(&public.stdout).contains(public_value));

    let secret_value = "/secret-home-explain";
    let secret = explain(
        directory.path(),
        "accept home_dir as text from environment \"HOME\"\nrun \"printf\" with arguments [\"%s\", home_dir]\n",
        &["--json", "--input", "home_dir=/secret-home-explain"],
    );
    assert!(!secret.status.success());
    assert!(!String::from_utf8_lossy(&secret.stdout).contains(secret_value));
    assert!(!String::from_utf8_lossy(&secret.stderr).contains(secret_value));
}

#[test]
fn nested_sensitive_and_unresolved_inputs_remain_safe() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept token as text from secret prompt \"Token?\"\naccept name as text\nuse value token\n";
    let output = explain(
        directory.path(),
        source,
        &["--json", "--input", "token=private-canary-462"],
    );
    let object = plan(&output);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-canary-462"));
    assert_eq!(object["steps"][0]["kind"], "use_value");
    assert!(object["steps"][0]["argv"].is_null());
}

#[test]
fn runtime_binding_remains_an_unresolved_reference() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept name as text\nuse value \"changed\" as generated\nrun plain.helper with text generated\n";
    let object = plan(&explain(
        directory.path(),
        source,
        &["--json", "--input", "name=original"],
    ));
    let row = object["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "invoke")
        .unwrap();
    assert_eq!(
        row["parameters"]["text"],
        serde_json::json!({"ref":"generated"})
    );
    assert_eq!(row["argv"][2], serde_json::json!({"ref":"generated"}));
}

#[cfg(unix)]
#[test]
fn sensitive_explain_creates_a_private_review_key() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path());
    let source = "accept token as text from secret prompt \"Token?\"\nuse value token\n";
    std::fs::write(directory.path().join("plan.program"), source).unwrap();
    let state = directory.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "explain",
            "plan.program",
            "--json",
            "--input",
            "token=private-canary-462",
            "--manifest-dir",
        ])
        .arg(manifests)
        .current_dir(directory.path())
        .env("XDG_STATE_HOME", &state)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let key = state.join("execute/what-if.key");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(key).unwrap().permissions().mode() & 0o077,
        0
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-canary-462"));
}

#[test]
fn literal_record_keys_cannot_be_confused_with_markers() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run nested.helper with data {\"ref\": \"ordinary\", \"sensitive\": true, \"record\": {\"ref\": \"inner\"}}\n";
    let object = plan(&explain(directory.path(), source, &["--json"]));
    let row = &object["steps"][0];
    assert_eq!(
        row["parameters"]["data"],
        serde_json::json!({"record":{"ref":"ordinary","sensitive":true,"record":{"record":{"ref":"inner"}}}})
    );
    let actual: Value = serde_json::from_str(
        row["argv"][0]
            .as_str()
            .expect("fully resolved native JSON argument"),
    )
    .unwrap();
    assert_eq!(
        actual,
        serde_json::json!({"ref":"ordinary","sensitive":true,"record":{"ref":"inner"}})
    );
}

#[test]
fn optional_arguments_are_emitted_only_when_present_even_with_unresolved_values() {
    let directory = tempfile::tempdir().unwrap();
    let absent = plan(&explain(
        directory.path(),
        "run plain.helper\n",
        &["--json"],
    ));
    assert_eq!(absent["steps"][0]["argv"], serde_json::json!(["--version"]));
    assert_eq!(absent["steps"][0]["parameters"], serde_json::json!({}));
    let unresolved = plan(&explain(
        directory.path(),
        "accept name as text\nrun plain.helper with text name\n",
        &["--json"],
    ));
    assert_eq!(
        unresolved["steps"][0]["argv"],
        serde_json::json!(["--version", "--text", {"ref":"name"}])
    );
    let literal = plan(&explain(
        directory.path(),
        "run plain.helper with text \"x\"\n",
        &["--json"],
    ));
    assert_eq!(
        literal["steps"][0]["argv"],
        serde_json::json!(["--version", "--text", "x"])
    );
}

#[test]
fn resolved_json_input_with_marker_keys_is_a_literal_record() {
    let directory = tempfile::tempdir().unwrap();
    let output = explain(
        directory.path(),
        "accept payload as json\nrun nested.helper with data payload\n",
        &[
            "--json",
            "--input",
            "payload={\"ref\":\"x\",\"sensitive\":true}",
        ],
    );
    let object = plan(&output);
    let row = &object["steps"][0];
    assert_eq!(
        row["parameters"]["data"],
        serde_json::json!({"record":{"ref":"x","sensitive":true}})
    );
    let native: Value = serde_json::from_str(row["argv"][0].as_str().unwrap()).unwrap();
    assert_eq!(native, serde_json::json!({"ref":"x","sensitive":true}));
}
