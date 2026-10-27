//! Static diagnostics are one JSON document, with no application execution.
#[path = "common/diagnostic_corpus.rs"]
mod diagnostic_corpus;

use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::{
    fs,
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

#[cfg(unix)]
fn invoke_with_path(root: &Path, path: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .current_dir(root)
        .env("PATH", path)
        .output()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn resolve_checks_direct_executables_only_and_suggests_path_names() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let printf = bin.join("printf");
    fs::write(
        &printf,
        "placeholder; resolution must not execute this file\n",
    )
    .unwrap();
    fs::set_permissions(&printf, fs::Permissions::from_mode(0o755)).unwrap();
    let path = bin.to_str().unwrap();

    fs::write(
        root.path().join("job.program"),
        "run \"prinft\" with arguments [\"x\"]\n",
    )
    .unwrap();
    let unchanged = invoke_with_path(root.path(), &bin, &["check", "job.program", "--json"]);
    assert_eq!(report(&unchanged, true)["diagnostics"], json!([]));

    let resolved = invoke_with_path(
        root.path(),
        &bin,
        &["check", "job.program", "--resolve", "--json"],
    );
    let value = report(&resolved, true);
    let diagnostic = &value["diagnostics"][0];
    assert_eq!(diagnostic["code"], "exec.not_found");
    assert_eq!(diagnostic["severity"], "warning");
    assert!(diagnostic["message"]
        .as_str()
        .unwrap()
        .contains("did you mean \"printf\""));
    assert!(diagnostic["message"].as_str().unwrap().contains(path));
    assert_eq!(diagnostic["fixes"][0]["edits"][0]["replacement"], "printf");

    let denied = invoke_with_path(
        root.path(),
        &bin,
        &[
            "check",
            "job.program",
            "--resolve",
            "--deny-warnings",
            "--json",
        ],
    );
    let denied = report(&denied, false);
    assert_eq!(denied["diagnostics"][0]["severity"], "warning");

    let run = invoke_with_path(root.path(), &bin, &["run", "job.program"]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("did you mean \"printf\""));

    fs::set_permissions(&printf, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(root.path().join("job.program"), "run \"printf\"\n").unwrap();
    let not_executable = invoke_with_path(
        root.path(),
        &bin,
        &["check", "job.program", "--resolve", "--json"],
    );
    let not_executable = report(&not_executable, true);
    assert_eq!(not_executable["diagnostics"][0]["code"], "exec.not_found");
    assert_eq!(not_executable["diagnostics"][0]["severity"], "warning");

    fs::write(
        root.path().join("job.program"),
        "run text.concat with parts [\"a\", \"b\"]\n",
    )
    .unwrap();
    let named_applet = invoke_with_path(
        root.path(),
        &bin,
        &["check", "job.program", "--resolve", "--json"],
    );
    assert_eq!(report(&named_applet, true)["diagnostics"], json!([]));

    fs::write(
        root.path().join("job.program"),
        "run \"text.conct\" with arguments [\"a\"]\n",
    )
    .unwrap();
    let applet_hint = invoke_with_path(
        root.path(),
        &bin,
        &["check", "job.program", "--resolve", "--json"],
    );
    let applet_hint = report(&applet_hint, true);
    let diagnostic = &applet_hint["diagnostics"][0];
    assert!(diagnostic["message"]
        .as_str()
        .unwrap()
        .contains("catalog applet \"text.concat\""));
    assert!(diagnostic.get("fixes").is_none());

    let subdir_bin = root.path().join("subdir").join("bin");
    fs::create_dir_all(&subdir_bin).unwrap();
    let subdir_printf = subdir_bin.join("printf");
    fs::write(&subdir_printf, "placeholder\n").unwrap();
    fs::set_permissions(&subdir_printf, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        root.path().join("job.program"),
        "use folder \"subdir\"\nrun \"printf\"\n",
    )
    .unwrap();
    let relative_path = invoke_with_path(
        root.path(),
        Path::new("bin"),
        &["check", "job.program", "--resolve", "--json"],
    );
    assert_eq!(report(&relative_path, true)["diagnostics"], json!([]));

    fs::set_permissions(&printf, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&subdir_printf, fs::Permissions::from_mode(0o644)).unwrap();
    let wrong_initial_cwd = invoke_with_path(
        root.path(),
        Path::new("bin"),
        &["check", "job.program", "--resolve", "--json"],
    );
    assert_eq!(
        report(&wrong_initial_cwd, true)["diagnostics"][0]["code"],
        "exec.not_found"
    );
    fs::set_permissions(&subdir_printf, fs::Permissions::from_mode(0o755)).unwrap();

    fs::write(
        root.path().join("job.program"),
        "use folder \"subdir\"\nrun \"prinft\" with arguments [\"x\"]\n",
    )
    .unwrap();
    let relative_suggestion = invoke_with_path(
        root.path(),
        Path::new("bin"),
        &["check", "job.program", "--resolve", "--json"],
    );
    let relative_suggestion = report(&relative_suggestion, true);
    assert!(relative_suggestion["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("did you mean \"printf\""));
    let relative_run = invoke_with_path(root.path(), Path::new("bin"), &["run", "job.program"]);
    assert!(!relative_run.status.success());
    assert!(String::from_utf8_lossy(&relative_run.stderr).contains("did you mean \"printf\""));

    fs::write(
        root.path().join("job.program"),
        "define a program named helper\n  run \"prinft\" with arguments [\"x\"]\nrun program helper\n",
    )
    .unwrap();
    fs::set_permissions(&printf, fs::Permissions::from_mode(0o755)).unwrap();
    let in_definition = invoke_with_path(
        root.path(),
        &bin,
        &["check", "job.program", "--resolve", "--json"],
    );
    let in_definition = report(&in_definition, true);
    assert_eq!(in_definition["diagnostics"][0]["code"], "exec.not_found");
}

#[cfg(unix)]
#[test]
fn resolve_does_not_apply_called_file_fixes_to_the_caller_source() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let printf = bin.join("printf");
    fs::write(
        &printf,
        "placeholder; resolution must not execute this file\n",
    )
    .unwrap();
    fs::set_permissions(&printf, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        root.path().join("job.program"),
        "run program file \"helper.program\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("helper.program"),
        "run \"prinft\" with arguments [\"x\"]\n",
    )
    .unwrap();

    let output = invoke_with_path(
        root.path(),
        &bin,
        &["check", "job.program", "--resolve", "--json"],
    );
    let value = report(&output, true);
    assert!(
        value["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|diagnostic| diagnostic["code"] != "exec.not_found"),
        "called-file diagnostics need a child-source URI/span before they can be emitted: {value}"
    );
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
fn deny_warnings_fails_check_and_keeps_warning_severity() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("job.program"), "run \"ls *.txt\"\n").unwrap();

    let ordinary = report(
        &invoke(root.path(), &["check", "job.program", "--json"]),
        true,
    );
    assert_eq!(
        ordinary["diagnostics"][0]["code"],
        "lint.executable_name_whitespace"
    );
    assert_eq!(ordinary["diagnostics"][0]["severity"], "warning");

    let denied = report(
        &invoke(
            root.path(),
            &["check", "job.program", "--json", "--deny-warnings"],
        ),
        false,
    );
    assert_eq!(denied["diagnostics"], ordinary["diagnostics"]);
}

#[test]
fn failures_in_all_execution_forms_are_structured_without_side_effects() {
    let root = tempfile::tempdir().unwrap();
    for (source, expected_code, expected_line) in [
        (
            "run \"printf\" with arguments [\"before\"]\nthen save to \"forbidden\"\nread \"unterminated\n",
            "parse.lexical",
            Some(3),
        ),
        (
            "every 0 seconds\n  run \"printf\"\n",
            "check.range",
            Some(1),
        ),
        (
            "stream from \"printf\"\nshow each line\npipe to \"cat\"\n",
            "check_failed",
            None,
        ),
        #[cfg(feature = "screens")]
        (
            "open a screen titled \"broken\"\nplace pane on the left\ndefine a program named pane\n  unknown widget\n",
            "check_failed",
            None,
        ),
    ] {
        fs::write(root.path().join("broken.program"), source).unwrap();
        let output = invoke(root.path(), &["check", "broken.program", "--json"]);
        let value = report(&output, false);
        assert_eq!(value["diagnostics"].as_array().unwrap().len(), 1);
        let diagnostic = &value["diagnostics"][0];
        assert_eq!(diagnostic["code"], expected_code);
        assert_eq!(diagnostic["severity"], "error");
        assert!(!diagnostic["message"].as_str().unwrap().is_empty());
        match expected_line {
            Some(line) => {
                assert_eq!(diagnostic["span"]["line"], line);
                let precision = if expected_code == "check.range" {
                    "token"
                } else {
                    "line"
                };
                assert_eq!(diagnostic["span"]["precision"], precision);
                assert!(diagnostic.get("causes").is_none());
            }
            None => assert!(!diagnostic["causes"].as_array().unwrap().is_empty()),
        }
        assert!(!root.path().join("forbidden").exists());
    }
}

#[test]
fn representative_invalid_programs_keep_specific_cli_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let manifests = diagnostic_corpus::write_manifests(root.path());
    let manifest_arg = manifests.to_str().unwrap();
    let cases = diagnostic_corpus::representative_invalid_sources();
    assert!(cases.len() >= 100);

    for case in cases {
        fs::write(root.path().join("representative.program"), &case.source).unwrap();
        let output = invoke(
            root.path(),
            &[
                "check",
                "representative.program",
                "--json",
                "--manifest-dir",
                manifest_arg,
            ],
        );
        let value = report(&output, false);
        let diagnostics = value["diagnostics"].as_array().unwrap();
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic["code"] != "check_failed"),
            "generic CLI diagnostic for {:?}: {value}",
            case.source
        );
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic["code"] == case.expected_code)
            .unwrap_or_else(|| {
                panic!(
                    "expected {} for {:?}: {value}",
                    case.expected_code, case.source
                )
            });
        assert_eq!(diagnostic["severity"], "error");
        let span = &diagnostic["span"];
        assert_eq!(span["precision"], "token");
        let start = span["start_byte"].as_u64().unwrap() as usize;
        let end = span["end_byte"].as_u64().unwrap() as usize;
        assert_eq!(
            &case.source[start..end],
            case.expected_token,
            "{:?}: {value}",
            case.source
        );
    }
}

