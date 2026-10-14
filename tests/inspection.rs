#![cfg(unix)]
//! Agent discovery must never execute application steps or require missing inputs.
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn inspect(root: &Path, source: &str, args: &[&str]) -> Output {
    let file = root.join("job.program");
    fs::write(&file, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("inspect")
        .arg(file)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

fn decoded(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn discovers_required_inputs_and_native_json_defaults_without_probing_tools() {
    let root = tempfile::tempdir().unwrap();
    let source = "# interface\naccept path as text\naccept count as integer with default -2\naccept enabled as boolean with default false\naccept files as list of text with default [\"a file\", \"$HOME\"]\nrun \"execute-inspection-nonexistent-command\"\nthen save to \"forbidden\"\n";
    let report = decoded(inspect(root.path(), source, &[]));
    assert_eq!(report["protocol"], "execute.inspect/1");
    assert_eq!(report["validation"], "static");
    assert_eq!(report["kind"], "finite");
    assert_eq!(report["stdin_mode"], "unused");
    assert_eq!(report["missing_inputs"], json!(["path"]));
    assert_eq!(report["inputs_complete"], false);
    assert_eq!(
        report["inputs"],
        json!([
            {"name":"path", "type":"text", "required":true, "line":2},
            {"name":"count", "type":"integer", "required":false, "line":3, "default":-2, "value":-2},
            {"name":"enabled", "type":"boolean", "required":false, "line":4, "default":false, "value":false},
            {"name":"files", "type":"list of text", "required":false, "line":5, "default":["a file","$HOME"], "value":["a file","$HOME"]}
        ])
    );
    assert!(!root.path().join("forbidden").exists());
    assert_eq!(report, decoded(inspect(root.path(), source, &[])));
}

#[test]
fn supplied_values_override_defaults_without_losing_the_declared_contract() {
    let root = tempfile::tempdir().unwrap();
    let source = "accept message as text\naccept count as integer with default 20\naccept enabled as boolean with default true\naccept files as list of text with default [\"default\"]\nrun \"printf\" with arguments [message, count, enabled]\n";
    let report = decoded(inspect(
        root.path(),
        source,
        &[
            "--input",
            "message=",
            "--input",
            "count=-9223372036854775808",
            "--input",
            "enabled=false",
            "--input",
            "files=[]",
        ],
    ));
    assert_eq!(report["missing_inputs"], json!([]));
    assert_eq!(report["inputs_complete"], true);
    assert_eq!(report["inputs"][0]["value"], "");
    assert_eq!(report["inputs"][0]["required"], true);
    assert_eq!(report["inputs"][1]["value"], i64::MIN);
    assert_eq!(report["inputs"][1]["default"], 20);
    assert_eq!(report["inputs"][2]["value"], false);
    assert_eq!(report["inputs"][3]["value"], json!([]));
}

#[test]
fn discovers_all_noninteractive_lifetimes_without_waiting_or_launching_producers() {
    let root = tempfile::tempdir().unwrap();
    let report = decoded(inspect(
        root.path(),
        "listen to stdin\nwhenever a line arrives as event\n  then \"cat\"\n",
        &[],
    ));
    assert_eq!(report["stdin_mode"], "utf8_lines");
    for (kind, body) in [
        ("interval", "every 1 hour\n  run \"sh\" with arguments [\"-c\", \"touch forbidden\"]\n"),
        ("calendar", "on schedule \"0 0 * * *\"\n  run \"sh\" with arguments [\"-c\", \"touch forbidden\"]\n"),
        ("listener", "listen to \"sh\" with arguments [\"-c\", \"touch forbidden\"]\nwhenever a line arrives as event\n  then \"cat\"\n"),
        ("stream", "stream from \"sh\" with arguments [\"-c\", \"touch forbidden\"]\ntake the first 2 lines\nshow each line\n"),
    ] {
        let source = format!("accept path as text\n{body}");
        let report = decoded(inspect(root.path(), &source, &[]));
        assert_eq!(report["kind"], kind);
        assert_eq!(report["missing_inputs"], json!(["path"]));
        if kind == "stream" { assert_eq!(report["line_limit"], 2); }
        assert!(!root.path().join("forbidden").exists());
    }
}

#[test]
fn screen_ports_are_distinct_from_root_cli_inputs_and_open_handlers_never_run() {
    let root = tempfile::tempdir().unwrap();
    let source = "open a screen titled \"Inspection\"\ndivide the screen into two columns\nplace sender on the left\nplace receiver on the right\nconnect message from sender to message in receiver\ndefine a program named sender\n  offer message as text\n  show a heading named label with text \"Sender\"\n  whenever the program opens\n    run \"sh\" with arguments [\"-c\", \"touch forbidden\"]\ndefine a program named receiver\n  accept message as text\n  show a text panel named output with text \"Waiting\"\n  whenever message arrives\n    show the result in output\n";
    let report = decoded(inspect(root.path(), source, &[]));
    assert_eq!(report["kind"], "screen");
    assert_eq!(report["stdin_mode"], "terminal");
    assert_eq!(report["inputs"], json!([]));
    assert_eq!(report["placements"], json!(["sender", "receiver"]));
    assert_eq!(
        report["components"],
        json!([
            {"name":"receiver","inputs":[{"name":"message","type":"text"}],"outputs":[]},
            {"name":"sender","inputs":[],"outputs":[{"name":"message","type":"text"}]}
        ])
    );
    assert!(!root.path().join("forbidden").exists());
    assert!(!inspect(root.path(), source, &["--input", "message=x"])
        .status
        .success());
}

#[test]
fn invalid_interfaces_and_execution_options_fail_without_partial_json_or_effects() {
    let root = tempfile::tempdir().unwrap();
    let prefix = "run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\n";
    for (source, args) in [
        (format!("{prefix}run nonexistent_named_applet\n"), vec![]),
        (format!("accept path as unknown\n{prefix}"), vec![]),
        (
            format!("accept count as integer\n{prefix}"),
            vec!["--input", "count=no"],
        ),
        (
            format!("accept count as integer\n{prefix}"),
            vec!["--input", "unknown=x"],
        ),
        (
            format!(
                "accept files as list of text with default []\n{prefix}run cat with files files\n"
            ),
            vec![],
        ),
        (prefix.into(), vec!["--trace"]),
        (prefix.into(), vec!["--headless"]),
        (prefix.into(), vec!["--cache-dir", "forbidden-cache"]),
    ] {
        let result = inspect(root.path(), &source, &args);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!root.path().join("forbidden").exists());
        assert!(!root.path().join("forbidden-cache").exists());
    }
}

#[test]
fn inspect_accepts_file_uris_and_live_schema_exposes_declared_type_restrictions() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("a file.program");
    fs::write(&file, "run \"pwd\"\n").unwrap();
    let uri = format!("file://{}", file.display()).replace(' ', "%20");
    let report = decoded(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["inspect", &uri])
            .output()
            .unwrap(),
    );
    assert_eq!(report["kind"], "finite");
    let schema = decoded(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["live", "schema"])
            .output()
            .unwrap(),
    );
    assert!(schema["inspection"]
        .as_str()
        .unwrap()
        .contains("execute inspect"));
    assert!(schema["declared_inputs"]
        .as_str()
        .unwrap()
        .contains("cannot be deleted"));
}
