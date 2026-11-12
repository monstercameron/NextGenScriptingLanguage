//! Command-line glue for run records and replay: `run --record DIR`, `show-run`,
//! and `replay`. The recording and replay logic lives in the library crate.

use anyhow::{anyhow, bail, Context, Result};
use flow::ast::{Program, Value};
use flow::record::{self, RecordStart, Recorder, RunSummary, StoredRecord};
use flow::replay::{self, Replayer, Stop};
use flow::runtime::{self, RunOptions, RunResult};
use flow::terminal_text::escape_controls;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Run a finite program and keep its record in `directory`. The record is written
/// whatever the outcome, and the run's own result is returned unchanged.
pub fn run_recorded(
    directory: &Path,
    source_path: &Path,
    program: &Program,
    registry: &flow::manifest::Registry,
    options: &RunOptions,
    drain_timeout: std::time::Duration,
    provenance: record::RecordProvenance<'_>,
) -> Result<RunResult> {
    let recorder = start_recording(
        directory,
        source_path,
        program,
        registry,
        options,
        provenance,
    )?;
    let guard = runtime::observe::install(recorder.clone())?;
    let result = runtime::with_streamed_stdout(|| {
        runtime::run_with_drain_timeout(program, registry, options, drain_timeout)
    });
    drop(guard);
    finish_recording(&recorder, directory, source_path, result)
}

/// Run a finite program through the checkpointed runner and keep its record in `directory`.
/// A fresh checkpoint writes the first record of its lineage; a resume (`checkpoint.resume`)
/// writes a further record for the same checkpoint. Each header carries the checkpoint's
/// lineage digest, so the records of one lineage link to each other.
// Unused until main.rs routes `--record` with `--checkpoint` here (#544 NEEDS).
#[allow(dead_code)]
pub fn run_recorded_checkpointed(
    directory: &Path,
    source_path: &Path,
    program: &Program,
    registry: &flow::manifest::Registry,
    options: &RunOptions,
    checkpoint: &runtime::CheckpointOptions,
    provenance: record::RecordProvenance<'_>,
) -> Result<RunResult> {
    let recorder = start_recording(
        directory,
        source_path,
        program,
        registry,
        options,
        provenance,
    )?;
    let sink_recorder = recorder.clone();
    let sink: record::CheckpointSink =
        Arc::new(move |digest: &str| sink_recorder.set_checkpoint(digest));
    let guard = runtime::observe::install(recorder.clone())?;
    let result = runtime::with_streamed_stdout(|| {
        record::with_checkpoint_sink(sink, || {
            runtime::run_checkpointed(program, registry, options, checkpoint)
        })
    });
    drop(guard);
    finish_recording(&recorder, directory, source_path, result)
}

/// Create the record for a run: the program copy, the plan, the inputs and the loop budget.
fn start_recording(
    directory: &Path,
    source_path: &Path,
    program: &Program,
    registry: &flow::manifest::Registry,
    options: &RunOptions,
    provenance: record::RecordProvenance<'_>,
) -> Result<Arc<Recorder>> {
    if program.trigger.is_some() {
        bail!("--record applies to finite programs; triggered programs cannot be recorded");
    }
    if let Some(reason) = replay::unsupported_reason(program) {
        bail!("--record does not support {reason}");
    }
    let source =
        fs::read(source_path).with_context(|| format!("reading {}", source_path.display()))?;
    let plan = runtime::explain_with_inputs(program, registry, &options.inputs)?;
    // The seal needs the per-user key. A key that cannot be read or created is
    // not a failed run: the record is written unsealed and `show-run` says
    // so — but the downgrade is said here too, so a missing or unreadable key
    // is never silent at record time.
    let audit_key = match flow::agent::what_if_key::load_or_create() {
        Ok(key) => Some(key),
        Err(error) => {
            eprintln!(
                "record: the per-user audit key is unavailable ({error:#}); writing an unsealed record"
            );
            None
        }
    };
    let recorder = Arc::new(Recorder::start(RecordStart {
        directory,
        program_source: &source,
        plan: &plan,
        inputs: record::inputs_json_for(&options.inputs, &program.inputs),
        registry,
        working_directory: &options.working_directory,
        audit_key,
        capability_policy_path: provenance.capability_policy_path,
        data_flow_policy_path: provenance.data_flow_policy_path,
        program_arguments: provenance.program_arguments,
        allow: provenance.allow,
        what_if_fingerprint: provenance.what_if_fingerprint,
    })?);
    recorder.set_loop_budget(options.max_loop_iterations, options.loop_budget_scope)?;
    Ok(recorder)
}