#[test]
fn condition_text_integer_mismatch_keeps_a_precise_cli_span() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("condition.program"),
        "accept out as text\nif out contains 3\n  run \"true\"\n",
    )
    .unwrap();
    let output = invoke(root.path(), &["check", "condition.program", "--json"]);
    let value = report(&output, false);
    let diagnostics = value["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic["code"] != "check_failed"),
        "generic CLI diagnostic: {value}"
    );
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "check.type_mismatch")
        .unwrap_or_else(|| panic!("expected condition mismatch: {value}"));
    assert_eq!(diagnostic["severity"], "error");
    let span = &diagnostic["span"];
    assert_eq!(span["precision"], "token");
    let start = span["start_byte"].as_u64().unwrap() as usize;
    let end = span["end_byte"].as_u64().unwrap() as usize;
    assert_eq!(
        &"accept out as text\nif out contains 3\n  run \"true\"\n"[start..end],
        "out"
    );
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
fn check_json_defers_missing_and_invalid_catalog_errors_until_authoritative_validation() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("valid.program"), "run \"printf\"\n").unwrap();
    fs::write(
        root.path().join("broken.program"),
        "run \"printf\"\nread \"unterminated\n",
    )
    .unwrap();
    fs::write(root.path().join("invalid.json"), "{bad manifest}").unwrap();

    for manifest in ["missing-manifests", "invalid.json"] {
        let valid = report(
            &invoke(
                root.path(),
                &[
                    "check",
                    "valid.program",
                    "--json",
                    "--manifest-dir",
                    manifest,
                ],
            ),
            false,
        );
        assert!(valid["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("could not load manifests"));

        let broken = report(
            &invoke(
                root.path(),
                &[
                    "check",
                    "broken.program",
                    "--json",
                    "--manifest-dir",
                    manifest,
                ],
            ),
            false,
        );
        let message = broken["diagnostics"][0]["message"].as_str().unwrap();
        assert!(message.contains("could not parse workflow"), "{message}");
        assert!(!message.contains("could not load manifests"), "{message}");
    }
}

#[cfg(unix)]
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
    // The human CLI retains the established contextual parser error. Structured
    // static diagnostics intentionally use a concise message plus source span.
    assert_eq!(
        String::from_utf8(human.stderr).unwrap(),
        "execute: could not parse workflow broken.program: line 1: unterminated quoted string\n"
    );
    assert_eq!(structured["diagnostics"][0]["code"], "parse.lexical");
    assert_eq!(
        structured["diagnostics"][0]["message"],
        "unterminated quoted string"
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

#[test]
fn called_file_diagnostics_keep_their_own_source_and_call_site() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("sub")).unwrap();
    fs::write(root.path().join("main.program"),
        "accept theme as text with default \"dark\"\nrun program file \"sub/child.program\" with theme theme as result\n").unwrap();
    fs::write(root.path().join("sub/child.program"),
        "accept theme as text\nrun \"printf\" with arguments [them]\n").unwrap();
    let result = report(&invoke(root.path(), &["check", "main.program", "--json"]), false);
    let diagnostic = &result["diagnostics"][0];
    assert_eq!(diagnostic["code"], "check.undefined_name");
    assert_eq!(diagnostic["span"]["path"], root.path().join("sub/child.program").canonicalize().unwrap().display().to_string());
    assert_eq!(diagnostic["span"]["line"], 2);
    assert_eq!(diagnostic["span"]["precision"], "token");
    assert!(diagnostic["message"].as_str().unwrap().contains("did you mean \"theme\""));
    assert_eq!(diagnostic["fixes"][0]["edits"][0]["span"]["path"], diagnostic["span"]["path"]);
    assert_eq!(diagnostic["related"][0]["span"]["path"], root.path().join("main.program").canonicalize().unwrap().display().to_string());
    assert_eq!(diagnostic["related"][0]["span"]["line"], 2);
}

