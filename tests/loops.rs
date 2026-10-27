#![cfg(unix)]
//! Loops use actual native applets in disposable directories.
use flow::{manifest::Registry, parser, runtime};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Output},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
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

#[test]
fn repeat_threads_current_output_and_zero_iterations_preserve_input() {
    let dir = tempfile::tempdir().unwrap();
    let result = run(dir.path(), "run \"printf\" with arguments [\"a\"]\nrepeat 3 times\n  then \"tr\" with arguments [\"a-c\", \"b-d\"]\n");
    assert_eq!(result.output, b"d");
    assert_eq!(result.launched_processes, 4);
    let result = run(
        dir.path(),
        "run \"printf\" with arguments [\"keep\"]\nrepeat 0 times\n  then \"cat\"\n",
    );
    assert_eq!(result.output, b"keep");
    assert_eq!(result.launched_processes, 1);
    let result = run(
        dir.path(),
        "repeat 0 times\n  run \"printf\" with arguments [\"unused\"]\n",
    );
    assert!(result.output.is_empty());
    assert_eq!(result.launched_processes, 0);
}

#[test]
fn foreach_uses_literal_filenames_and_keeps_only_last_result() {
    let dir = tempfile::tempdir().unwrap();
    for (name, bytes) in [
        ("first one.txt", b"first".as_slice()),
        ("last ; $HOME.txt", b"last"),
    ] {
        fs::write(dir.path().join(name), bytes).unwrap();
    }
    let source = "accept files as list of text\nfor each file in files\n  run \"cat\" with arguments [file]\n  then save to \"last.out\"\n";
    let mut opts = options(dir.path());
    opts.inputs.insert(
        "files".into(),
        flow::ast::Value::List(vec![
            flow::ast::Value::String("first one.txt".into()),
            flow::ast::Value::String("last ; $HOME.txt".into()),
        ]),
    );
    let result =
        runtime::run(&parser::parse(source).unwrap(), &Registry::default(), &opts).unwrap();
    assert_eq!(result.output, b"last");
    assert_eq!(fs::read(dir.path().join("last.out")).unwrap(), b"last");
    assert_eq!(result.launched_processes, 2);
    assert_eq!(run(dir.path(), "run \"printf\" with arguments [\"keep\"]\nfor each file in []\n  run \"cat\" with arguments [file]\n").output, b"keep");
}

#[test]
fn foreach_iterates_json_array_literals_and_keeps_the_last_value() {
    let dir = tempfile::tempdir().unwrap();
    let result = run(
        dir.path(),
        "for each item in [1, null, {\"nested\": [2]}]\n  then encode item as json\n",
    );
    assert_eq!(result.output, br#"{"nested":[2]}"#);
}

#[test]
fn while_reevaluates_status_and_preserves_type_compatible_outer_rebinding() {
    let dir = tempfile::tempdir().unwrap();
    let result = run(dir.path(), "run \"true\" and keep status as code\nwhile code is 0\n  run \"printf\" with arguments [\"one iteration\"]\n  then save to \"observed\"\n  run \"false\" and keep status as code\nthen \"printf\" with arguments [\"status=%s\", code]\n");
    assert_eq!(result.output, b"status=1");
    assert_eq!(result.launched_processes, 4);
    assert_eq!(
        fs::read(dir.path().join("observed")).unwrap(),
        b"one iteration"
    );
}

#[test]
fn nested_conditions_and_reusable_calls_reenter_the_right_headers() {
    let dir = tempfile::tempdir().unwrap();
    let result = run(dir.path(), "define a program named append\n  repeat 2 times\n    if true is true\n      then \"tr\" with arguments [\"a-z\", \"b-za\"]\n    otherwise\n      run \"false\"\nrun \"printf\" with arguments [\"a\"]\nrepeat 2 times\n  then program append\n");
    assert_eq!(result.output, b"e");
    assert_eq!(result.launched_processes, 5);
}

#[test]
fn invalid_body_counts_and_local_escapes_fail_before_any_effect() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        "repeat -1 times\n  run \"true\"",
        "repeat 100001 times\n  run \"true\"",
        "repeat 0 times\n  run missing_manifest",
        "for each item in true\n  run \"true\"",
        "repeat 1 times\n  run \"printf\" with arguments [\"x\"] as local\nthen \"printf\" with arguments [local]",
        "run \"printf\" with arguments [\"outer\"] as item\nfor each item in [\"a\"]\n  run \"true\"",
        "run \"true\" and keep status as code\nwhile code is 0\n  run \"printf\" with arguments [\"wrong type\"] as code",
    ] {
        let source = format!("run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\n{bad}\n");
        let error = parser::parse(&source).and_then(|program| runtime::run(&program, &Registry::default(), &options(dir.path())));
        assert!(error.is_err(), "{bad}");
        assert!(!dir.path().join("forbidden").exists(), "{bad}");
    }
}

