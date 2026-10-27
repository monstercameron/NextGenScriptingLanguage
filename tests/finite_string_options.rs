//! Finite runtime string options are declared in manifests so `check` can reject
//! known-invalid values before an applet runs.
use flow::manifest::Registry;
use serde_json::json;
use std::{fs, path::Path, process::Command};

fn check(root: &Path, source: &str) -> std::process::Output {
    let program = root.join("enum-options.program");
    fs::write(&program, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("check")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
        .current_dir(root)
        .output()
        .unwrap()
}

#[test]
fn check_rejects_unsupported_finite_string_options() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        (
            "run number.format_size with value 100 and system \"us\"\n",
            "system",
        ),
        (
            "run text.pad with text \"x\" and width 3 and side \"center\"\n",
            "side",
        ),
        (
            "run time.diff with left 10 and right 0 and unit \"weeks\"\n",
            "unit",
        ),
        (
            "run time.add with epoch 0 and amount 1 and unit \"weeks\"\n",
            "unit",
        ),
        (
            "run text.trim with text \" x \" and mode \"middle\"\n",
            "mode",
        ),
        (
            "run wait.until with probe \"hello\" and timeout_ms 0\n",
            "probe",
        ),
        (
            "run text.format with template \"{{x}}\" and values {\"x\":\"a\"} and escape \"bogus\"\n",
            "escape",
        ),
        (
            "run text.length with text \"abc\" and unit \"words\"\n",
            "unit",
        ),
        (
            "run text.substring with text \"abc\" and start 0 and length 1 and unit \"words\"\n",
            "unit",
        ),
        (
            "run jsonc.edit_content with content \"{}\" and action \"replace\" and key \"x\"\n",
            "action",
        ),
        (
            "run fs.write_atomic with path \"/tmp/execute-audit-do-not-create\" and expected_content_hash \"0000000000000000000000000000000000000000000000000000000000000000\" and symlink_policy \"wrong\" and permissions_policy \"preserve\" and content \"x\"\n",
            "symlink_policy",
        ),
        (
            "run fs.write_atomic with path \"/tmp/execute-audit-do-not-create\" and expected_content_hash \"0000000000000000000000000000000000000000000000000000000000000000\" and symlink_policy \"preserve\" and permissions_policy \"wrong\" and content \"x\"\n",
            "permissions_policy",
        ),
        (
            "run fs.transaction.recover with journal \"/tmp/execute-audit-no-journal\" and decision \"erase\"\n",
            "decision",
        ),
    ];
    for (source, parameter) in cases {
        let output = check(root.path(), source);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(parameter),
            "accepted invalid {parameter} in {source:?}: {stderr}"
        );
    }
}

#[test]
fn check_accepts_every_declared_finite_string_option() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        ("run number.format_size with value 100 and system ", &["si", "iec"] as &[_]),
        ("run text.pad with text \"x\" and width 3 and side ", &["left", "right"]),
        ("run time.diff with left 10 and right 0 and unit ", &["seconds", "minutes", "hours", "days"]),
        ("run time.add with epoch 0 and amount 1 and unit ", &["seconds", "minutes", "hours", "days"]),
        ("run text.trim with text \" x \" and mode ", &["both", "start", "end"]),
        ("run text.format with template \"{{x}}\" and values {\"x\":\"a\"} and escape ", &["plain", "json", "shell", "url", "csv", "html", "regex"]),
        ("run text.length with text \"abc\" and unit ", &["chars", "bytes"]),
        ("run text.substring with text \"abc\" and start 0 and length 1 and unit ", &["chars", "bytes"]),
        (
            "run jsonc.edit_content with content \"{}\" and action ",
            &["set", "remove"],
        ),
        ("run wait.until with probe ", &["file_exists", "file_readable", "tcp_port_open"]),
        ("run fs.write_atomic with path \"/tmp/not-written\" and expected_content_hash \"0000000000000000000000000000000000000000000000000000000000000000\" and symlink_policy ", &["preserve", "reject", "replace"]),
        ("run fs.write_atomic with path \"/tmp/not-written\" and expected_content_hash \"0000000000000000000000000000000000000000000000000000000000000000\" and symlink_policy \"preserve\" and permissions_policy ", &["preserve", "private", "exact"]),
        ("run fs.transaction.recover with journal \"journal.json\" and decision ", &["complete", "rollback"]),
    ];
    for (prefix, values) in cases {
        for value in values {
            let suffix = if prefix.contains("wait.until") {
                format!("\"{value}\" and path \"not-present\" and host \"127.0.0.1\" and port 0 and timeout_ms 0\n")
            } else if prefix.contains("jsonc.edit_content") {
                format!("\"{value}\" and key \"x\"\n")
            } else if prefix.contains("symlink_policy") && !prefix.contains("permissions_policy") {
                format!("\"{value}\" and permissions_policy \"preserve\" and content \"x\"\n")
            } else {
                format!("\"{value}\"\n")
            };
            let source = format!("{prefix}{suffix}");
            let output = check(root.path(), &source);
            assert!(
                output.status.success(),
                "rejected declared choice {value:?} in {source:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn projected_parameter_enums_match_existing_contract_port_enums() {
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    let cases = [
        ("osd.brightness", "action", json!(["raise", "lower"])),
        (
            "osd.volume",
            "action",
            json!(["raise", "lower", "mute-toggle"]),
        ),
        (
            "notify.send",
            "urgency",
            json!(["low", "normal", "critical"]),
        ),
        ("fs.ensure_line", "state", json!(["present", "absent"])),
        (
            "fs.test",
            "kind",
            json!([
                "exists",
                "file",
                "directory",
                "symlink",
                "readable",
                "writable",
                "executable",
                "empty"
            ]),
        ),
    ];
    for (applet, parameter, expected) in cases {
        let manifest = registry.resolve(applet).unwrap();
        let schema =
            manifest.contract()["parameters_schema"]["properties"][parameter]["enum"].clone();
        assert_eq!(schema, expected, "{applet}.{parameter}");
    }
}
