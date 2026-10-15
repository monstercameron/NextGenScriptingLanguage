#![cfg(unix)]
use flow::{
    ast::{StepKind, Value},
    manifest::Registry,
    parser, runtime, stream,
};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::Command,
    sync::{atomic::AtomicBool, Arc},
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

#[test]
fn native_status_is_an_integer_and_retains_complete_stdout_for_success_and_failure() {
    let dir = tempfile::tempdir().unwrap();
    for code in [0, 1, 7, 255] {
        // Explicit shell is an owned exit-code fixture; Execute never inserts a shell.
        let source=format!("run \"sh\" with arguments [\"-c\", \"printf 'partial\\n'; exit {code}\"] as output and keep status as code\nthen \"printf\" with arguments [\"%s:<%s>\", code, output]\n");
        let result = runtime::run(
            &parser::parse(&source).unwrap(),
            &Registry::default(),
            &options(dir.path()),
        )
        .unwrap();
        assert_eq!(result.output, format!("{code}:<partial\n>").as_bytes());
        assert_eq!(result.launched_processes, 2);
    }
}

#[test]
fn parser_preserves_parameter_named_keep_and_old_serialization() {
    let p =
        parser::parse("run worker with item 1 and keep 2 as text and keep status as code").unwrap();
    let StepKind::Invoke { call, .. } = &p.steps[0].kind else {
        panic!()
    };
    assert_eq!(call.parameters["keep"], Value::Integer(2));
    assert_eq!(call.binding.as_deref(), Some("text"));
    assert_eq!(call.status_binding.as_deref(), Some("code"));
    let old = parser::parse("run worker with item 1 as text").unwrap();
    let json = serde_json::to_string(&old.steps[0].kind).unwrap();
    assert_eq!(
        json,
        r#"{"Invoke":{"call":{"operation":"worker","parameters":{"item":{"Integer":1}},"binding":"text","cache":false},"connection":"Start"}}"#
    );
    let decoded: StepKind = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, old.steps[0].kind);
    for bad in [
        "run \"false\" and keep status",
        "run \"false\" and keep status as",
        "run \"false\" and keep status as code garbage",
        "run \"false\" and keep status as code as output",
    ] {
        assert!(parser::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn invalid_status_uses_fail_before_prior_effects() {
    let dir = tempfile::tempdir().unwrap();
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    for bad in [
        "run \"false\" and keep status as code\npipe to \"cat\"",
        "run \"true\"\npipe to \"false\" and keep status as code",
        "run printf with message \"x\" and keep status as code and cache",
        "run cd with path \".\" and keep status as code",
        "run \"false\" as code and keep status as code",
        "run \"false\" and keep status as code\nrun \"true\" and keep status as code",
        "run \"false\" and keep status as code\nread code",
        "run \"false\" and keep status as code\nthen save to code",
    ] {
        let source = format!(
            "run \"printf\" with arguments [\"forbidden\"]\nthen save to \"forbidden\"\n{bad}\n"
        );
        let error = runtime::run(
            &parser::parse(&source).unwrap(),
            &registry,
            &options(dir.path()),
        )
        .err()
        .unwrap();
        assert!(!dir.path().join("forbidden").exists(), "{bad}: {error:#}");
    }
}

#[test]
fn signals_timeout_spawn_and_cancel_are_never_converted_to_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::default();
    let signalled=parser::parse("run \"sh\" with arguments [\"-c\", \"kill -TERM $$\"] and keep status as code\nthen save to \"forbidden\"").unwrap();
    let error = runtime::run(&signalled, &registry, &options(dir.path()))
        .err()
        .unwrap();
    let failed = error.downcast_ref::<runtime::CommandFailed>().unwrap();
    assert!(failed.status.code().is_none());
    assert!(!dir.path().join("forbidden").exists());
    let sleeping =
        parser::parse("run \"sleep\" with arguments [\"2\"] and keep status as code").unwrap();
    let mut opts = options(dir.path());
    opts.command_timeout = Some(Duration::from_millis(30));
    let error = runtime::run(&sleeping, &registry, &opts).err().unwrap();
    assert!(format!("{error:#}").contains("timed out"));
    assert!(error.downcast_ref::<runtime::CommandFailed>().is_none());
    let missing =
        parser::parse("run \"execute-nonexistent-status-test-binary\" and keep status as code")
            .unwrap();
    let error = runtime::run(&missing, &registry, &opts).err().unwrap();
    assert!(format!("{error:#}").contains("not found"));
    let error = runtime::run_with_context(
        &sleeping,
        &registry,
        &opts,
        BTreeMap::new(),
        Vec::new(),
        Arc::new(AtomicBool::new(true)),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("cancelled"));
}

#[test]
fn ordinary_failure_still_fails_with_structured_native_status() {
    let dir = tempfile::tempdir().unwrap();
    let p = parser::parse("run \"sh\" with arguments [\"-c\", \"exit 23\"]").unwrap();
    let error = runtime::run(&p, &Registry::default(), &options(dir.path()))
        .err()
        .unwrap();
    let failure = error.downcast_ref::<runtime::CommandFailed>().unwrap();
    assert_eq!(failure.status.code(), Some(23));
    assert_eq!(failure.line, 1);
    assert_eq!(failure.operation, "sh");
    assert!(failure.to_string().contains("exit status: 23"));
}

#[test]
fn status_capture_is_not_a_stream_or_listener_source_option() {
    let p = stream::parse("stream from \"false\" and keep status as code\nshow each line").unwrap();
    assert!(stream::validate(&p, &Registry::default()).is_err());
    assert!(parser::parse("listen to \"false\" and keep status as code\nwhenever a line arrives as record\n  run \"true\"").is_err());
}

#[test]
fn listener_handlers_capture_each_native_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let p=parser::parse("listen to \"printf\" with arguments [\"a\\nb\\n\"]\nwhenever a line arrives as record\n  run \"false\" and keep status as code\n  then \"printf\" with arguments [\"%s:%s\", code, record]\n").unwrap();
    let mut output = Vec::new();
    runtime::run_service(
        &p,
        &Registry::default(),
        &options(dir.path()),
        &runtime::ServiceLimits {
            max_runs: Some(2),
            max_record_bytes: 128,
        },
        |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(output, b"1:a\n1:b\n");
}

#[test]
fn completed_status_binding_is_restored_and_command_is_not_repeated() {
    let dir = tempfile::tempdir().unwrap();
    let first = "run \"sh\" with arguments [\"-c\", \"printf x >> calls; exit 7\"] and keep status as code\n";
    fs::write(
        dir.path().join("main.program"),
        format!("{first}run \"false\"\n"),
    )
    .unwrap();
    let failed = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .args(["main.program", "--checkpoint", "journal"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert_eq!(fs::read(dir.path().join("calls")).unwrap(), b"x");
    // Repair only the failed suffix: printf must read the restored integer.
    fs::write(
        dir.path().join("main.program"),
        format!("{first}then \"printf\" with arguments [\"%s\", code]\n"),
    )
    .unwrap();
    for _ in 0..2 {
        let result = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(dir.path())
            .args(["resume", "main.program", "--checkpoint", "journal"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"7");
    }
    assert_eq!(fs::read(dir.path().join("calls")).unwrap(), b"x");
    fs::write(dir.path().join("main.program"),"run \"sh\" with arguments [\"-c\", \"printf x >> calls; exit 7\"] and keep status as different\nthen \"printf\" with arguments [\"%s\", different]\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .args(["resume", "main.program", "--checkpoint", "journal"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("completed checkpoint node changed"));
    assert_eq!(fs::read(dir.path().join("calls")).unwrap(), b"x");
}

#[test]
fn capture_preserves_named_manifest_calls_and_output_limits() {
    let dir = tempfile::tempdir().unwrap();
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    let program=parser::parse("run printf with message \"hi\" and keep status as code\nthen \"printf\" with arguments [\"%s\", code]\n").unwrap();
    assert_eq!(
        runtime::run(&program, &registry, &options(dir.path()))
            .unwrap()
            .output,
        b"0"
    );
    let mut opts = options(dir.path());
    opts.max_output_bytes = 2;
    let program=parser::parse("run \"sh\" with arguments [\"-c\", \"printf 123456789; exit 7\"] and keep status as code\nthen save to \"forbidden\"\n").unwrap();
    let error = runtime::run(&program, &registry, &opts).err().unwrap();
    assert!(format!("{error:#}").contains("output exceeds maximum"));
    assert!(!dir.path().join("forbidden").exists());
}

#[test]
fn cli_preserves_unhandled_native_statuses_and_keeps_handled_failures_successful() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("status.program");
    for code in [0, 1, 2, 7, 42, 126, 127, 255] {
        fs::write(
            &file,
            format!("run \"sh\" with arguments [\"-c\", \"exit {code}\"]\n"),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(&file)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{code}: {:?}",
            output.stderr
        );
        assert!(output.stdout.is_empty());
    }
    fs::write(&file, "run \"sh\" with arguments [\"-c\", \"exit 42\"] and keep status as code\nrun \"printf\" with arguments [\"%s\", code]\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(&file)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"42");
}

#[test]
fn cli_reports_signaled_native_children_with_conventional_shell_status() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("signal.program");
    fs::write(
        &file,
        "run \"sh\" with arguments [\"-c\", \"kill -TERM $$\"]\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(143), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
}