#[test]
fn known_loop_inputs_are_preflighted_before_earlier_effects_in_all_contexts() {
    let dir = tempfile::tempdir().unwrap();
    let body = "run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\nrepeat count times\n  run \"true\"\n";
    fs::write(
        dir.path().join("main.program"),
        format!("accept count as integer\n{body}"),
    )
    .unwrap();
    for value in ["-1", "100001"] {
        let input = format!("count={value}");
        let output = cli(dir.path(), &["main.program", "--input", &input]);
        assert!(!output.status.success());
        assert!(!dir.path().join("forbidden").exists());
    }
    let program = parser::parse(body).unwrap();
    let supplied = BTreeMap::from([("count".into(), flow::ast::Value::Integer(-1))]);
    let error = runtime::run_with_context(
        &program,
        &Registry::default(),
        &options(dir.path()),
        supplied,
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("repeat count"));
    assert!(!dir.path().join("forbidden").exists());
    let program = parser::parse("run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\nwhile flag is true\n  run \"true\"\n").unwrap();
    for value in [
        flow::ast::Value::String("wrong scalar type".into()),
        flow::ast::Value::List(Vec::new()),
    ] {
        let supplied = BTreeMap::from([("flag".into(), value)]);
        let error = runtime::run_with_context(
            &program,
            &Registry::default(),
            &options(dir.path()),
            supplied,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("condition"));
        assert!(!dir.path().join("forbidden").exists());
    }
}

#[test]
fn nested_loops_and_calls_share_one_entered_iteration_budget() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("empty"), []).unwrap();
    for source in [
        "repeat 100000 times\n  repeat 1 times\n    read \"empty\"\n",
        "define a program named inner\n  repeat 1 times\n    read \"empty\"\nrepeat 100000 times\n  run program inner\n",
    ] {
        let program = parser::parse(source).unwrap();
        let error = runtime::run(&program, &Registry::default(), &options(dir.path()))
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("iteration"));
    }
}