/// Write the run's outcome into its record and print the repair hints for a failure.
fn finish_recording(
    recorder: &Recorder,
    directory: &Path,
    source_path: &Path,
    result: Result<RunResult>,
) -> Result<RunResult> {
    let summary = match &result {
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
    recorder.finish(&summary)?;
    if !summary.ok {
        // The first failure usually has no checkpoint, since agents rarely know
        // in advance which run will fail. Name the repair loop so the failure
        // itself is repairable and resumable without re-running completed steps.
        eprintln!(
            "next: execute show-run {} to diagnose the failure",
            directory.display()
        );
        eprintln!(
            "next: execute repair --from-record {} --program {} --op '<operation>', then execute resume {} --from-record {}",
            directory.display(),
            source_path.display(),
            source_path.display(),
            directory.display()
        );
    }
    result
}

/// `execute resume FILE --from-record DIR`: run the repaired program with the
/// record's completed prefix replayed from captured results.
///
/// Every recorded step before the record's first failed step is answered from
/// the record — no process is launched and no file is written for those steps.
/// The repaired suffix runs natively. Inputs and the loop budget come from the
/// record; a group in the completed prefix that no longer matches is refused
/// rather than re-run, and a group whose bytes were not recorded is refused
/// with its step index and reason.
///
/// The completed prefix itself is pinned: the resume program must keep every
/// byte before the recorded boundary, and a truncated, torn, or
/// provenance-mismatched record refuses the resume instead of re-running
/// completed effects.
pub fn resume_from_record(
    program: &Program,
    source_path: &Path,
    source: &str,
    record_directory: &Path,
    registry: &flow::manifest::Registry,
    base_options: &RunOptions,
) -> Result<()> {
    let record = StoredRecord::open(record_directory)?;
    if record.run["result"]["ok"].as_bool() == Some(true) {
        bail!("the record completed successfully; there is nothing to resume");
    }
    record::check_from_record_usable(&record, registry, &base_options.working_directory)?;
    if let Some(reason) = replay::unsupported_reason(program) {
        bail!("resume --from-record does not support {reason}");
    }
    let inputs = replay::recorded_inputs(&record)?;
    let (max_loop_iterations, loop_budget_scope) = replay::recorded_loop_budget(&record)?;
    // The boundary comes from the recorded program, never from the program
    // being resumed: line numbers in the untrusted file must not move the
    // line that suppresses the completed prefix's file writes.
    let recorded_source = record.program_source()?;
    let recorded_text =
        std::str::from_utf8(&recorded_source).context("the recorded program is not UTF-8")?;
    let (boundary, failed_line) =
        flow::repair::from_record_boundary(recorded_text, Some(source_path), &record)?;
    flow::repair::check_resume_prefix(recorded_text, source, boundary)?;
    let replayer = Arc::new(replay::PrefixReplayer::new(record, registry, boundary)?);
    eprintln!(
        "resume: {} recorded steps, {} replayable{}",
        replayer.record().steps.len(),
        replayer.prefix(),
        failed_line
            .map(|line| format!(" (first failure at line {line})"))
            .unwrap_or_default()
    );
    let guard = runtime::observe::install(replayer.clone())?;
    let mut options = base_options.clone();
    options.inputs = inputs;
    options.max_loop_iterations = max_loop_iterations;
    options.loop_budget_scope = loop_budget_scope;
    let result = runtime::with_streamed_stdout(|| runtime::run(program, registry, &options));
    drop(guard);
    let result = result?;
    io::stdout()
        .write_all(&result.output)
        .context("could not write workflow output")?;
    eprintln!(
        "resume: {} steps replayed from {}, suffix ran natively, {} processes launched",
        replayer.steps_replayed(),
        record_directory.display(),
        result.launched_processes
    );
    if let Some(exit) = result.exit {
        io::stdout()
            .flush()
            .context("could not write workflow output")?;
        if let Some(message) = &exit.message {
            let message = flow::security::secret_redactor(registry).redact_text(message);
            runtime::write_diagnostic(format!("{message}\n").as_bytes());
        }
        std::process::exit(i32::from(exit.status));
    }
    Ok(())
}

/// `execute show-run RECORD [--json]`.
pub fn show_run(arguments: &[String]) -> Result<()> {
    let mut directory = None;
    let mut as_json = false;
    for argument in arguments {
        match argument.as_str() {
            "--json" => as_json = true,
            option if option.starts_with("--") => bail!("unknown show-run option {option:?}"),
            path if directory.is_none() => directory = Some(PathBuf::from(path)),
            _ => bail!("show-run accepts one record directory"),
        }
    }
    let directory = directory.context("usage: execute show-run RECORD [--json]")?;
    let text = record::show_run(&directory, as_json)?;
    let text = if as_json {
        text
    } else {
        escape_controls(&text)
    };
    io::stdout().write_all(text.as_bytes())?;
    Ok(())
}

/// `execute replay RECORD [--program FILE] [--until LINE] [--as-test DIR] [--manifest-dir DIR]`.
pub fn replay(arguments: &[String]) -> Result<()> {
    let mut directory = None;
    let mut program_path = None;
    let mut until = None;
    let mut as_test = None;
    let mut manifest_dir = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        let mut value = || -> Result<String> {
            index += 1;
            arguments
                .get(index)
                .cloned()
                .with_context(|| format!("{argument} requires a value"))
        };
        match argument {
            "--program" => program_path = Some(PathBuf::from(value()?)),
            "--until" => {
                let text = value()?;
                until = Some(
                    text.parse::<usize>()
                        .with_context(|| format!("--until expects a line number, not {text:?}"))?,
                );
            }
            "--as-test" => as_test = Some(PathBuf::from(value()?)),
            "--manifest-dir" => manifest_dir = Some(PathBuf::from(value()?)),
            option if option.starts_with("--") => bail!("unknown replay option {option:?}"),
            path if directory.is_none() => directory = Some(PathBuf::from(path)),
            _ => bail!("replay accepts one record directory"),
        }
        index += 1;
    }
    let directory = directory.context("usage: execute replay RECORD [--until LINE]")?;
    let record = StoredRecord::open(&directory)?;
    record::check_replayable_after_redaction(&record)?;
    // A record edited after the run replays nothing: refuse it before any
    // blob is trusted. Records without a seal predate it and keep the older
    // behavior.
    record::check_record_seal_for_reuse(&record)?;
    let source_path = if let Some(path) = program_path.as_ref() {
        fs::canonicalize(path).with_context(|| format!("resolving program {}", path.display()))?
    } else {
        record.program_path()
    };
    let source = if let Some(path) = program_path {
        fs::read(&path).with_context(|| format!("reading program {}", path.display()))?
    } else {
        record.program_source()?
    };
    let edited = !replay::program_matches_record(&record, &source);
    let program = parse_replay_program(&source_path, &source)?;
    if let Some(reason) = replay::unsupported_reason(&program) {
        bail!("replay does not support {reason}");
    }
    // A secret answer was withheld from the record, so a replay cannot reproduce the
    // run. Refusing here is better than replaying with a marker in its place.
    if let Some(name) = record.run["inputs"]
        .as_object()
        .and_then(|fields| {
            fields
                .iter()
                .find(|(_, value)| value.get("withheld").is_some())
        })
        .map(|(name, _)| name.clone())
    {
        bail!(
            "replay cannot reproduce the secret answer for input {name:?}; the record withholds it"
        );
    }
    let inputs: BTreeMap<String, Value> = serde_json::from_value(record.run["inputs"].clone())
        .context("the record's inputs cannot be read")?;
    let registry = crate::load_workflow_registry(
        manifest_dir,
        None,
        flow::manifest::ManifestCheck::Lazy,
        None,
        None,
    )?;
    // Records written before per-loop limits used 100,000 as their default.
    let recorded_limit = record.run["limits"].get("loop_iterations_max");
    let max_loop_iterations = match recorded_limit {
        None | Some(serde_json::Value::Null) => 100_000,
        Some(value) => usize::try_from(value.as_u64().context("invalid recorded loop limit")?)
            .context("recorded loop limit exceeds this platform")?,
    };
    let loop_budget_scope = match record.run["limits"].get("loop_iterations_scope") {
        None | Some(serde_json::Value::Null)
            if recorded_limit.is_none_or(serde_json::Value::is_null) =>
        {
            runtime::LoopBudgetScope::LegacySharedRun
        }
        None | Some(serde_json::Value::Null) => runtime::LoopBudgetScope::PerLoop,
        Some(serde_json::Value::String(scope)) if scope == "per_loop" => {
            runtime::LoopBudgetScope::PerLoop
        }
        Some(serde_json::Value::String(scope)) if scope == "legacy_shared_run" => {
            runtime::LoopBudgetScope::LegacySharedRun
        }
        _ => bail!("invalid recorded loop budget scope"),
    };
    let replayer = Arc::new(Replayer::new(record, &registry, until)?);
    let guard = runtime::observe::install(replayer.clone())?;
    let options = RunOptions {
        inputs,
        max_output_bytes: 16 * 1024 * 1024,
        max_loop_iterations,
        loop_budget_scope,
        ..RunOptions::new(env::current_dir().context("could not determine current directory")?)
    };
    let result = runtime::with_streamed_stdout(|| {
        runtime::without_executable_preflight(|| runtime::run(&program, &registry, &options))
    });
    drop(guard);
    if result.is_ok() {
        replayer.finish_check();
    }
    let stop = replayer.stop();
    let exit = replay::completed_exit(
        &stop,
        result.as_ref().ok().and_then(|result| result.exit.clone()),
    );
    if edited {
        eprintln!(
            "replay: the program differs from the recorded one; the first divergence is reported"
        );
    }
    match (stop, result) {
        (Stop::Completed, Ok(result)) => {
            io::stdout()
                .write_all(&result.output)
                .context("could not write workflow output")?;
            eprintln!(
                "replay: {} steps replayed, no divergence, {} processes launched",
                replayer.steps_replayed(),
                result.launched_processes
            );
            if let Some(target) = as_test {
                write_test_fixture(&target, &source, replayer.record())?;
                eprintln!("replay: wrote a test fixture to {}", target.display());
            }
            // Report the replayed program's explicit exit the same way a
            // normal run does, after the replay diagnostics above.
            if let Some(exit) = exit {
                io::stdout()
                    .flush()
                    .context("could not write workflow output")?;
                if let Some(message) = &exit.message {
                    let message = flow::security::secret_redactor(&registry).redact_text(message);
                    runtime::write_exit_diagnostic(format!("{message}\n").as_bytes());
                }
                std::process::exit(i32::from(exit.status));
            }
            Ok(())
        }
        (
            Stop::UntilLine {
                line,
                steps_replayed,
            },
            _,
        ) => {
            eprintln!(
                "replay: stopped before line {line} after {steps_replayed} steps; no divergence"
            );
            Ok(())
        }
        (Stop::Diverged(divergence), _) => Err(anyhow!(
            "replay diverged at step {} (line {}): the record expected {}, the program ran {}",
            divergence.step,
            divergence.line,
            divergence.expected,
            divergence.actual
        )),
        (Stop::UnusedRecordedSteps { next, line }, _) => Err(anyhow!(
            "the program finished before recorded step {next} (line {line}) was replayed"
        )),
        (Stop::Completed, Err(error)) => Err(error),
    }
}

