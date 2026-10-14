use anyhow::{anyhow, bail, Context, Result};
use flow::ast::Trigger;
use flow::manifest::Registry;
use flow::parser;
use flow::runtime::{self, CheckpointOptions, RunOptions, ServiceLimits};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Args {
    inputs: BTreeMap<String, String>,
    json: bool,
    command: String,
    file: PathBuf,
    manifest_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    checkpoint_dir: Option<PathBuf>,
    retry_uncertain: bool,
    trace: bool,
    headless: bool,
    max_output_bytes: usize,
    max_runs: Option<usize>,
    command_timeout: Option<std::time::Duration>,
    max_record_bytes: usize,
    max_record_bytes_set: bool,
}

fn main() {
    if let Err(error) = execute() {
        runtime::write_diagnostic(format!("execute: {error:#}\n").as_bytes());
        std::process::exit(1);
    }
}

fn execute() -> Result<()> {
    let arguments = env::args_os()
        .skip(1)
        .enumerate()
        .map(|(index, argument)| {
            argument.into_string().map_err(|_| {
                anyhow!("argument {} is not valid UTF-8; use a percent-encoded file:// URI for non-UTF-8 workflow paths", index + 1)
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if arguments.first().is_some_and(|a| a == "live") {
        return flow::live::cli(&arguments[1..], default_manifest_dir(), workflow_path);
    }
    if arguments.first().is_some_and(|a| a == "model") {
        let helper = runtime::bundled_model_applet()?;
        let error = std::process::Command::new(helper)
            .args(&arguments[1..])
            .exec();
        return Err(error).context("starting the bundled model applet");
    }
    let args = match parse_args(arguments)? {
        Some(args) => args,
        None => return Ok(()),
    };

    if args.json {
        let path = args.file.to_string_lossy().into_owned();
        let path_bytes = args.file.as_os_str().as_bytes().to_vec();
        let result = execute_workflow(args);
        let diagnostics = result
            .as_ref()
            .err()
            .map(|error| {
                serde_json::json!({
                    "severity": "error", "code": "check_failed",
                    "message": format!("{error:#}"),
                    "causes": error.chain().map(ToString::to_string).collect::<Vec<_>>()
                })
            })
            .into_iter()
            .collect::<Vec<_>>();
        write_json(&serde_json::json!({
            "protocol": "execute.check/1", "validation": "static",
            "ok": result.is_ok(), "path": path, "path_bytes": path_bytes,
            "diagnostics": diagnostics
        }))?;
        if result.is_err() {
            // Validation has no spawned workers/effects. The JSON response is
            // complete; preserve the ordinary failing-check exit status.
            std::process::exit(1);
        }
        return Ok(());
    }
    execute_workflow(args)
}

fn execute_workflow(args: Args) -> Result<()> {
    let source = fs::read_to_string(&args.file)
        .with_context(|| format!("could not read workflow {}", args.file.display()))?;
    if flow::ui::syntax::is_screen(&source) {
        if !args.inputs.is_empty() {
            bail!("screen programs use component input ports and live widget state, not --input");
        }
        let screen = flow::ui::syntax::parse(&source).context("parsing UI")?;
        let registry = load_registry(args.manifest_dir)?;
        flow::ui::syntax::validate(&screen, &registry)?;
        if args.command == "resume" || args.checkpoint_dir.is_some() || args.max_record_bytes_set {
            bail!("UI programs do not support resume, checkpoints, or listener record limits");
        }
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: env::current_dir()?,
            cache_directory: args.cache_dir,
            trace: args.trace,
            max_output_bytes: args.max_output_bytes,
            command_timeout: args.command_timeout,
        };
        return match args.command.as_str() {
            "check" => Ok(()),
            "inspect" => {
                let mut description = flow::inputs::describe(&[], &BTreeMap::new())?;
                description["kind"] = "screen".into();
                description["stdin_mode"] = "terminal".into();
                description["title"] = screen.title.clone().into();
                description["placements"] = serde_json::to_value(&screen.placements)?;
                description["components"] = screen.components.iter().map(|(name, component)| {
                    serde_json::json!({
                        "name": name,
                        "inputs": component.inputs.iter().map(|name| serde_json::json!({"name": name, "type": "text"})).collect::<Vec<_>>(),
                        "outputs": component.outputs.iter().map(|name| serde_json::json!({"name": name, "type": "text"})).collect::<Vec<_>>()
                    })
                }).collect::<Vec<_>>().into();
                write_json(&description)
            }
            "explain" => {
                writeln!(
                    std::io::stdout().lock(),
                    "screen {:?}; components {:?}; connections {}",
                    screen.title,
                    screen.placements,
                    screen.links.len()
                )?;
                for component in screen.components.values() {
                    for handler in &component.handlers {
                        let steps = handler
                            .actions
                            .iter()
                            .filter_map(|a| {
                                if let flow::ui::syntax::Action::Native(step) = a {
                                    Some(step.clone())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>();
                        for step in steps {
                            if let flow::ast::StepKind::Invoke { call, .. } = step.kind {
                                let manifest = registry.resolve_call(&call)?;
                                writeln!(
                                    std::io::stdout().lock(),
                                    "line {}: {:?} parameters {:?}",
                                    step.line,
                                    manifest.binary_for(&call),
                                    call.parameters.keys().collect::<Vec<_>>()
                                )?;
                            }
                        }
                    }
                }
                Ok(())
            }
            "run" if args.headless => {
                if args.max_runs.is_some() {
                    bail!("--max-runs cannot be combined with --headless; headless UI runs initialization once");
                }
                writeln!(
                    std::io::stdout().lock(),
                    "{}",
                    serde_json::to_string_pretty(&flow::ui::snapshot(
                        &screen, &registry, &options
                    )?)?
                )?;
                Ok(())
            }
            "run" => flow::ui::run(&screen, &registry, &options, args.max_runs),
            _ => unreachable!(),
        };
    }
    if flow::stream::is_stream(&source) {
        let stream = flow::stream::parse(&source).context("parsing stream")?;
        let inputs = flow::inputs::parse_cli(&stream.pipeline.inputs, &args.inputs)?;
        if args.command == "resume" || args.checkpoint_dir.is_some() || args.max_runs.is_some() {
            bail!("stream programs do not support resume, checkpoints, or --max-runs");
        }
        if args.headless {
            bail!("--headless applies only to UI programs");
        }
        let registry = load_registry(args.manifest_dir)?;

        return match args.command.as_str() {
            "inspect" => {
                flow::stream::validate(&stream, &registry)?;
                runtime::validate_with_arguments(&stream.pipeline, &registry, &inputs)?;
                let mut description = flow::inputs::describe(&stream.pipeline.inputs, &inputs)?;
                description["kind"] = "stream".into();
                description["stdin_mode"] = "unused".into();
                description["line_limit"] = serde_json::to_value(stream.line_limit)?;
                write_json(&description)
            }
            "check" => {
                flow::stream::validate(&stream, &registry)?;
                runtime::validate_with_arguments(&stream.pipeline, &registry, &inputs)
            }
            "explain" => {
                flow::stream::validate(&stream, &registry)?;
                let pipeline = runtime::explain_with_inputs(&stream.pipeline, &registry, &inputs)?;
                match stream.line_limit {
                    Some(limit) => writeln!(std::io::stdout().lock(),
                        "persistent native pipeline; raw output to stdout; take the first {limit} final records; per-record limit {} bytes\n{pipeline}",
                        args.max_record_bytes
                    )?,
                    None => writeln!(std::io::stdout().lock(),
                        "persistent native pipeline; raw output to stdout until source EOF; per-record limit {} bytes\n{pipeline}",
                        args.max_record_bytes
                    )?,
                }
                Ok(())
            }
            "run" => {
                let options = RunOptions {
                    inputs,
                    working_directory: env::current_dir()?,
                    cache_directory: args.cache_dir,
                    trace: args.trace,
                    max_output_bytes: args.max_output_bytes,
                    command_timeout: args.command_timeout,
                };
                let mut emit =
                    |bytes: &[u8],
                     cancelled: &std::sync::atomic::AtomicBool,
                     deadline: Option<std::time::Instant>| {
                        runtime::write_with_deadline(
                            libc::STDOUT_FILENO,
                            bytes,
                            cancelled,
                            deadline,
                        )
                    };
                runtime::run_stream(
                    &stream,
                    &registry,
                    &options,
                    args.max_record_bytes,
                    &mut emit,
                )?;
                Ok(())
            }
            _ => unreachable!("argument parser only accepts supported commands"),
        };
    }
    if args.headless {
        bail!("--headless applies only to UI programs");
    }
    let program = parser::parse(&source)
        .with_context(|| format!("could not parse workflow {}", args.file.display()))?;
    let inputs = flow::inputs::parse_cli(&program.inputs, &args.inputs)?;
    if args.max_runs.is_some() && program.trigger.is_none() {
        bail!("--max-runs applies only to triggered programs");
    }
    if args.max_record_bytes_set
        && !matches!(program.trigger.as_ref(), Some(Trigger::Listen { .. }))
    {
        bail!("--max-record-bytes applies only to listener programs");
    }
    let registry = load_registry(args.manifest_dir)?;

    match args.command.as_str() {
        "inspect" => {
            runtime::validate_with_arguments(&program, &registry, &inputs)?;
            let mut description = flow::inputs::describe(&program.inputs, &inputs)?;
            if !program.definitions.is_empty() {
                description["programs"] = program.definitions.iter().map(|(name, definition)| {
                    Ok(serde_json::json!({"name":name,"line":definition.line,"inputs":flow::inputs::describe(&definition.inputs, &BTreeMap::new())?["inputs"],"output":"bytes","scope":"local","lifetime":"finite"}))
                }).collect::<Result<Vec<_>>>()?.into();
            }
            description["stdin_mode"] = if program
                .steps
                .iter()
                .any(|step| matches!(step.kind, flow::ast::StepKind::ReadStdin { .. }))
            {
                "bytes"
            } else if matches!(
                program.trigger,
                Some(Trigger::Listen {
                    source: flow::ast::ListenSource::Stdin,
                    ..
                })
            ) {
                "utf8_lines"
            } else {
                "unused"
            }
            .into();
            description["kind"] = match program.trigger {
                None => "finite",
                Some(Trigger::Every { .. }) => "interval",
                Some(Trigger::Cron { .. }) => "calendar",
                Some(Trigger::Listen { .. }) => "listener",
            }
            .into();
            write_json(&description)
        }
        "check" => {
            runtime::validate_with_arguments(&program, &registry, &inputs)?;
            Ok(())
        }
        "explain" => {
            writeln!(
                std::io::stdout().lock(),
                "{}",
                runtime::explain_with_inputs(&program, &registry, &inputs)?
            )?;
            Ok(())
        }
        "run" | "resume" => {
            // Each runtime entrypoint validates before starting effects.
            let options = RunOptions {
                inputs,
                working_directory: env::current_dir()
                    .context("could not determine current directory")?,
                cache_directory: args.cache_dir,
                trace: args.trace,
                max_output_bytes: args.max_output_bytes,
                command_timeout: args.command_timeout,
            };
            let checkpoint = args
                .checkpoint_dir
                .map(|directory| -> Result<CheckpointOptions> {
                    Ok(CheckpointOptions {
                        directory,
                        program_path: fs::canonicalize(&args.file).with_context(|| {
                            format!("could not resolve workflow path {}", args.file.display())
                        })?,
                        resume: args.command == "resume",
                        retry_uncertain: args.retry_uncertain,
                    })
                })
                .transpose()?;
            if program.trigger.is_some() {
                let limits = ServiceLimits {
                    max_runs: args.max_runs,
                    max_record_bytes: args.max_record_bytes,
                };
                let mut emit = |bytes: &[u8], cancelled: &std::sync::atomic::AtomicBool| {
                    runtime::write_cancellable(libc::STDOUT_FILENO, bytes, cancelled)
                };
                if let Some(checkpoint) = checkpoint.as_ref() {
                    runtime::run_service_checkpointed(
                        &program, &registry, &options, &limits, checkpoint, &mut emit,
                    )?;
                } else {
                    runtime::run_service_with_cancellation(
                        &program, &registry, &options, &limits, emit,
                    )?;
                }
            } else {
                let result = if let Some(checkpoint) = checkpoint.as_ref() {
                    runtime::run_checkpointed(&program, &registry, &options, checkpoint)?
                } else {
                    runtime::run(&program, &registry, &options)?
                };
                io::stdout()
                    .write_all(&result.output)
                    .context("could not write workflow output")?;
            }
            Ok(())
        }
        _ => unreachable!("argument parser only accepts supported commands"),
    }
}

/// Parse command line arguments. `None` means help or version was printed.
fn parse_args(arguments: Vec<String>) -> Result<Option<Args>> {
    if arguments.is_empty() {
        print_help()?;
        bail!("a command is required");
    }
    let (command, first_argument) = match arguments.first().map(String::as_str) {
        Some("check" | "explain" | "inspect" | "run" | "resume") => (arguments[0].clone(), 1),
        Some(_) => ("run".to_owned(), 0),
        None => {
            print_help()?;
            bail!("a workflow file is required");
        }
    };

    let mut file = None;
    let mut json = false;
    let mut inputs = BTreeMap::new();
    let mut manifest_dir = None;
    let mut cache_dir = None;
    let mut checkpoint_dir = None;
    let mut retry_uncertain = false;
    let mut trace = false;
    let mut headless = false;
    let mut max_output_bytes = 16 * 1024 * 1024;
    let mut max_output_seen = false;
    let mut max_runs = None;
    let mut command_timeout = None;
    let mut max_record_bytes = 65_536;
    let mut max_record_bytes_seen = false;
    let mut index = first_argument;
    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--json" if command == "check" => {
                if json {
                    bail!("--json may be specified only once");
                }
                json = true;
            }
            "--input" => {
                let pair = take_option_value(&arguments, &mut index, "--input", "NAME=VALUE")?;
                let (name, value) = pair
                    .split_once('=')
                    .context("--input requires NAME=VALUE")?;
                if inputs.insert(name.to_owned(), value.to_owned()).is_some() {
                    bail!("duplicate program input {name:?}");
                }
            }
            "--help" | "-h" => {
                print_help()?;
                return Ok(None);
            }
            "--version" | "-V" => {
                writeln!(
                    std::io::stdout().lock(),
                    "execute {}",
                    env!("CARGO_PKG_VERSION")
                )?;
                return Ok(None);
            }
            "--manifest-dir" => {
                if manifest_dir.is_some() {
                    bail!("--manifest-dir may be specified only once");
                }
                manifest_dir = Some(PathBuf::from(take_option_value(
                    &arguments,
                    &mut index,
                    "--manifest-dir",
                    "a directory",
                )?));
            }
            "--cache-dir" if matches!(command.as_str(), "run" | "resume") => {
                if cache_dir.is_some() {
                    bail!("--cache-dir may be specified only once");
                }
                cache_dir = Some(PathBuf::from(take_option_value(
                    &arguments,
                    &mut index,
                    "--cache-dir",
                    "a directory",
                )?));
            }
            "--checkpoint" if matches!(command.as_str(), "run" | "resume") => {
                if checkpoint_dir.is_some() {
                    bail!("--checkpoint may be specified only once");
                }
                checkpoint_dir = Some(PathBuf::from(take_option_value(
                    &arguments,
                    &mut index,
                    "--checkpoint",
                    "a directory",
                )?));
            }
            "--retry-uncertain" if command == "resume" => {
                if retry_uncertain {
                    bail!("--retry-uncertain may be specified only once");
                }
                retry_uncertain = true;
            }
            "--headless" if command == "run" => {
                if headless {
                    bail!("--headless may be specified only once");
                }
                headless = true;
            }
            "--trace" if matches!(command.as_str(), "run" | "resume") => {
                if trace {
                    bail!("--trace may be specified only once");
                }
                trace = true;
            }
            "--max-output-bytes" if matches!(command.as_str(), "run" | "resume") => {
                if max_output_seen {
                    bail!("--max-output-bytes may be specified only once");
                }
                let value = take_option_value(
                    &arguments,
                    &mut index,
                    "--max-output-bytes",
                    "a non-negative integer",
                )?;
                max_output_bytes = value
                    .parse::<usize>()
                    .map_err(|_| anyhow!("--max-output-bytes must be a non-negative integer"))?;
                max_output_seen = true;
            }
            "--command-timeout-ms" if matches!(command.as_str(), "run" | "resume") => {
                if command_timeout.is_some() {
                    bail!("--command-timeout-ms may be specified only once");
                }
                let value = parse_positive_usize(&arguments, &mut index, "--command-timeout-ms")?;
                let duration = std::time::Duration::from_millis(u64::try_from(value)?);
                std::time::Instant::now()
                    .checked_add(duration)
                    .context("command timeout is too large")?;
                command_timeout = Some(duration);
            }
            "--max-runs" if matches!(command.as_str(), "run" | "resume") => {
                if max_runs.is_some() {
                    bail!("--max-runs may be specified only once");
                }
                max_runs = Some(parse_positive_usize(&arguments, &mut index, "--max-runs")?);
            }
            "--max-record-bytes" if matches!(command.as_str(), "run" | "resume") => {
                if max_record_bytes_seen {
                    bail!("--max-record-bytes may be specified only once");
                }
                max_record_bytes =
                    parse_positive_usize(&arguments, &mut index, "--max-record-bytes")?;
                max_record_bytes_seen = true;
            }
            value if value.starts_with('-') => bail!("unknown or unsupported option `{value}`"),
            _ => {
                if file.replace(workflow_path(argument)?).is_some() {
                    bail!("only one workflow file may be specified");
                }
            }
        }
        index += 1;
    }

    let file = file.ok_or_else(|| anyhow!("missing workflow file\n{}", usage()))?;
    if command == "resume" && checkpoint_dir.is_none() {
        bail!("resume requires --checkpoint DIR");
    }
    Ok(Some(Args {
        inputs,
        json,
        command,
        file,
        manifest_dir,
        cache_dir,
        checkpoint_dir,
        retry_uncertain,
        trace,
        headless,
        max_output_bytes,
        max_runs,
        command_timeout,
        max_record_bytes,
        max_record_bytes_set: max_record_bytes_seen,
    }))
}

/// Convert a local file URI without changing ordinary path interpretation.
/// Percent escapes decode to Unix filename bytes, not form-encoded text.
fn workflow_path(argument: &str) -> Result<PathBuf> {
    if !argument
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        return Ok(PathBuf::from(argument));
    }
    let rest = &argument[5..];
    let path = if let Some(authority_path) = rest.strip_prefix("//") {
        let (authority, path) = authority_path
            .split_once('/')
            .context("file URI requires an absolute path")?;
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            bail!(
                "file URI must refer to this computer; remote host {authority:?} is not supported"
            );
        }
        format!("/{path}")
    } else {
        rest.to_owned()
    };
    if !path.starts_with('/') {
        bail!("file URI requires an absolute path");
    }
    if path.contains(['?', '#']) {
        bail!("file URI queries and fragments are not supported; encode filename characters as %3F and %23");
    }
    let mut decoded = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let value = if bytes[index] == b'%' {
            let high = bytes.get(index + 1).and_then(|v| (*v as char).to_digit(16));
            let low = bytes.get(index + 2).and_then(|v| (*v as char).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => {
                    index += 3;
                    (high * 16 + low) as u8
                }
                _ => bail!("invalid percent escape in file URI"),
            }
        } else {
            let value = bytes[index];
            index += 1;
            value
        };
        if value == 0 {
            bail!("file URI cannot contain a NUL filename byte");
        }
        decoded.push(value);
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

fn parse_positive_usize(arguments: &[String], index: &mut usize, option: &str) -> Result<usize> {
    let value = take_option_value(arguments, index, option, "a positive integer")?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| anyhow!("{option} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{option} must be greater than zero");
    }
    Ok(parsed)
}

fn take_option_value(
    arguments: &[String],
    index: &mut usize,
    option: &str,
    expected: &str,
) -> Result<String> {
    *index += 1;
    let value = arguments
        .get(*index)
        .ok_or_else(|| anyhow!("{option} requires {expected}"))?;
    if value.starts_with('-') {
        bail!("{option} requires {expected}, got `{value}`");
    }
    Ok(value.clone())
}

fn load_registry(explicit_path: Option<PathBuf>) -> Result<Registry> {
    match explicit_path {
        Some(path) => Registry::load(&path)
            .with_context(|| format!("could not load manifests from {}", path.display())),
        None => Registry::load_optional(&default_manifest_dir()),
    }
}

fn default_manifest_dir() -> PathBuf {
    let local = PathBuf::from("manifests");
    if local.is_dir() {
        return local;
    }

    if let Ok(executable) = env::current_exe() {
        // macOS may report the launch symlink rather than its bundle target.
        let executable = fs::canonicalize(&executable).unwrap_or(executable);
        if let Some(parent) = executable.parent() {
            let sibling = parent.join("..").join("manifests");
            if sibling.is_dir() {
                return sibling;
            }
        }
    }

    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn usage() -> &'static str {
    "Usage: execute FILE_OR_FILE_URI [options]\n       execute <check|explain|inspect|run> FILE [options]\n       execute resume FILE --checkpoint DIR [options]\n       execute live <serve|read|create|update|delete|patch|control|send|event|wait|schema> ...\n       execute model <classify|judge|score|generate|extract> [options]\n       execute model --help\n       execute --help\n\nOptions:\n  --json                  Structured static validation result (check only)\n  --input NAME=VALUE     Supply a declared program input (repeat for different names)\n  --manifest-dir DIR       Directory containing applet manifests\n  --cache-dir DIR          Cache directory (run/resume)\n  --checkpoint DIR         Durable checkpoint directory (run/resume)\n  --retry-uncertain        Retry an invocation with an uncertain prior outcome (resume only)\n  --headless               Run a UI once and print its JSON state\n  --trace                  Print execution details to stderr (run/resume/stream)\n  --max-output-bytes N     Maximum captured output size (run/resume)\n  --max-runs N             Maximum trigger runs or interactive UI jobs (not --headless)\n  --command-timeout-ms N  Maximum lifetime of each native pipeline (run/resume/stream)\n  --max-record-bytes N     Maximum listener/stream record size"
}

fn write_json(value: &serde_json::Value) -> Result<()> {
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string_pretty(value)?
    )?;
    Ok(())
}

fn print_help() -> Result<()> {
    writeln!(std::io::stdout().lock(), "{}", usage())?;
    Ok(())
}
