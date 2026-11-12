//! A checkpointed run and its resume each write a run record, and both records carry the
//! same checkpoint lineage digest (#544). The records are driven through the library the
//! way `record_cli::run_recorded_checkpointed` drives them: a recorder observes the run,
//! and the journal's digest reaches the recorder as soon as the journal opens.

#![cfg(feature = "records")]

use flow::ast::Program;
use flow::manifest::Registry;
use flow::record::{self, RecordStart, Recorder, RunSummary, SealKey};
use flow::runtime::{self, CheckpointOptions, RunOptions, RunResult};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// A fixed audit key, so the tests never depend on the user's per-user key.
static AUDIT_KEY: [u8; 32] = [0x44; 32];

const PROGRAM: &str = "fail with message \"stop here\"\n";

fn start_recorder(directory: &Path, registry: &Registry, options: &RunOptions) -> Arc<Recorder> {
    Arc::new(
        Recorder::start(RecordStart {
            directory,
            program_source: PROGRAM.as_bytes(),
            plan: "plan",
            inputs: json!({}),
            registry,
            working_directory: &options.working_directory,
            audit_key: Some(AUDIT_KEY),
            capability_policy_path: None,
            data_flow_policy_path: None,
            program_arguments: &[],
            allow: &[],
            what_if_fingerprint: None,
        })
        .unwrap(),
    )
}

fn finish(recorder: &Recorder, result: &anyhow::Result<RunResult>) {
    let summary = match result {
        Ok(result) => {
            let exit_status = result.exit.as_ref().map(|exit| i32::from(exit.status));
            RunSummary {
                ok: exit_status.unwrap_or(0) == 0,
                exit_status,
                error: result.exit.as_ref().and_then(|exit| exit.message.clone()),
                output: result.output.clone(),
            }
        }
        Err(error) => RunSummary {
            ok: false,
            exit_status: None,
            error: Some(format!("{error:#}")),
            output: Vec::new(),
        },
    };
    recorder.finish(&summary).unwrap();
}

/// One checkpointed run (or resume, when `checkpoint.resume` is set) with its record.
fn record_checkpointed(
    directory: &Path,
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    checkpoint: &CheckpointOptions,
) -> anyhow::Result<RunResult> {
    let recorder = start_recorder(directory, registry, options);
    let sink_recorder = recorder.clone();
    let sink: record::CheckpointSink =
        Arc::new(move |digest: &str| sink_recorder.set_checkpoint(digest));
    let guard = runtime::observe::install(recorder.clone()).unwrap();
    let result = record::with_checkpoint_sink(sink, || {
        runtime::run_checkpointed(program, registry, options, checkpoint)
    });
    drop(guard);
    finish(&recorder, &result);
    result
}

fn run_document(directory: &Path) -> Value {
    serde_json::from_slice(&fs::read(directory.join(record::RUN_FILE)).unwrap()).unwrap()
}

#[test]
fn a_checkpointed_run_and_its_resume_write_records_with_the_same_lineage_digest() {
    let temp = tempfile::tempdir().unwrap();
    let program_path = temp.path().join("job.program");
    fs::write(&program_path, PROGRAM).unwrap();
    let program = flow::parser::parse(PROGRAM).unwrap();
    let registry = Registry::default();
    let options = RunOptions::new(temp.path().to_owned());
    let checkpoint = CheckpointOptions {
        directory: temp.path().join("checkpoint"),
        program_path,
        resume: false,
        retry_uncertain: false,
    };

    // The first run stops at its failing step, after its journal is open.
    let first = temp.path().join("first-record");
    let _ = record_checkpointed(&first, &program, &registry, &options, &checkpoint);
    let resume = CheckpointOptions {
        resume: true,
        retry_uncertain: true,
        ..checkpoint.clone()
    };
    let second = temp.path().join("second-record");
    let _ = record_checkpointed(&second, &program, &registry, &options, &resume);

    let digest = run_document(&first)["checkpoint"]
        .as_str()
        .expect("the first record names its checkpoint")
        .to_owned();
    assert_eq!(digest.len(), 64);
    assert_eq!(run_document(&second)["checkpoint"], json!(digest));
    for directory in [&first, &second] {
        let text =
            record::show_run_with_key(directory, false, SealKey::Explicit(&AUDIT_KEY)).unwrap();
        assert!(
            text.contains(&format!("checkpoint lineage: {digest}")),
            "{text}"
        );
        let report: Value = serde_json::from_str(
            &record::show_run_with_key(directory, true, SealKey::Explicit(&AUDIT_KEY)).unwrap(),
        )
        .unwrap();
        assert_eq!(report["checkpoint"], json!(digest));
    }
}

#[test]
fn a_recorded_run_without_a_checkpoint_has_no_checkpoint_field() {
    let temp = tempfile::tempdir().unwrap();
    let program = flow::parser::parse(PROGRAM).unwrap();
    let registry = Registry::default();
    let options = RunOptions::new(temp.path().to_owned());
    let directory = temp.path().join("plain-record");
    let recorder = start_recorder(&directory, &registry, &options);
    let guard = runtime::observe::install(recorder.clone()).unwrap();
    let result = runtime::run(&program, &registry, &options);
    drop(guard);
    finish(&recorder, &result);
    assert!(run_document(&directory).get("checkpoint").is_none());
    let text = record::show_run_with_key(&directory, false, SealKey::Explicit(&AUDIT_KEY)).unwrap();
    assert!(!text.contains("checkpoint lineage"), "{text}");
}
