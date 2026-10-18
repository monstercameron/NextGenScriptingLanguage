#![cfg(feature = "graph")]
use flow::ast::{StepKind, Trigger};
use flow::manifest::Registry;
use flow::{parser, runtime};
#[cfg(unix)]
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: Some(Duration::from_secs(10)),
        ..runtime::RunOptions::new(cwd.to_path_buf())
    }
}

#[cfg(unix)]
#[test]
fn direct_stderr_discard_keeps_stdout_status_and_truthful_result_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let source = concat!(
        "run \"/bin/sh\" with arguments [\"-c\", \"printf kept; printf warning >&2; exit 7\"] ",
        "and discard stderr and keep status as code and keep result as execution\n",
        "if code is 7\n",
        "  then encode execution as json\n",
        "otherwise\n",
        "  run \"/bin/false\"\n",
    );
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .unwrap();
    let record: JsonValue = serde_json::from_slice(&result.output).unwrap();

    assert_eq!(record["status"], 7);
    assert_eq!(record["stdout_captured"], true);
    assert_eq!(record["stdout"], serde_json::json!(b"kept"));
    assert_eq!(record["stderr_captured"], false);
    assert_eq!(record["stderr"], serde_json::json!([]));
    assert_eq!(record["stages"][0]["stderr_captured"], false);
    assert_eq!(result.executions.len(), 1);
    assert_eq!(result.executions[0].status, Some(7));
    assert!(result.executions[0].stdout_captured);
    assert!(!result.executions[0].stderr_captured);
}

#[cfg(unix)]
#[test]
fn unhandled_failure_keeps_interpreter_diagnostic_and_native_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("discard-stderr.program");
    fs::write(
        &source,
        concat!(
            "run \"/bin/sh\" with arguments [\"-c\", ",
            "\"printf child-warning >&2; exit 7\"] and discard stderr\n",
        ),
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(&source)
        .current_dir(directory.path())
        .output()
        .unwrap();
    let diagnostic = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(7));
    assert!(
        diagnostic.contains("execute: line 1: applet \"/bin/sh\" failed (exit status: 7):"),
        "the interpreter should retain its contextual failure diagnostic: {diagnostic:?}"
    );
    assert!(
        !diagnostic.contains("child-warning"),
        "the discarded child stream must stay suppressed: {diagnostic:?}"
    );
}

#[cfg(unix)]
#[test]
fn direct_stderr_discard_uses_the_native_stream_stage_launch_path() {
    let directory = tempfile::tempdir().unwrap();
    let stream = flow::stream::parse(
        "stream from \"/bin/sh\" with arguments [\"-c\", \"printf 'streamed\\\\n'; printf warning >&2\"] and discard stderr\nshow each line\n",
    )
    .unwrap();
    let mut records = Vec::new();
    let result = runtime::run_stream(
        &stream,
        &Registry::default(),
        &options(directory.path()),
        1024,
        |bytes, _, _| {
            records.push(bytes.to_vec());
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(records, [b"streamed\n".to_vec()]);
    assert_eq!(result.records, 1);
    assert_eq!(result.launched_processes, 1);
}

#[cfg(unix)]
#[test]
fn reusable_program_expansion_preserves_direct_stderr_discard() {
    let directory = tempfile::tempdir().unwrap();
    let source = concat!(
        "define a program named emit\n",
        "  accept message as text\n",
        "  run \"/bin/sh\" with arguments [\"-c\", \"printf '%s' \\\"$1\\\"; printf warning >&2\", \"sh\", message] and discard stderr\n",
        "run program emit with message \"kept\"\n",
    );
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .unwrap();
    assert_eq!(result.output, b"kept");
}

#[test]
fn unsupported_ast_contexts_fail_before_earlier_effects() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("must-not-exist");
    let path = serde_json::to_string(marker.to_str().unwrap()).unwrap();
    let source = format!(
        "run \"/bin/sh\" with arguments [\"-c\", \"touch \\\"$1\\\"\", \"sh\", {path}]\nrun named.fixture\n"
    );
    let mut program = parser::parse(&source).unwrap();
    let StepKind::Invoke { call, .. } = &mut program.steps[1].kind else {
        panic!("expected the second statement to be an invocation");
    };
    call.discard_stderr = true;

    let error = runtime::run(&program, &Registry::default(), &options(directory.path()))
        .err()
        .expect("unsupported named invocation should fail preflight");
    assert!(format!("{error:#}").contains("quoted direct native invocation"));
    assert!(!marker.exists(), "preflight must precede the first command");
}

#[cfg(unix)]
#[test]
fn library_context_rejects_named_and_cached_overrides_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("must-not-exist");
    let path = serde_json::to_string(marker.to_str().unwrap()).unwrap();
    let source = format!(
        "run \"/bin/sh\" with arguments [\"-c\", \"touch \\\"$1\\\"\", \"sh\", {path}]\nrun \"/bin/true\"\n"
    );

    let mut named = parser::parse(&source).unwrap();
    let StepKind::Invoke { call, .. } = &mut named.steps[1].kind else {
        panic!("expected a direct invocation");
    };
    call.direct = false;
    call.discard_stderr = true;
    let error = runtime::run_with_context(
        &named,
        &Registry::default(),
        &options(directory.path()),
        BTreeMap::new(),
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .expect("named invocation must fail before the first command");
    assert!(format!("{error:#}").contains("requires a quoted direct native invocation"));
    assert!(!marker.exists());

    let mut cached = parser::parse(&source).unwrap();
    let StepKind::Invoke { call, .. } = &mut cached.steps[1].kind else {
        panic!("expected a direct invocation");
    };
    call.discard_stderr = true;
    call.cache = true;
    let error = runtime::run_with_context(
        &cached,
        &Registry::default(),
        &options(directory.path()),
        BTreeMap::new(),
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .expect("discard plus cache must fail before the first command");
    assert!(format!("{error:#}").contains("cannot be combined with `and cache`"));
    assert!(!marker.exists());
}

#[cfg(unix)]
#[test]
fn stream_entrypoint_rejects_library_built_named_override_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("must-not-exist");
    let path = serde_json::to_string(marker.to_str().unwrap()).unwrap();
    let source = format!(
        "stream from \"/bin/sh\" with arguments [\"-c\", \"touch \\\"$1\\\"; printf x\", \"sh\", {path}]\npipe to \"cat\"\nshow each line\n"
    );
    let mut stream = flow::stream::parse(&source).unwrap();
    let StepKind::Invoke { call, .. } = &mut stream.pipeline.steps[1].kind else {
        panic!("expected a downstream invocation");
    };
    call.direct = false;
    call.discard_stderr = true;

    let result = runtime::run_stream(
        &stream,
        &Registry::default(),
        &options(directory.path()),
        1024,
        |_, _, _| Ok(()),
    );
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("stream validation must reject the unsupported modifier"),
    };
    assert!(format!("{error:#}").contains("requires a quoted direct native invocation"));
    assert!(!marker.exists());
}

