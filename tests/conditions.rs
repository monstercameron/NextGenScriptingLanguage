#![cfg(unix)]
use flow::{
    ast::{StepKind, Value},
    manifest::Registry,
    parser, runtime,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
    time::Duration,
};

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.into(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: None,
    }
}
fn run(cwd: &Path, source: &str) -> runtime::RunResult {
    runtime::run(
        &parser::parse(source).unwrap(),
        &Registry::default(),
        &options(cwd),
    )
    .unwrap()
}
fn cli(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}
fn good(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn scalar_equality_negation_and_nested_branches_run_only_selected_native_commands() {
    let dir = tempfile::tempdir().unwrap();
    for (test, wanted) in [
        ("true is true", "yes"),
        ("true is not true", "no"),
        ("-3 is -3", "yes"),
        ("7 is 0", "no"),
        ("\"a b\" is \"a b\"", "yes"),
        ("\"a\" is not \"b\"", "yes"),
    ] {
        let source=format!("if {test}\n  if 1 is 1\n    run \"printf\" with arguments [\"yes\"]\n  otherwise\n    run \"execute-never-launched\"\notherwise\n  run \"printf\" with arguments [\"no\"]\n");
        let result = run(dir.path(), &source);
        assert_eq!(result.output, wanted.as_bytes());
        assert_eq!(result.launched_processes, 1);
    }
}

#[test]
fn json_arrays_objects_and_null_use_deep_equality() {
    let dir = tempfile::tempdir().unwrap();
    for (condition, wanted) in [
        ("null is null", "yes"),
        ("[\"a\"] is [\"a\"]", "yes"),
        ("{\"items\": [1, null]} is {\"items\": [1, null]}", "yes"),
        ("{\"items\": [1, null]} is {\"items\": [1, 2]}", "no"),
        ("null is 1", "no"),
    ] {
        let source = format!(
            "if {condition}\n  run \"printf\" with arguments [\"yes\"]\notherwise\n  run \"printf\" with arguments [\"no\"]\n"
        );
        let result = run(dir.path(), &source);
        assert_eq!(result.output, wanted.as_bytes(), "{condition}");
        assert_eq!(result.launched_processes, 1, "{condition}");
    }
}

#[test]
fn status_capture_controls_program_branches_and_common_result_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let source="run \"false\" and keep status as code\nif code is 0\n  run \"printf\" with arguments [\"success\"] as message\notherwise\n  if code is 1\n    run \"printf\" with arguments [\"expected miss\"] as message\n  otherwise\n    run \"printf\" with arguments [\"other failure\"] as message\nthen \"printf\" with arguments [\"<%s>\", message]\n";
    let result = run(dir.path(), source);
    assert_eq!(result.output, b"<expected miss>");
    assert_eq!(result.launched_processes, 3);
}

#[test]
fn false_branch_leaves_unselected_reads_writes_and_missing_binaries_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let result=run(dir.path(),"run \"printf\" with arguments [\"keep bytes\"]\nif false is true\n  read \"missing file\"\n  then save to \"forbidden\"\n  run \"execute-never-launched\"\nthen \"cat\"\n");
    assert_eq!(result.output, b"keep bytes");
    assert_eq!(result.launched_processes, 2);
    assert!(!dir.path().join("forbidden").exists());
}

#[test]
fn both_branches_are_statically_validated_before_any_effect() {
    let dir = tempfile::tempdir().unwrap();
    for invalid in [
        "if true is true\n  run \"true\"\notherwise\n  run no_such_named_applet\n",
        "if true is true\n  run \"printf\" with arguments [missing]\n",
        "if true is true\n  run \"printf\" with arguments [\"x\"] as local\nthen \"printf\" with arguments [local]\n",
        "if true is true\n  run \"printf\" with arguments [\"x\"] as result\notherwise\n  run \"false\" and keep status as result\n",
        "if true is 1\n  run \"true\"\n",
        "if true is true\n  run \"false\" and keep status as code\n  then save to code\n",
        "run \"printf\" with arguments [\"x\"]\nif true is true\n  pipe to \"cat\"\n",
        "if true is true\n  run \"true\"\npipe to \"cat\"\n",
    ] {
        let source=format!("run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\n{invalid}");
        let error=runtime::run(&parser::parse(&source).unwrap(),&Registry::default(),&options(dir.path())).err().unwrap();
        assert!(!dir.path().join("forbidden").exists(),"{invalid}: {error:#}");
    }
}

