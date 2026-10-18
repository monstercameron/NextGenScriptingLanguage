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
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(3)),
        ..runtime::RunOptions::new(cwd.into())
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
fn local_inputs_defaults_outputs_and_native_commands_are_real() {
    let dir = tempfile::tempdir().unwrap();
    let source="define a program named greet\n  accept name as text\n  accept enabled as boolean with default true\n  if enabled is true\n    run \"printf\" with arguments [\"hello %s\", name] as private\n  otherwise\n    run \"printf\" with arguments [\"disabled\"] as private\naccept name as text with default \"caller\"\nrun program greet with name \"雪; $HOME\" as first\nrun program greet with name name and enabled false as second\nthen \"printf\" with arguments [\"%s|%s|%s\", first, second, name]\n";
    let result = run(dir.path(), source);
    assert_eq!(result.output, "hello 雪; $HOME|disabled|caller".as_bytes());
    assert_eq!(result.launched_processes, 3);
}

#[test]
fn read_stdin_in_a_called_program_uses_call_bytes_and_preserves_binary_transport() {
    let dir = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..3 * 1024 * 1024).map(|n| (n % 256) as u8).collect();
    fs::write(dir.path().join("bytes"), &bytes).unwrap();
    let source="define a program named count\n  read from stdin\n  pipe to \"wc\" with arguments [\"-c\"]\nread \"bytes\"\nthen program count\n";
    let mut bounded = options(dir.path());
    bounded.max_output_bytes = 32;
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &Registry::default(),
        &bounded,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(result.output).unwrap().trim(),
        bytes.len().to_string()
    );
    assert_eq!(result.launched_processes, 1);
    let source="define a program named copy\n  read from stdin\n  pipe to \"cat\"\nread from stdin\nthen program copy\n";
    fs::write(dir.path().join("stdin.program"), source).unwrap();
    let payload = b"\0\xff\nowned\x80";
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .arg("stdin.program")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    let output = child.wait_with_output().unwrap();
    good(&output);
    assert_eq!(output.stdout, payload);
    let result=run(dir.path(),"define a program named count\n  read from stdin\n  pipe to \"wc\" with arguments [\"-c\"]\nrun \"printf\" with arguments [\"ignored\"]\nrun program count\n");
    assert_eq!(String::from_utf8(result.output).unwrap().trim(), "0");
}

#[test]
fn nested_calls_and_conditions_rebase_offsets_and_keep_all_local_names_private() {
    let dir = tempfile::tempdir().unwrap();
    let source="define a program named leaf\n  accept enabled as boolean\n  if enabled is true\n    run \"printf\" with arguments [\"yes\"] as message\n  otherwise\n    run \"printf\" with arguments [\"no\"] as message\ndefine a program named middle\n  accept enabled as boolean\n  run program leaf with enabled enabled as message\n  if message is \"yes\"\n    run \"printf\" with arguments [\"selected\"]\n  otherwise\n    run \"printf\" with arguments [\"other\"]\nif true is true\n  run program middle with enabled true as message\notherwise\n  run program middle with enabled false as message\nthen \"printf\" with arguments [\"%s\", message]\n";
    let result = run(dir.path(), source);
    assert_eq!(result.output, b"selected");
    assert_eq!(result.launched_processes, 3);
}

#[test]
fn child_cwd_changes_are_local_and_restore_before_caller_commands() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    let result=run(dir.path(),"define a program named location\n  accept folder as text\n  use folder folder\n  run \"pwd\"\nrun program location with folder \"sub\" as child\nrun \"pwd\" as caller\nthen \"printf\" with arguments [\"%s%s\", child, caller]\n");
    let root = fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        result.output,
        format!("{}/sub\n{}\n", root.display(), root.display()).as_bytes()
    );
}

#[test]
fn malformed_calls_and_unused_bodies_fail_before_any_effect() {
    let dir = tempfile::tempdir().unwrap();
    let definition="define a program named task\n  accept count as integer\n  then \"printf\" with arguments [\"%s\", count]\n";
    for bad in [
        "run program missing",
        "run program task",
        "run program task with count true",
        "run program task with count 2 and unknown 3",
        "run program task with count 2 and cache",
        "run program task with count 2 and keep status as code",
        "run program task with count 2\npipe to \"cat\"",
        "run program task with count 2\nthen \"printf\" with arguments [count]",
    ] {
        let source=format!("{definition}run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\n{bad}\n");
        let result = parser::parse(&source)
            .and_then(|p| runtime::run(&p, &Registry::default(), &options(dir.path())).map(|_| ()));
        assert!(result.is_err(), "{bad}");
        assert!(!dir.path().join("forbidden").exists());
    }
    for bad in ["define a program named unused\n  run missing_catalog_entry\n", "define a program named unused\n  run \"printf\" with arguments [unknown]\n", "define a program named unused\n  accept x as text\n  take integer at \"\" from true as invalid\n"] {
        let source=format!("{bad}run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\n");let result=parser::parse(&source).and_then(|p|runtime::run(&p,&Registry::default(),&options(dir.path())).map(|_|()));assert!(result.is_err());assert!(!dir.path().join("forbidden").exists());
    }
}