#[test]
fn discarded_stderr_cannot_be_added_to_deserialized_cache_or_job_calls() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("must-not-exist");
    let path = serde_json::to_string(marker.to_str().unwrap()).unwrap();
    let source =
        format!("run \"/bin/sh\" with arguments [\"-c\", \"touch \\\"$1\\\"\", \"sh\", {path}]\n");
    let mut cached = parser::parse(&source).unwrap();
    let StepKind::Invoke { call, .. } = &mut cached.steps[0].kind else {
        panic!();
    };
    call.discard_stderr = true;
    call.cache = true;
    let error = runtime::run(&cached, &Registry::default(), &options(directory.path()))
        .err()
        .expect("discard plus cache should fail preflight");
    assert!(format!("{error:#}").contains("cannot be combined with `and cache`"));
    assert!(!marker.exists());

    let mut job = parser::parse(&format!(
        "{source}spawn as child\n  run \"/bin/sh\" with arguments [\"-c\", \"exit 0\"]\nawait child\n"
    ))
    .unwrap();
    let StepKind::SpawnJob { stages, .. } = &mut job.steps[1].kind else {
        panic!();
    };
    stages[0].call.discard_stderr = true;
    let error = runtime::run(&job, &Registry::default(), &options(directory.path()))
        .err()
        .expect("discarded background stderr should fail preflight");
    assert!(format!("{error:#}").contains("background pipeline stages require captured stderr"));
    assert!(
        !marker.exists(),
        "job preflight must precede the first command"
    );
}

#[test]
fn listener_source_cannot_silently_drop_the_stderr_modifier() {
    let mut program = parser::parse(
        "listen to stdin\nwhenever a line arrives as event\n  run \"printf\" with arguments [event]\n",
    )
    .unwrap();
    let Some(Trigger::Listen {
        source: flow::ast::ListenSource::Stdin,
        ..
    }) = &mut program.trigger
    else {
        panic!("expected stdin listener");
    };
    program.trigger = Some(Trigger::Listen {
        source: flow::ast::ListenSource::Applet(flow::ast::Invocation {
            operation: "producer".into(),
            direct: true,
            discard_stderr: true,
            parameters: BTreeMap::new(),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        }),
        binding: "event".into(),
        line: 1,
    });
    let error = runtime::validate(&program, &Registry::default()).unwrap_err();
    assert!(format!("{error:#}").contains("listener sources cannot use `and discard stderr`"));
}

#[test]
fn graph_nodes_reject_stderr_discard_instead_of_lowering_it_away() {
    let error = flow::graph::parse(
        "graph unsupported_override\nrun \"printf\" with arguments [\"x\"] as output and discard stderr\nend graph\n",
        &Registry::default(),
    )
    .unwrap_err();
    assert!(format!("{error:#}")
        .contains("graph node \"output\" does not support `and discard stderr`"));
}
