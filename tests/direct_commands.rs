#![cfg(unix)]

use flow::{ast::StepKind, manifest::Registry, parser, runtime, stream};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::symlink,
    path::Path,
    process::Command,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        inputs: Default::default(),
        working_directory: cwd.to_owned(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 4 * 1024 * 1024,
        command_timeout: Some(Duration::from_secs(10)),
        total_timeout: None,
    }
}

#[test]
fn direct_pipeline_preserves_binary_bytes_without_a_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
    fs::write(directory.path().join("input with spaces"), &bytes).unwrap();
    let program =
        parser::parse("run \"cat\" with arguments [\"input with spaces\"]\npipe to \"cat\"\n")
            .unwrap();
    let registry = Registry::load_optional(&directory.path().join("absent-catalog")).unwrap();
    assert!(registry.manifests().unwrap().is_empty());
    let result = runtime::run(&program, &registry, &options(directory.path())).unwrap();
    assert_eq!(result.output, bytes);
    assert_eq!(result.launched_processes, 2);
}

#[test]
fn direct_argv_preserves_empty_unicode_and_shell_looking_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let arguments = [
        "",
        "café ☃",
        "$(touch forbidden)",
        "`touch forbidden`",
        "*;$HOME",
        "with",
        "as",
        "and cache",
    ];
    let mut argv = vec!["<%s>".to_owned()];
    argv.extend(arguments.map(str::to_owned));
    let source = format!(
        "run \"printf\" with arguments {}\n",
        serde_json::to_string(&argv).unwrap()
    );
    let result = runtime::run(
        &parser::parse(&source).unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .unwrap();
    assert_eq!(
        result.output,
        arguments.map(|arg| format!("<{arg}>")).concat().as_bytes()
    );
    assert!(!directory.path().join("forbidden").exists());
}