#[test]
fn a_result_from_every_branch_can_supply_a_later_native_parameter() {
    let dir = tempfile::tempdir().unwrap();
    let source="accept enabled as boolean with default false\nif enabled is true\n  run \"printf\" with arguments [\"yes\"] as selected\notherwise\n  run \"printf\" with arguments [\"no\"] as selected\nthen \"printf\" with arguments [\"chosen %s\", selected]\n";
    fs::write(dir.path().join("main.program"), source).unwrap();
    for (value, wanted) in [
        ("true", b"chosen yes".as_slice()),
        ("false", b"chosen no".as_slice()),
    ] {
        let output = cli(
            dir.path(),
            &["main.program", "--input", &format!("enabled={value}")],
        );
        good(&output);
        assert_eq!(output.stdout, wanted);
    }
}

#[test]
fn selected_pipelines_keep_native_bytes_and_backpressure() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = (0..3 * 1024 * 1024)
        .map(|n| (n % 256) as u8)
        .collect::<Vec<_>>();
    fs::write(dir.path().join("bytes"), &bytes).unwrap();
    let p=parser::parse("read \"bytes\"\nif false is true\n  run \"execute-never-launched\"\notherwise\n  then \"cat\"\n  pipe to \"wc\" with arguments [\"-c\"]\n").unwrap();
    let mut opts = options(dir.path());
    opts.max_output_bytes = 32;
    let result = runtime::run(&p, &Registry::default(), &opts).unwrap();
    assert_eq!(
        String::from_utf8(result.output).unwrap().trim(),
        bytes.len().to_string()
    );
    assert_eq!(result.launched_processes, 2);
}

#[test]
fn inherited_stdin_can_pass_through_a_skipped_block_without_materializing() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.program"),
        "read from stdin\nif false is true\n  run \"true\"\nthen \"wc\" with arguments [\"-c\"]\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .args(["main.program", "--max-output-bytes", "32"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&vec![0xff; 128 * 1024])
        .unwrap();
    let output = child.wait_with_output().unwrap();
    good(&output);
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "131072");
}

#[test]
fn malformed_blocks_and_excessive_nesting_are_rejected() {
    for source in [
        "if true\n  run pwd",
        "if true is\n  run pwd",
        "if true is false extra\n  run pwd",
        "if true is true\nrun pwd",
        "otherwise\n  run pwd",
        "if true is true\n  run pwd\notherwise\nrun pwd",
        "if true is true\n  run pwd\notherwise\n    run pwd",
        "if true is true\n  accept name as text",
        "if true is true\n  every 1 second\n    run pwd",
    ] {
        assert!(parser::parse(source).is_err(), "{source}");
    }
    // A chain of `otherwise if` clauses that each have a body is complete at end of file.
    assert!(parser::parse("if true is true\n  run pwd\notherwise if true is true\n  run pwd").is_ok());
    let mut source = String::new();
    for n in 0..64 {
        source.push_str(&format!("{}if true is true\n", "  ".repeat(n)));
    }
    source.push_str(&format!("{}run pwd\n", "  ".repeat(64)));
    assert!(parser::parse(&source).is_ok());
    source = source.replacen("run pwd", "if true is true\n", 1);
    assert!(parser::parse(&source).is_err());
}