#[test]
fn recursion_nesting_bad_definitions_and_expansion_amplification_are_bounded() {
    for source in [
        "define a program named x\n  run program x\n",
        "define a program named x\n  run program y\ndefine a program named y\n  run program x\n",
        "define a program named x\n  accept x as text\n",
        "define a program named x\n  run \"true\"\ndefine a program named x\n  run \"true\"\n",
        "define a program named x\n  every 1 seconds\n    run \"true\"\n",
        "define a program named x\n  run \"true\"\n  read from stdin\n",
        "define a program named x\n  if true is true\n    read from stdin\n",
    ] {
        assert!(parser::parse(source).is_err(), "{source}");
    }
    let chain = (0..18)
        .map(|n| {
            format!(
                "define a program named p{n}\n  {}\n",
                if n == 17 {
                    "run \"true\"".into()
                } else {
                    format!("run program p{}", n + 1)
                }
            )
        })
        .collect::<String>();
    assert!(parser::parse(&chain).is_err());
    let mut huge = String::from("define a program named p0\n  run \"true\"\n");
    for n in 1..16 {
        huge.push_str(&format!(
            "define a program named p{n}\n  run program p{}\n  run program p{}\n",
            n - 1,
            n - 1
        ));
    }
    assert!(parser::parse(&huge).is_err());
    let payload = "x".repeat(1024 * 1024);
    let amplification = format!("define a program named large\n  accept payload as text with default \"{payload}\"\n  run \"true\"\n{}", "run program large\n".repeat(32));
    let error = parser::parse(&amplification).unwrap_err();
    assert!(format!("{error:#}").contains("expansion exceeds 16 MiB"));
}

#[test]
fn native_cache_inside_a_program_is_shared_across_calls() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lines"), b"first\nsecond\n").unwrap();
    let source = "define a program named first\n  accept count as integer with default 1\n  read from stdin\n  then head with lines count and cache\nread \"lines\"\nthen program first as a\nread \"lines\"\nthen program first as b\n";
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &registry,
        &options(dir.path()),
    )
    .unwrap();
    assert_eq!(result.output, b"first\n");
    assert_eq!(result.launched_processes, 1);
    assert_eq!(result.cache_hits, 1);
}

#[test]
fn scoped_preflight_retains_derived_integer_status_types_with_declared_root_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let source = "accept seed as text with default \"x\"\nrun \"false\" and keep status as code\nthen head with lines code\n";
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &registry,
        &options(dir.path()),
    )
    .unwrap();
    assert!(result.output.is_empty());
    assert_eq!(result.launched_processes, 2);
}

#[test]
fn typed_json_lists_and_repeated_parameter_names_do_not_prop_drill_implicitly() {
    let dir = tempfile::tempdir().unwrap();
    let source="define a program named read_count\n  accept count as json\n  take integer at \"/count\" from count as number\n  then \"printf\" with arguments [\"%s\", number]\naccept count as text with default \"parent\"\naccept data as json\nrun program read_count with count data as result\nthen \"printf\" with arguments [\"%s:%s\", count, result]\n";
    fs::write(dir.path().join("main.program"), source).unwrap();
    let output = cli(
        dir.path(),
        &["main.program", "--input", r#"data={"count":42}"#],
    );
    good(&output);
    assert_eq!(output.stdout, b"parent:42");
    let result=run(dir.path(),"define a program named echo\n  accept args as list of text\n  run \"printf\" with arguments args\naccept word as text with default \"literal $HOME; *\"\nrun program echo with args [\"%s\", word]\n");
    assert_eq!(result.output, b"literal $HOME; *");
}

#[test]
fn known_inputs_and_call_defaults_validate_before_earlier_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let source="define a program named select\n  accept value as json\n  take integer at \"/n\" from value as n\n  then encode n as json\naccept data as json\nrun \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\nrun program select with value data\n";
    fs::write(dir.path().join("main.program"), source).unwrap();
    let output = cli(
        dir.path(),
        &["main.program", "--input", r#"data={"n":"wrong"}"#],
    );
    assert!(!output.status.success());
    assert!(!dir.path().join("forbidden").exists());
}

#[test]
fn checkpoints_skip_complete_calls_pin_body_semantics_and_require_uncertain_retry() {
    let dir = tempfile::tempdir().unwrap();
    let definition="define a program named emit\n  accept text as text\n  run \"printf\" with arguments [\"%s\", text]\n";
    fs::write(
        dir.path().join("main.program"),
        format!("{definition}run program emit with text \"done\" as result\nrun \"false\"\n"),
    )
    .unwrap();
    assert!(
        !cli(dir.path(), &["main.program", "--checkpoint", "journal"])
            .status
            .success()
    );
    fs::write(dir.path().join("main.program"),format!("# move diagnostic lines\n{definition}run program emit with text \"done\" as result\nthen \"printf\" with arguments [\"%s\", result]\n")).unwrap();
    let output = cli(
        dir.path(),
        &[
            "resume",
            "main.program",
            "--checkpoint",
            "journal",
            "--retry-uncertain",
            "--trace",
        ],
    );
    good(&output);
    assert_eq!(output.stdout, b"done");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("program_enter"));
    fs::write(dir.path().join("main.program"),format!("{}run program emit with text \"done\" as result\nthen \"printf\" with arguments [\"%s\", result]\n",definition.replace("[\"%s\", text]","[\"changed %s\", text]"))).unwrap();
    assert!(!cli(
        dir.path(),
        &["resume", "main.program", "--checkpoint", "journal"]
    )
    .status
    .success());
    fs::write(dir.path().join("partial.program"),"define a program named partial\n  run \"printf\" with arguments [\"first\"]\n  then save to \"partial.out\"\n  run \"false\"\nrun program partial\n").unwrap();
    assert!(!cli(
        dir.path(),
        &["partial.program", "--checkpoint", "partial-journal"]
    )
    .status
    .success());
    assert_eq!(fs::read(dir.path().join("partial.out")).unwrap(), b"first");
    let output = cli(
        dir.path(),
        &[
            "resume",
            "partial.program",
            "--checkpoint",
            "partial-journal",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("uncertain"));
}

#[test]
fn checkpointed_child_cd_is_rejected_before_effects() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.program"),"define a program named move\n  run cd with path \".\"\n  run \"true\"\nrun \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\nrun program move\n").unwrap();
    let output = cli(dir.path(), &["main.program", "--checkpoint", "journal"]);
    assert!(!output.status.success());
    assert!(!dir.path().join("forbidden").exists());
}