#[test]
fn quoted_relative_executable_with_spaces_and_named_arguments_work() {
    let directory = tempfile::tempdir().unwrap();
    let printf = ["/usr/bin/printf", "/bin/printf"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
        .unwrap();
    symlink(printf, directory.path().join("my printer")).unwrap();
    let source = "run \"printf\" with arguments [\"%s\", \"first\"] as message\nthen \"./my printer\" with arguments [\"<%s>\", message]\n";
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .unwrap();
    assert_eq!(result.output, b"<first>");
}

#[test]
fn direct_validation_prevents_prior_effects_and_rejects_invalid_arguments() {
    let directory = tempfile::tempdir().unwrap();
    for invalid in [
        "run \"cat\" with path \"input\"",
        "run \"cat\" with arguments \"input\"",
        "run \"cat\" with arguments [[\"nested\"]]",
        "run \"cat\" with arguments [\"bad\\u0000argument\"]",
        "run \"cat\" with arguments [missing]",
        "run \"cat\" and cache",
        "run misspelled_applet",
    ] {
        let source = format!(
            "run \"printf\" with arguments [\"unsafe\"]\nthen save to \"forbidden\"\n{invalid}\n"
        );
        let program = parser::parse(&source).unwrap();
        assert!(
            runtime::run(&program, &Registry::default(), &options(directory.path())).is_err(),
            "{invalid}"
        );
        assert!(!directory.path().join("forbidden").exists(), "{invalid}");
    }
    for source in [
        "run \"\"",
        "run \"bad\\u0000binary\"",
        "run \"cat\" extra",
        "run \"cat\" with arguments [] garbage",
    ] {
        assert!(parser::parse(source).is_err(), "{source}");
    }
}

#[test]
fn direct_stream_starts_each_stage_once_and_forwards_final_partial_record() {
    let directory = tempfile::tempdir().unwrap();
    let program = stream::parse("stream from \"printf\" with arguments [\"one\\ntwo\\nlast\"]\npipe to \"tr\" with arguments [\"[:lower:]\", \"[:upper:]\"]\nshow each line\n").unwrap();
    let mut records = Vec::new();
    let result = runtime::run_stream(
        &program,
        &Registry::default(),
        &options(directory.path()),
        128,
        |bytes, _, _| {
            records.push(bytes.to_vec());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        records,
        [b"ONE\n".to_vec(), b"TWO\n".to_vec(), b"LAST".to_vec()]
    );
    assert_eq!(result.launched_processes, 2);
    assert_eq!(result.records, 3);
}

#[test]
fn direct_listener_keeps_its_producer_and_runs_native_handlers() {
    let directory = tempfile::tempdir().unwrap();
    let program = parser::parse("listen to \"printf\" with arguments [\"one\\ntwo\\n\"]\nwhenever a line arrives as event\n  then \"tr\" with arguments [\"[:lower:]\", \"[:upper:]\"]\n").unwrap();
    let mut bytes = Vec::new();
    let result = runtime::run_service(
        &program,
        &Registry::default(),
        &options(directory.path()),
        &runtime::ServiceLimits {
            max_runs: Some(2),
            max_record_bytes: 128,
        },
        |record| {
            bytes.extend_from_slice(record);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(bytes, b"ONE\nTWO\n");
    assert_eq!(result.launched_processes, 3);
}

#[test]
fn direct_commands_retain_timeout_cancellation_and_failure_semantics() {
    let directory = tempfile::tempdir().unwrap();
    let registry = Registry::default();
    let failed = parser::parse("run \"false\"\npipe to \"cat\"\n").unwrap();
    assert!(runtime::run(&failed, &registry, &options(directory.path())).is_err());
    let sleeping = parser::parse("run \"sleep\" with arguments [\"30\"]\n").unwrap();
    let mut limits = options(directory.path());
    limits.command_timeout = Some(Duration::from_millis(50));
    let error = runtime::run(&sleeping, &registry, &limits).err().unwrap();
    assert!(format!("{error:#}").contains("timed out"));
    let error = runtime::run_with_context(
        &sleeping,
        &registry,
        &options(directory.path()),
        BTreeMap::new(),
        Vec::new(),
        Arc::new(AtomicBool::new(true)),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("cancelled"));
}

#[test]
fn direct_explain_is_inspectable_and_never_executes() {
    let directory = tempfile::tempdir().unwrap();
    let program = parser::parse("run \"touch\" with arguments [\"forbidden\"]\n").unwrap();
    let explanation = runtime::explain(&program, &Registry::default()).unwrap();
    assert!(
        explanation.contains("\"touch\" argv=[\"forbidden\"]"),
        "{explanation}"
    );
    assert!(!directory.path().join("forbidden").exists());
    let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
        panic!()
    };
    assert!(call.direct);
    let legacy = parser::parse("run pwd\n").unwrap();
    let serialized = serde_json::to_string(&legacy).unwrap();
    assert!(
        !serialized.contains("direct"),
        "legacy checkpoint representation changed"
    );
    assert_eq!(
        serde_json::from_str::<flow::ast::Program>(&serialized).unwrap(),
        legacy
    );
}

#[test]
fn direct_cli_runs_without_a_wrapper_and_explicit_missing_catalog_still_fails() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("checksum.program");
    fs::write(&source, "run \"cksum\" with arguments [\"input\"]\n").unwrap();
    fs::write(directory.path().join("input"), b"owned fixture\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(&source)
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let native = Command::new("cksum")
        .arg("input")
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert_eq!(output.stdout, native.stdout);
    let bad = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(&source)
        .args(["--manifest-dir", "missing"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!bad.status.success());
}

#[test]
fn direct_calls_mix_with_manifest_adapters_and_use_distinct_checkpoint_identities() {
    let directory = tempfile::tempdir().unwrap();
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    let mixed =
        parser::parse("run \"printf\" with arguments [\"mixed\"]\npipe to uppercase\n").unwrap();
    assert_eq!(
        runtime::run(&mixed, &registry, &options(directory.path()))
            .unwrap()
            .output,
        b"MIXED"
    );
    let source = directory.path().join("job.program");
    fs::write(&source, "run \"true\"\n").unwrap();
    let checkpoint = runtime::CheckpointOptions {
        directory: directory.path().join("checkpoint"),
        program_path: source.clone(),
        resume: false,
        retry_uncertain: false,
    };
    let program = parser::parse("run \"true\"\n").unwrap();
    runtime::run_checkpointed(
        &program,
        &Registry::default(),
        &options(directory.path()),
        &checkpoint,
    )
    .unwrap();
    let resume = runtime::CheckpointOptions {
        resume: true,
        ..checkpoint
    };
    let unchanged = runtime::run_checkpointed(
        &program,
        &Registry::default(),
        &options(directory.path()),
        &resume,
    )
    .unwrap();
    assert_eq!(unchanged.launched_processes, 0);
    let changed = parser::parse("run \"false\"\n").unwrap();
    let error = runtime::run_checkpointed(
        &changed,
        &Registry::default(),
        &options(directory.path()),
        &resume,
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("completed checkpoint node"));
}