#[test]
fn malformed_public_ast_offsets_fail_validation_without_execution_or_panics() {
    let source = "if true is true\n  run \"true\"\notherwise\n  run \"false\"\n";
    let mut empty = parser::parse(source).unwrap();
    empty.steps.remove(1);
    if let StepKind::If { otherwise, end, .. } = &mut empty.steps[0].kind {
        *otherwise = 2;
        *end = 4;
    }
    assert!(runtime::validate(&empty, &Registry::default()).is_err());
    for (otherwise, end) in [(0, 0), (usize::MAX, usize::MAX), (1, 5), (3, 1000), (2, 5)] {
        let mut p = parser::parse(source).unwrap();
        if let StepKind::If {
            otherwise: other,
            end: finish,
            ..
        } = &mut p.steps[0].kind
        {
            *other = otherwise;
            *finish = end;
        }
        assert!(
            runtime::validate(&p, &Registry::default()).is_err(),
            "{otherwise}/{end}"
        );
    }
}

#[test]
fn selected_native_failure_and_timeout_never_execute_otherwise_as_recovery() {
    let dir = tempfile::tempdir().unwrap();
    for operation in ["run \"false\"", "run \"sleep\" with arguments [\"2\"]"] {
        let source=format!("if true is true\n  {operation}\notherwise\n  run \"printf\" with arguments [\"forbidden\"]\n  then save to \"forbidden\"\n");
        let mut opts = options(dir.path());
        opts.command_timeout = Some(Duration::from_millis(30));
        assert!(runtime::run(
            &parser::parse(&source).unwrap(),
            &Registry::default(),
            &opts
        )
        .is_err());
        assert!(!dir.path().join("forbidden").exists());
    }
}