#[test]
fn native_failures_and_deadlines_keep_call_context_and_shared_supervision() {
    let dir = tempfile::tempdir().unwrap();
    let program =
        parser::parse("define a program named fail\n  run \"false\"\nrun program fail\n").unwrap();
    let error = runtime::run(&program, &Registry::default(), &options(dir.path()))
        .err()
        .unwrap();
    assert!(error.downcast_ref::<runtime::CommandFailed>().is_some());
    assert!(format!("{error:#}").contains("program \"fail\", called at line 3"));
    let mut timed = options(dir.path());
    timed.command_timeout = Some(Duration::from_millis(50));
    let program = parser::parse(
        "define a program named wait\n  run \"sleep\" with arguments [\"2\"]\nrun program wait\n",
    )
    .unwrap();
    let started = std::time::Instant::now();
    assert!(runtime::run(&program, &Registry::default(), &timed).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn inspect_reports_defined_program_interfaces_and_old_program_serialization_is_compatible() {
    let dir = tempfile::tempdir().unwrap();
    let source="define a program named greet\n  accept name as text\n  accept enabled as boolean with default true\n  run \"true\"\nrun program greet with name \"user\"\n";
    fs::write(dir.path().join("main.program"), source).unwrap();
    let output = cli(dir.path(), &["inspect", "main.program"]);
    good(&output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["programs"][0]["name"], "greet");
    assert_eq!(value["programs"][0]["inputs"][0]["type"], "text");
    assert_eq!(value["programs"][0]["output"], "bytes");
    let old = parser::parse("run \"true\"\n").unwrap();
    assert!(serde_json::to_value(&old)
        .unwrap()
        .get("definitions")
        .is_none());
    let mut invalid = parser::parse(source).unwrap();
    if let StepKind::CallProgram { end, .. } = &mut invalid.steps[0].kind {
        *end = usize::MAX;
    }
    assert!(runtime::validate(&invalid, &Registry::default()).is_err());
    let mut o = options(dir.path());
    o.inputs.insert("unused".into(), Value::Bool(true));
    assert!(runtime::run(&old, &Registry::default(), &o).is_err());
}

#[test]
fn listeners_and_intervals_call_the_same_finite_programs() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("events"), b"one\ntwo\n").unwrap();
    let def="define a program named render\n  read from stdin as message\n  then \"printf\" with arguments [\"[%s]\\n\", message]\n";
    fs::write(dir.path().join("listener.program"),format!("{def}listen to \"cat\" with arguments [\"events\"]\nwhenever a line arrives as event\n  then program render\n")).unwrap();
    let output = cli(dir.path(), &["listener.program", "--max-runs", "2"]);
    good(&output);
    assert_eq!(output.stdout, b"[one\n]\n[two\n]\n");
    fs::write(dir.path().join("interval.program"),"define a program named tick\n  accept message as text\n  run \"printf\" with arguments [\"%s\\n\", message]\nevery 1 milliseconds\n  run program tick with message \"tick\"\n").unwrap();
    let output = cli(dir.path(), &["interval.program", "--max-runs", "2"]);
    good(&output);
    assert_eq!(output.stdout, b"tick\ntick\n");
}