/// Parse captured source bytes using the stable path identity resolved before
/// the record directory was pinned. Replay has no file-read access, so the
/// parser must not canonicalize this path after the parent can be replaced.
fn parse_replay_program(source_path: &Path, source: &[u8]) -> Result<Program> {
    let source_path = if source_path.is_absolute() {
        source_path.to_path_buf()
    } else {
        env::current_dir()
            .context("could not determine current directory")?
            .join(source_path)
    };
    let text = std::str::from_utf8(source).context("the program is not UTF-8")?;
    flow::program_files::parse_source_resolved(&source_path, text, None)
        .with_context(|| format!("parsing {}", source_path.display()))
}

/// Write the record as a test for `execute test`: the program, its recorded inputs,
/// and the recorded exit status and stdout. Existing files are not overwritten.
fn write_test_fixture(target: &Path, source: &[u8], record: &StoredRecord) -> Result<()> {
    let stem = target
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || character == '-' {
                        character
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .unwrap_or_else(|| "recorded".into());
    let expected_status = record.run["result"]["exit_status"].as_i64().unwrap_or(0);
    // The terminal stream is what a test run would print: streamed output, then the
    // final value.
    let stdout = record.blob(&record.run["terminal"])?;
    let stdout = String::from_utf8(stdout)
        .context("the recorded stdout is not UTF-8, so it cannot be a test expectation")?;
    let sidecar = serde_json::json!({
        "inputs": replay::inputs_as_strings(record),
        "expect": { "status": expected_status, "stdout": stdout },
    });
    fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    write_new(&target.join(format!("{stem}.test.program")), source)?;
    write_new(
        &target.join(format!("{stem}.test.json")),
        serde_json::to_string_pretty(&sidecar)?.as_bytes(),
    )
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {} (it must not exist)", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod replay_source_tests {
    #[cfg(unix)]
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[cfg(unix)]
    #[test]
    fn replay_keeps_canonical_source_identity_after_record_parent_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let directory = target.join("record");
        fs::create_dir_all(&directory).unwrap();
        let original_source = b"accept where as text from program path\naccept folder as text from program directory\n";
        fs::write(directory.join(record::PROGRAM_FILE), original_source).unwrap();
        fs::write(
            directory.join(record::RUN_FILE),
            br#"{"protocol":"execute.record/1","finished":false,"steps":0}"#,
        )
        .unwrap();
        fs::write(directory.join(record::TRACE_FILE), b"").unwrap();
        let alias = temp.path().join("alias");
        symlink(&target, &alias).unwrap();
        let stored = StoredRecord::open(&alias.join("record")).unwrap();
        let expected_source_path = fs::canonicalize(&directory)
            .unwrap()
            .join(record::PROGRAM_FILE);
        let source_path = stored.program_path();
        assert_eq!(source_path, expected_source_path);

        let displaced = temp.path().join("displaced-target");
        fs::rename(&target, &displaced).unwrap();
        fs::create_dir(&target).unwrap();
        let replacement = temp.path().join("replacement");
        fs::create_dir(&replacement).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&replacement, &alias).unwrap();

        let source = stored.program_source().unwrap();
        assert_eq!(source, original_source);
        let program = parse_replay_program(&source_path, &source).unwrap();
        assert_eq!(
            program.inputs[0].default,
            Some(Value::String(source_path.to_string_lossy().into_owned()))
        );
        assert_eq!(
            program.inputs[1].default,
            Some(Value::String(
                source_path.parent().unwrap().to_string_lossy().into_owned()
            ))
        );
        let error = parse_replay_program(&source_path, b"run program file \"callee.program\"\n")
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("needs the program's capability policy"),
            "{error:#}"
        );
    }
}
