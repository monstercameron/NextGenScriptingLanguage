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
) -> Result<RunResult> {
    if program.trigger.is_some() {
        bail!("--record applies to finite programs; triggered programs cannot be recorded");
    }
    if let Some(reason) = replay::unsupported_reason(program) {
        bail!("--record does not support {reason}");
    }
    let source =
        fs::read(source_path).with_context(|| format!("reading {}", source_path.display()))?;
    let plan = runtime::explain_with_inputs(program, registry, &options.inputs)?;
    let recorder = Arc::new(Recorder::start(RecordStart {
        directory,
        program_source: &source,
        plan: &plan,
        inputs: record::inputs_json_for(&options.inputs, &program.inputs),
        registry,
    })?);
    let guard = runtime::observe::install(recorder.clone())?;
    let result = runtime::with_streamed_stdout(|| runtime::run(program, registry, options));
    drop(guard);
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
    result
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
    let source_path = program_path.unwrap_or_else(|| replay::stored_program_path(&directory));
    let source = fs::read(&source_path)
        .with_context(|| format!("reading program {}", source_path.display()))?;
    let edited = !replay::program_matches_record(&record, &source);
    let text = std::str::from_utf8(&source).context("the program is not UTF-8")?;
    let program = flow::program_files::parse_source(&source_path, text, None)
        .with_context(|| format!("parsing {}", source_path.display()))?;
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
    let replayer = Arc::new(Replayer::new(record, &registry, until)?);
    let guard = runtime::observe::install(replayer.clone())?;
    let options = RunOptions {
        inputs,
        max_output_bytes: 16 * 1024 * 1024,
        ..RunOptions::new(env::current_dir().context("could not determine current directory")?)
    };
    let result = runtime::with_streamed_stdout(|| runtime::run(&program, &registry, &options));
    drop(guard);
    if result.is_ok() {
        replayer.finish_check();
    }
    let stop = replayer.stop();
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
