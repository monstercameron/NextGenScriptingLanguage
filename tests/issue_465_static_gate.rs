//! Static errors from `explain`, `inspect` and `help` use the same shape as
//! `check`: category `static`, the registry code and the span, with exit 1 and
//! no output on stdout.

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// The program from the issue: `if n is 3` compares the text input with an integer.
const PROGRAM: &str = "define a program named count_it
  accept path as text
  run \"printf\" with arguments [\"3\"]
run program count_it with path \"a\" as n
if n is 3
  run \"printf\" with arguments [\"three\\n\"]
";

fn run(directory: &Path, arguments: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null());
    if arguments.first() != Some(&"help") {
        // A private manifest folder keeps the run off the repository's manifests.
        let manifests = directory.join("manifests");
        std::fs::create_dir_all(&manifests).unwrap();
        let helper = serde_json::json!({
            "name": "ok.helper",
            "binary": env!("CARGO_BIN_EXE_execute"),
            "accepts_stdin": false,
            "parameters": {},
            "arguments": [{ "literal": "--version" }]
        });
        std::fs::write(manifests.join("ok.helper.json"), helper.to_string()).unwrap();
        command.arg("--manifest-dir").arg(&manifests);
    }
    command.output().unwrap()
}

fn final_json_line(stderr: &[u8]) -> Value {
    let text = String::from_utf8_lossy(stderr);
    let last = text
        .lines()
        .last()
        .unwrap_or_else(|| panic!("stderr is empty, expected a JSON object"));
    serde_json::from_str(last)
        .unwrap_or_else(|error| panic!("final stderr line is not JSON ({error}): {text}"))
}

#[test]
fn static_error_keeps_code_span_and_category_for_explain_inspect_and_help() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("count_it.program"), PROGRAM).unwrap();

    for command in ["explain", "inspect", "help"] {
        let output = run(
            directory.path(),
            &[command, "count_it.program", "--json-errors"],
        );
        assert!(
            !output.status.success(),
            "{command} accepted a program that does not check"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "",
            "{command} wrote stdout before failing; stderr: {}",
            String::from_utf8_lossy(&output.stderr).replace('\n', " | ")
        );
        let object = final_json_line(&output.stderr);
        assert_eq!(object["protocol"], "execute.error/1", "{command}");
        assert_eq!(object["category"], "static", "{command}: {object}");
        assert_eq!(object["code"], "check.type_mismatch", "{command}");
        assert_eq!(object["exit_code"], 1, "{command}");
        assert_eq!(object["span"]["line"], 5, "{command}");
        assert!(
            object["message"].as_str().unwrap().starts_with("line 5: "),
            "{command}: {object}"
        );
    }
}