#[test]
fn undeclared_host_values_require_matching_scalar_types_at_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let p = parser::parse("if setting is 1\n  run \"true\"\n").unwrap();
    let error = runtime::run_with_context(
        &p,
        &Registry::default(),
        &options(dir.path()),
        BTreeMap::from([("setting".into(), Value::String("1".into()))]),
        Vec::new(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("same type"));

    let matching = parser::parse(
        "if setting is \"1\"\n  run \"printf\" with arguments [\"same\"]\notherwise\n  run \"printf\" with arguments [\"different\"]\n",
    )
    .unwrap();
    let result = runtime::run_with_context(
        &matching,
        &Registry::default(),
        &options(dir.path()),
        BTreeMap::from([(String::from("setting"), Value::String(String::from("1")))]),
        Vec::new(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(result.output, b"same");

    // Structured host values remain eligible for deep JSON equality rather
    // than being coerced into a scalar type.
    let json = parser::parse(
        "if setting is null\n  run \"printf\" with arguments [\"null\"]\notherwise\n  run \"printf\" with arguments [\"record\"]\n",
    )
    .unwrap();
    let result = runtime::run_with_context(
        &json,
        &Registry::default(),
        &options(dir.path()),
        BTreeMap::from([(
            String::from("setting"),
            Value::Record(BTreeMap::from([(String::from("key"), Value::Bool(true))])),
        )]),
        Vec::new(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(result.output, b"record");
}

#[test]
fn intervals_and_listener_handlers_use_the_same_branch_engine() {
    let dir = tempfile::tempdir().unwrap();
    for source in ["every 1 millisecond\n  if 1 is 1\n    run \"printf\" with arguments [\"tick\\n\"]\n  otherwise\n    run \"execute-never-launched\"\n", "listen to \"printf\" with arguments [\"a\\nb\\n\"]\nwhenever a line arrives as record\n  if record is \"a\\n\"\n    run \"printf\" with arguments [\"tick\\n\"]\n  otherwise\n    run \"printf\" with arguments [\"tick\\n\"]\n"] {
        let mut output=Vec::new();let result=runtime::run_service(&parser::parse(source).unwrap(),&Registry::default(),&options(dir.path()),&runtime::ServiceLimits{max_runs:Some(2),max_record_bytes:128},|bytes|{output.extend_from_slice(bytes);Ok(())}).unwrap();assert_eq!(output,b"tick\ntick\n");assert_eq!(result.runs,2);
    }
}

#[test]
fn completed_conditionals_restore_a_common_binding_and_ignore_comment_line_shifts() {
    let dir = tempfile::tempdir().unwrap();
    let block="if true is true\n  run \"sh\" with arguments [\"-c\", \"printf x >> calls; printf selected\"] as result\notherwise\n  run \"printf\" with arguments [\"unused\"] as result\n";
    fs::write(
        dir.path().join("main.program"),
        format!("{block}run \"false\"\n"),
    )
    .unwrap();
    assert!(
        !cli(dir.path(), &["main.program", "--checkpoint", "journal"])
            .status
            .success()
    );
    assert_eq!(fs::read(dir.path().join("calls")).unwrap(), b"x");
    fs::write(dir.path().join("main.program"),format!("# shifted source positions\n{block}then \"printf\" with arguments [\"<%s>\", result]\n")).unwrap();
    let resumed = cli(
        dir.path(),
        &["resume", "main.program", "--checkpoint", "journal"],
    );
    good(&resumed);
    assert_eq!(resumed.stdout, b"<selected>");
    assert_eq!(fs::read(dir.path().join("calls")).unwrap(), b"x");
    let changed = fs::read_to_string(dir.path().join("main.program"))
        .unwrap()
        .replace("unused", "changed unused branch");
    fs::write(dir.path().join("main.program"), changed).unwrap();
    let rejected = cli(
        dir.path(),
        &["resume", "main.program", "--checkpoint", "journal"],
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("completed checkpoint node"));
}

#[test]
fn partial_conditional_effects_require_explicit_uncertain_retry() {
    let dir = tempfile::tempdir().unwrap();
    let source="if true is true\n  run \"sh\" with arguments [\"-c\", \"printf x >> calls\"]\n  run \"false\"\notherwise\n  run \"true\"\n";
    fs::write(dir.path().join("main.program"), source).unwrap();
    assert!(
        !cli(dir.path(), &["main.program", "--checkpoint", "journal"])
            .status
            .success()
    );
    fs::write(
        dir.path().join("main.program"),
        source.replacen("run \"false\"", "run \"true\"", 1),
    )
    .unwrap();
    let rejected = cli(
        dir.path(),
        &["resume", "main.program", "--checkpoint", "journal"],
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("--retry-uncertain"));
    assert_eq!(fs::read(dir.path().join("calls")).unwrap(), b"x");
    good(&cli(
        dir.path(),
        &[
            "resume",
            "main.program",
            "--checkpoint",
            "journal",
            "--retry-uncertain",
        ],
    ));
    assert_eq!(fs::read(dir.path().join("calls")).unwrap(), b"xx");
}

#[test]
fn conditional_cd_works_ordinary_but_checkpoint_mode_rejects_it_before_effects() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    let source = "if true is true\n  run cd with path \"nested\"\nrun \"pwd\"\n";
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &registry,
        &options(dir.path()),
    )
    .unwrap();
    assert!(String::from_utf8(result.output)
        .unwrap()
        .trim()
        .ends_with("/nested"));
    let scheduled = parser::parse(
        "every 1 millisecond\n  if true is true\n    run cd with path \"nested\"\n  run \"pwd\"\n",
    )
    .unwrap();
    let mut ticks = Vec::new();
    runtime::run_service(
        &scheduled,
        &registry,
        &options(dir.path()),
        &runtime::ServiceLimits {
            max_runs: Some(2),
            max_record_bytes: 128,
        },
        |bytes| {
            ticks.push(String::from_utf8(bytes.to_vec()).unwrap());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(ticks.len(), 2);
    assert!(ticks.iter().all(|line| line.trim().ends_with("/nested")));
    fs::write(
        dir.path().join("main.program"),
        format!(
            "run \"printf\" with arguments [\"forbidden\"]\nthen save to \"forbidden\"\n{source}"
        ),
    )
    .unwrap();
    let rejected = cli(
        dir.path(),
        &[
            "main.program",
            "--checkpoint",
            "journal",
            "--manifest-dir",
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("manifests")
                .to_string_lossy(),
        ],
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr)
        .contains("checkpointed conditionals cannot change directory"));
    assert!(!dir.path().join("forbidden").exists());
}

#[test]
fn conditional_traces_report_decision_without_operand_values() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.program"),
        "if \"private-value\" is \"private-value\"\n  run \"true\"\n",
    )
    .unwrap();
    let output = cli(dir.path(), &["main.program", "--trace"]);
    good(&output);
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(!text.contains("private-value"));
    let events = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(events
        .iter()
        .any(|event| event["event"] == "condition" && event["matched"] == true));
}

#[test]
fn pure_cache_entries_inside_selected_branches_reuse_normal_cache_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    let p=parser::parse("if true is true\n  run printf with message \"hello\"\n  then tr with from \"[:lower:]\" and to \"[:upper:]\" and cache\notherwise\n  run \"execute-never-launched\"\n").unwrap();
    let first = runtime::run(&p, &registry, &options(dir.path())).unwrap();
    assert_eq!(first.output, b"HELLO\n");
    assert_eq!(first.cache_hits, 0);
    let second = runtime::run(&p, &registry, &options(dir.path())).unwrap();
    assert_eq!(second.output, b"HELLO\n");
    assert_eq!(second.cache_hits, 1);
    assert_eq!(second.launched_processes, 1);
}