#[test]
fn scheduled_handlers_get_fresh_loop_frames_for_each_tick() {
    let dir = tempfile::tempdir().unwrap();
    let source = "every 1 millisecond\n  run \"printf\" with arguments [\"a\"]\n  repeat 2 times\n    then \"tr\" with arguments [\"a-z\", \"b-za\"] as local\n    use value local\n";
    let program = parser::parse(source).unwrap();
    let mut outputs = Vec::new();
    runtime::run_service(
        &program,
        &Registry::default(),
        &options(dir.path()),
        &runtime::ServiceLimits {
            max_runs: Some(2),
            max_record_bytes: 128,
        },
        |bytes| {
            outputs.push(bytes.to_vec());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(outputs, [b"c".to_vec(), b"c".to_vec()]);
}

#[test]
fn pure_control_loops_observe_cancellation_without_launching_children() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("payload"), vec![b'x'; 64 * 1024]).unwrap();
    let program = parser::parse("while true is true\n  read \"payload\"\n").unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let token = cancelled.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        token.store(true, Ordering::Relaxed);
    });
    let start = Instant::now();
    let error = runtime::run_with_context(
        &program,
        &Registry::default(),
        &options(dir.path()),
        BTreeMap::new(),
        Vec::new(),
        cancelled,
    )
    .err()
    .unwrap();
    canceller.join().unwrap();
    assert!(format!("{error:#}").contains("cancelled"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn ordinary_loop_cwd_changes_persist_but_checkpoint_controls_preflight_them() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    let result = runtime::run(
        &parser::parse("repeat 1 times\n  run cd with path \"nested\"\nrun \"pwd\"\n").unwrap(),
        &registry,
        &options(dir.path()),
    )
    .unwrap();
    let expected = fs::canonicalize(dir.path().join("nested")).unwrap();
    assert_eq!(
        String::from_utf8(result.output).unwrap().trim(),
        expected.to_str().unwrap()
    );
    fs::write(dir.path().join("main.program"), "run \"printf\" with arguments [\"effect\"]\nthen save to \"forbidden\"\nrepeat 1 times\n  run cd with path \"nested\"\n").unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let output = cli(
        dir.path(),
        &[
            "main.program",
            "--checkpoint",
            "journal",
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(!dir.path().join("forbidden").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("outside their bodies"));
}

#[test]
fn checkpoints_skip_completed_loops_pin_body_and_require_partial_retry() {
    let dir = tempfile::tempdir().unwrap();
    let prefix =
        "repeat 2 times\n  run \"printf\" with arguments [\"done\"]\n  then save to \"effect\"\n";
    fs::write(
        dir.path().join("main.program"),
        format!("{prefix}run \"false\"\n"),
    )
    .unwrap();
    assert!(
        !cli(dir.path(), &["main.program", "--checkpoint", "journal"])
            .status
            .success()
    );
    fs::write(dir.path().join("effect"), b"sentinel").unwrap();
    fs::write(
        dir.path().join("main.program"),
        format!("# moved lines\n{prefix}run \"printf\" with arguments [\"recovered\"]\n"),
    )
    .unwrap();
    let resumed = cli(
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
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(resumed.stdout, b"recovered");
    assert_eq!(fs::read(dir.path().join("effect")).unwrap(), b"sentinel");
    assert!(!String::from_utf8_lossy(&resumed.stderr).contains("\"event\":\"loop\""));
    fs::write(
        dir.path().join("main.program"),
        format!(
            "{}run \"printf\" with arguments [\"recovered\"]\n",
            prefix.replace("repeat 2", "repeat 3")
        ),
    )
    .unwrap();
    let changed = cli(
        dir.path(),
        &["resume", "main.program", "--checkpoint", "journal"],
    );
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("completed checkpoint node"));
    let partial = "for each outputpath in [\"first.out\", \"second.out\"]\n  run \"printf\" with arguments [\"partial\"]\n  then save to outputpath\n  run \"false\"\n";
    fs::write(dir.path().join("partial.program"), partial).unwrap();
    assert!(!cli(
        dir.path(),
        &["partial.program", "--checkpoint", "partial-journal"]
    )
    .status
    .success());
    assert_eq!(fs::read(dir.path().join("first.out")).unwrap(), b"partial");
    assert!(!dir.path().join("second.out").exists());
    let retry = cli(
        dir.path(),
        &[
            "resume",
            "partial.program",
            "--checkpoint",
            "partial-journal",
        ],
    );
    assert!(!retry.status.success());
    assert!(String::from_utf8_lossy(&retry.stderr).contains("uncertain"));
    fs::write(dir.path().join("first.out"), b"sentinel").unwrap();
    fs::write(
        dir.path().join("partial.program"),
        partial.replace("run \"false\"", "run \"true\""),
    )
    .unwrap();
    let retry = cli(
        dir.path(),
        &[
            "resume",
            "partial.program",
            "--checkpoint",
            "partial-journal",
            "--retry-uncertain",
        ],
    );
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    // Explicit retry restarts the entire node, including its first iteration.
    assert_eq!(fs::read(dir.path().join("first.out")).unwrap(), b"partial");
    assert_eq!(fs::read(dir.path().join("second.out")).unwrap(), b"partial");
}