#[test]
fn nested_called_file_diagnostics_report_the_innermost_file() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("sub")).unwrap();
    fs::write(root.path().join("main.program"), "run program file \"sub/middle.program\"\n").unwrap();
    fs::write(root.path().join("sub/middle.program"), "run program file \"leaf.program\"\n").unwrap();
    fs::write(root.path().join("sub/leaf.program"), "read \"unterminated\n").unwrap();
    let result = report(&invoke(root.path(), &["check", "main.program", "--json"]), false);
    let diagnostic = &result["diagnostics"][0];
    assert_eq!(diagnostic["code"], "parse.lexical");
    assert_eq!(diagnostic["span"]["path"], root.path().join("sub/leaf.program").canonicalize().unwrap().display().to_string());
    assert_eq!(diagnostic["span"]["line"], 1);
}

#[test]
fn called_file_parse_errors_report_the_callee_line_and_reason() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("main.program"),
        "run program file \"callee.program\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("callee.program"),
        "run \"printf\"\nrun \"printf\"\nrun \"printf\"\nrun\n",
    )
    .unwrap();
    let result = report(&invoke(root.path(), &["check", "main.program", "--json"]), false);
    let diagnostic = &result["diagnostics"][0];
    assert_eq!(diagnostic["code"], "parse.syntax");
    assert_eq!(diagnostic["span"]["path"], root.path().join("callee.program").canonicalize().unwrap().display().to_string());
    assert_eq!(diagnostic["span"]["line"], 4);
    assert!(!diagnostic["message"]
        .as_str()
        .unwrap()
        .contains("called program file"));
    assert_eq!(diagnostic["related"][0]["span"]["line"], 1);
}