#[test]
fn both_nested_checkpoint_paths_commit_and_resume_without_native_replay() {
    for (condition, wanted) in [("true", b"yes".as_slice()), ("false", b"no".as_slice())] {
        let dir = tempfile::tempdir().unwrap();
        let source=format!("run \"printf\" with arguments [\"input\"]\nif {condition} is true\n  if false is true\n    run \"printf\" with arguments [\"unused\"] as selected\n  otherwise\n    run \"printf\" with arguments [\"yes\"] as selected\notherwise\n  run \"printf\" with arguments [\"no\"] as selected\nthen \"printf\" with arguments [\"%s\", selected]\n");
        fs::write(dir.path().join("main.program"), source).unwrap();
        let first = cli(
            dir.path(),
            &["main.program", "--checkpoint", "journal", "--trace"],
        );
        good(&first);
        assert_eq!(first.stdout, wanted);
        assert_eq!(
            String::from_utf8_lossy(&first.stderr)
                .matches("\"event\":\"spawn\"")
                .count(),
            3
        );
        let resumed = cli(
            dir.path(),
            &[
                "resume",
                "main.program",
                "--checkpoint",
                "journal",
                "--trace",
            ],
        );
        good(&resumed);
        assert_eq!(resumed.stdout, wanted);
        assert!(!String::from_utf8_lossy(&resumed.stderr).contains("\"event\":\"spawn\""));
    }
}

#[test]
fn checkpoint_pins_absent_tools_in_unselected_branches_without_running_them() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.program"),"if true is true\n  run \"printf\" with arguments [\"selected\"]\notherwise\n  run \"./optional-tool\"\n").unwrap();
    let first = cli(dir.path(), &["main.program", "--checkpoint", "journal"]);
    good(&first);
    assert_eq!(first.stdout, b"selected");
    good(&cli(
        dir.path(),
        &["resume", "main.program", "--checkpoint", "journal"],
    ));
    let native_true = ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .unwrap();
    std::os::unix::fs::symlink(native_true, dir.path().join("optional-tool")).unwrap();
    let rejected = cli(
        dir.path(),
        &["resume", "main.program", "--checkpoint", "journal"],
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("completed checkpoint node"));
}

#[test]
fn readonly_validation_preserves_definite_data_and_folder_ordering() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input"), b"input bytes").unwrap();
    for (source, expected) in [
        ("if true is true\n  run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\n", "save requires preceding output"),
        ("if true is true\n  run \"sh\" with arguments [\"-c\", \"printf x > forbidden\"]\nuse folder \".\"\n", "use folder must precede data-producing steps"),
    ] {
        let error = runtime::run(&parser::parse(source).unwrap(), &Registry::default(), &options(dir.path())).err().unwrap();
        assert!(format!("{error:#}").contains(expected));
        assert!(!dir.path().join("forbidden").exists());
    }
    for (flag, expected) in [
        ("true", b"input bytes".as_slice()),
        ("false", b"".as_slice()),
    ] {
        let source = format!(
            "if {flag} is true\n  read \"input\"\notherwise\n  use folder \".\"\nthen \"cat\"\n"
        );
        assert_eq!(run(dir.path(), &source).output, expected);
    }
}
