#![cfg(unix)]
//! Static diagnostics are one JSON document, with no application execution.
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::ffi::OsStrExt,
    path::Path,
    process::{Command, Output},
};

fn invoke(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .current_dir(root)
        .output()
        .unwrap()
}
fn report(output: &Output, ok: bool) -> Value {
    assert_eq!(
        output.status.success(),
        ok,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["protocol"], "execute.check/1");
    assert_eq!(value["validation"], "static");
    assert_eq!(value["ok"], ok);
    value
}

#[test]
fn valid_check_json_is_static_and_deterministic_with_missing_inputs_and_tools() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("job.program"), "accept path as text\nrun \"execute-diagnostic-nonexistent-tool\" with arguments [path]\nthen save to \"forbidden\"\n").unwrap();
    let first = invoke(root.path(), &["check", "job.program", "--json"]);
    let value = report(&first, true);
    assert_eq!(value["diagnostics"], json!([]));
    assert_eq!(value["path"], "job.program");
    assert_eq!(value["path_bytes"], json!(b"job.program".to_vec()));
    assert_eq!(
        first.stdout,
        invoke(root.path(), &["check", "job.program", "--json"]).stdout
    );
    assert!(!root.path().join("forbidden").exists());
}

#[test]
fn failures_in_all_execution_forms_are_structured_without_side_effects() {
    let root = tempfile::tempdir().unwrap();
    for source in [
        "run \"printf\" with arguments [\"before\"]\nthen save to \"forbidden\"\nread \"unterminated\n",
        "every 0 seconds\n  run \"printf\"\n",
        "stream from \"printf\"\nshow each line\npipe to \"cat\"\n",
        "open a screen titled \"broken\"\nplace pane on the left\ndefine a program named pane\n  unknown widget\n",
    ] {
        fs::write(root.path().join("broken.program"), source).unwrap();
        let output = invoke(root.path(), &["check", "broken.program", "--json"]);
        let value = report(&output, false);
        assert_eq!(value["diagnostics"].as_array().unwrap().len(), 1);
        assert_eq!(value["diagnostics"][0]["code"], "check_failed");
        assert_eq!(value["diagnostics"][0]["severity"], "error");
        assert!(!value["diagnostics"][0]["message"].as_str().unwrap().is_empty());
        assert!(!value["diagnostics"][0]["causes"].as_array().unwrap().is_empty());
        assert!(!root.path().join("forbidden").exists());
    }
}

#[test]
fn read_manifest_and_input_errors_share_the_same_framed_protocol() {
    let root = tempfile::tempdir().unwrap();
    let missing = report(
        &invoke(root.path(), &["check", "missing.program", "--json"]),
        false,
    );
    assert!(missing["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("could not read workflow"));
    fs::write(
        root.path().join("job.program"),
        "accept count as integer\nrun \"printf\" with arguments [count]\n",
    )
    .unwrap();
    let input = report(
        &invoke(
            root.path(),
            &["check", "job.program", "--json", "--input", "count=oops"],
        ),
        false,
    );
    assert!(input["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("integer"));
    fs::write(root.path().join("invalid.json"), "{bad manifest}").unwrap();
    let manifest = report(
        &invoke(
            root.path(),
            &[
                "check",
                "job.program",
                "--json",
                "--manifest-dir",
                "invalid.json",
            ],
        ),
        false,
    );
    assert!(manifest["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("manifest"));
}

#[test]
fn json_file_uri_preserves_non_utf8_path_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join(std::ffi::OsString::from_vec(
        b"name with space-\xff.program".to_vec(),
    ));
    // APFS rejects invalid UTF-8 names. The diagnostic must still preserve
    // requested path bytes when reading it fails; byte-permitting filesystems
    // also exercise successful validation through the identical URI.
    let readable = match fs::write(&file, "run \"printf\"\n") {
        Ok(()) => true,
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => false,
        Err(error) => panic!("creating owned non-UTF-8 fixture: {error}"),
    };
    let uri = format!(
        "file://{}",
        file.as_os_str()
            .as_bytes()
            .iter()
            .map(|byte| if *byte == b'/' {
                "/".to_owned()
            } else {
                format!("%{byte:02X}")
            })
            .collect::<String>()
    );
    let value = report(&invoke(root.path(), &["check", &uri, "--json"]), readable);
    assert_eq!(value["path_bytes"], json!(file.as_os_str().as_bytes()));
    assert!(value["path"].as_str().unwrap().contains('\u{fffd}'));
}

#[test]
fn human_errors_and_option_rejection_remain_unchanged() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("broken.program"), "read \"unterminated\n").unwrap();
    let human = invoke(root.path(), &["check", "broken.program"]);
    assert!(!human.status.success());
    assert!(human.stdout.is_empty());
    let structured = report(
        &invoke(root.path(), &["check", "broken.program", "--json"]),
        false,
    );
    assert_eq!(
        String::from_utf8(human.stderr).unwrap(),
        format!(
            "execute: {}\n",
            structured["diagnostics"][0]["message"].as_str().unwrap()
        )
    );
    for args in [
        vec!["run", "broken.program", "--json"],
        vec!["inspect", "broken.program", "--json"],
        vec!["check", "broken.program", "--json", "--json"],
    ] {
        let output = invoke(root.path(), &args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
