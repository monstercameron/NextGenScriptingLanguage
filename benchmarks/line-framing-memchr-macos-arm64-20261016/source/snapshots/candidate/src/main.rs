use anyhow::{anyhow, bail, Context, Result};
use flow::ast::Trigger;
use flow::capabilities::CapabilityPolicy;
use flow::manifest::Registry;
use flow::parser;
use flow::runtime::{self, CheckpointOptions, RunOptions, ServiceLimits};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::ToSocketAddrs;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Args {
    inputs: BTreeMap<String, String>,
    program_arguments: Vec<String>,
    json: bool,
    command: String,
    file: PathBuf,
    manifest_dir: Option<PathBuf>,
    manifest_catalog: Option<PathBuf>,
    capability_policy: Option<PathBuf>,
    data_flow_policy: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    checkpoint_dir: Option<PathBuf>,
    retry_uncertain: bool,
    trace: bool,
    headless: bool,
    max_output_bytes: usize,
    max_runs: Option<usize>,
    missed_ticks: Option<String>,
    max_catch_up: Option<usize>,
    calendar_dedup: Option<String>,
    command_timeout: Option<std::time::Duration>,
    total_timeout: Option<std::time::Duration>,
    max_record_bytes: usize,
    max_record_bytes_set: bool,
}

struct FiniteCheckSnapshot {
    source: String,
    registry: Registry,
}

struct SignalRegistration(signal_hook::SigId);

impl Drop for SignalRegistration {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.0);
    }
}

fn register_cancellation_signal(
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<SignalRegistration> {
    #[cfg(windows)]
    let signal = signal_hook::consts::SIGTERM;
    #[cfg(not(windows))]
    let signal = signal_hook::consts::SIGINT;
    signal_hook::flag::register(signal, cancelled)
        .map(SignalRegistration)
        .context("registering graph cancellation handler")
}

fn main() {
    if let Err(error) = execute() {
        runtime::write_diagnostic(format!("execute: {error:#}\n").as_bytes());
        let status = error
            .downcast_ref::<runtime::CommandFailed>()
            .map(|failure| {
                failure
                    .status
                    .code()
                    .unwrap_or_else(|| signal_status(&failure.status))
            })
            .filter(|status| {
                if cfg!(windows) {
                    *status != 0
                } else {
                    (1..=255).contains(status)
                }
            })
            .unwrap_or(1);
        std::process::exit(status);
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
    if arguments
        .first()
        .is_some_and(|argument| argument == "__remote_resolve")
    {
        return remote_resolve(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "live") {
        return flow::live::cli(&arguments[1..], default_manifest_dir(), workflow_path);
    }
    if arguments.first().is_some_and(|a| a == "applets") {
        return applets_cli(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "lsp") {
        if arguments.len() != 1 && !(arguments.len() == 2 && arguments[1] == "--stdio") {
            bail!("execute lsp accepts no arguments other than optional --stdio");
        }
        return flow::language_server::run_stdio();
    }
    if arguments.first().is_some_and(|a| a == "modules") {
        return modules_cli(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "service") {
        return service_cli(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "model") {
        #[cfg(unix)]
        {
            let helper = runtime::bundled_model_applet()?;
            let mut command = std::process::Command::new(helper);
            command.args(&arguments[1..]);
            return Err(command.exec()).context("starting the bundled model applet");
        }
        #[cfg(windows)]
        {
            std::process::exit(runtime::model_cli(&arguments[1..])?);
        }
    }
    let args = match parse_args(arguments)? {
        Some(args) => args,
        None => return Ok(()),
    };

    if args.json {
        let path = args.file.to_string_lossy().into_owned();
        let path_bytes = args.file.as_os_str().as_encoded_bytes().to_vec();
        let source = fs::read_to_string(&args.file).ok();
        let (static_diagnostics, finite_check_snapshot) = match source {
            Some(source)
                if !flow::ui::syntax::is_screen(&source)
                    && !flow::stream::is_stream(&source)
                    && !flow::graph::is_graph(&source) =>
            {
                match load_registry(args.manifest_dir.clone(), args.manifest_catalog.clone()) {
                    Ok(registry) => {
                        let diagnostics = flow::diagnostics::analyze_source(&source, &registry);
                        (diagnostics, Some(FiniteCheckSnapshot { source, registry }))
                    }
                    // Diagnostics are best effort. Keep the authoritative load at
                    // its existing point so catalog errors retain their ordering.
                    Err(_) => (Vec::new(), None),
                }
            }
            _ => (Vec::new(), None),
        };
        let data_flow_warning = args.capability_policy.is_some() && args.data_flow_policy.is_none();
        let result = execute_workflow(args, finite_check_snapshot);
        let mut diagnostics = static_diagnostics
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if data_flow_warning {
            diagnostics.push(serde_json::json!({
                "severity": "warning",
                "code": "data_flow_policy_not_configured",
                "message": "--capability-policy grants applet capabilities only; without --data-flow-policy, remote egress and persistence are not protected by data-flow grants"
            }));
        }
        if let Err(error) = &result {
            let has_static_error = static_diagnostics.iter().any(|diagnostic| {
                diagnostic.severity == flow::diagnostics::DiagnosticSeverity::Error
            });
            if !has_static_error {
                diagnostics.push(serde_json::json!({
                    "severity": "error", "code": "check_failed",
                    "message": format!("{error:#}"),
                    "causes": error.chain().map(ToString::to_string).collect::<Vec<_>>()
                }));
            }
        }
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
    execute_workflow(args, None)
}

fn remote_resolve(arguments: &[String]) -> Result<()> {
    if arguments.len() != 2 {
        bail!("internal remote resolver requires HOST PORT");
    }
    let host = arguments[0].as_str();
    if host.is_empty() || host.chars().any(char::is_control) || host.trim() != host {
        bail!("internal remote resolver requires a non-empty host without whitespace");
    }
    let port = arguments[1]
        .parse::<u16>()
        .context("internal remote resolver port must be 1..65535")?;
    if port == 0 {
        bail!("internal remote resolver port must be 1..65535");
    }
    let addresses = (host, port)
        .to_socket_addrs()
        .context("resolving remote host")?
        .take(16)
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        bail!("remote host resolved to no addresses");
    }
    let mut stdout = io::stdout().lock();
    for address in addresses {
        writeln!(stdout, "{address}")?;
    }
    Ok(())
}

fn execute_workflow(args: Args, finite_check_snapshot: Option<FiniteCheckSnapshot>) -> Result<()> {
    let capability_policy = args
        .capability_policy
        .as_deref()
        .map(load_capability_policy)
        .transpose()?;
    let data_flow_policy = args
        .data_flow_policy
        .as_deref()
        .map(load_data_flow_policy)
        .transpose()?;
    if capability_policy.is_some() && data_flow_policy.is_none() && !args.json {
        runtime::write_diagnostic(
            b"warning: --capability-policy grants applet capabilities only; without --data-flow-policy, remote egress and persistence are not protected by data-flow grants\n",
        );
    }
    let (source, registry_hint) = match finite_check_snapshot {
        Some(snapshot) => (snapshot.source, Some(snapshot.registry)),
        None => (
            fs::read_to_string(&args.file)
                .with_context(|| format!("could not read workflow {}", args.file.display()))?,
            None,
        ),
    };
    if flow::graph::is_graph(&source) {
        if args.command == "resume"
            || args.checkpoint_dir.is_some()
            || args.max_runs.is_some()
            || args.max_record_bytes_set
            || args.headless
            || args.retry_uncertain
            || args.missed_ticks.is_some()
            || args.max_catch_up.is_some()
            || args.calendar_dedup.is_some()
        {
            bail!(
                "graphs do not support resume, checkpoints, trigger limits, listener limits, or UI options"
            );
        }
        let registry = load_workflow_registry(
            args.manifest_dir,
            args.manifest_catalog,
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        )?;
        let graph = flow::graph::parse(&source, &registry).context("parsing graph")?;
        let supplied_inputs = flow::inputs::parse_cli_arguments(
            graph.input_declarations(),
            &args.inputs,
            &args.program_arguments,
        )?;
        let inputs = if args.command == "run" {
            flow::inputs::resolve(graph.input_declarations(), &supplied_inputs, false)?
        } else {
            supplied_inputs
        };
        let interface = flow::inputs::describe(graph.input_declarations(), &inputs)?;
        let graph = graph.with_input_values(&inputs, false)?;
        graph.validate_with_registry(&registry)?;
        if capability_policy.is_some() {
            let cwd = fs::canonicalize(env::current_dir()?)?;
            flow::capabilities::preflight_invocations(graph.invocations(), &registry, &cwd)?;
        }
        return match args.command.as_str() {
            "check" => Ok(()),
            "inspect" => write_json(&serde_json::json!({
                "protocol": "execute.graph/1",
                "validation": "static",
                "interface": interface,
                "graph": serde_json::to_value(&graph)?
            })),
            "explain" => writeln!(io::stdout().lock(), "{}", graph.to_dot()).map_err(Into::into),
            "run" => {
                let options = RunOptions {
                    inputs,
                    working_directory: env::current_dir()?,
                    cache_directory: args.cache_dir,
                    trace: args.trace,
                    max_output_bytes: args.max_output_bytes,
                    command_timeout: args.command_timeout,
                    total_timeout: args.total_timeout,
                };
                let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let _signal_registration =
                    register_cancellation_signal(std::sync::Arc::clone(&cancelled))?;
                let result = flow::graph::run(graph, &registry, &options, cancelled)?;
                let outputs = result
                    .outputs
                    .into_iter()
                    .map(|event| {
                        serde_json::json!({
                            "port": event.port,
                            "generation": event.generation,
                            "revision": event.revision,
                            "sequence": event.sequence,
                            "sensitivity": event.sensitivity,
                            "payload": event.payload
                        })
                    })
                    .collect::<Vec<_>>();
                write_json(&serde_json::json!({
                    "protocol": "execute.graph-run/1",
                    "generation": result.generation,
                    "revision": result.revision,
                    "dropped_messages": result.dropped_messages,
                    "launched_processes": result.launched_processes,
                    "outputs": outputs
                }))
            }
            _ => unreachable!("argument parser only accepts workflow commands"),
        };
    }
    if flow::ui::syntax::is_screen(&source) {
        if args.missed_ticks.is_some()
            || args.max_catch_up.is_some()
            || args.calendar_dedup.is_some()
        {
            bail!("schedule flags apply only to triggered run/resume programs");
        }
        if !args.inputs.is_empty() || !args.program_arguments.is_empty() {
            bail!("screen programs use component input ports and live widget state, not --input");
        }
        let screen = flow::ui::syntax::parse(&source).context("parsing UI")?;
        let registry = load_workflow_registry(
            args.manifest_dir,
            args.manifest_catalog,
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        )?;
        flow::ui::syntax::validate(&screen, &registry)?;
        if capability_policy.is_some() {
            let policy_program = flow::ast::Program {
                definitions: Default::default(),
                inputs: Vec::new(),
                steps: screen
                    .components
                    .values()
                    .flat_map(|component| &component.handlers)
                    .flat_map(|handler| &handler.actions)
                    .filter_map(|action| match action {
                        flow::ui::syntax::Action::Native(step) => Some(step.clone()),
                        _ => None,
                    })
                    .collect(),
                trigger: None,
            };
            flow::capabilities::preflight(&policy_program, &registry, &env::current_dir()?)?;
        }
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
            total_timeout: args.total_timeout,
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
                    bail!(
                        "--max-runs cannot be combined with --headless; headless UI runs initialization once"
                    );
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
        let inputs = flow::inputs::parse_cli_arguments(
            &stream.pipeline.inputs,
            &args.inputs,
            &args.program_arguments,
        )?;
        if args.command == "resume" || args.checkpoint_dir.is_some() || args.max_runs.is_some() {
            bail!("stream programs do not support resume, checkpoints, or --max-runs");
        }
        if args.missed_ticks.is_some()
            || args.max_catch_up.is_some()
            || args.calendar_dedup.is_some()
        {
            bail!("schedule flags apply only to triggered run/resume programs");
        }
        if args.headless {
            bail!("--headless applies only to UI programs");
        }
        let registry = load_workflow_registry(
            args.manifest_dir,
            args.manifest_catalog,
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        )?;
        if capability_policy.is_some() {
            flow::stream::validate(&stream, &registry)?;
            runtime::validate_with_arguments(&stream.pipeline, &registry, &inputs)?;
            flow::capabilities::preflight(&stream.pipeline, &registry, &env::current_dir()?)?;
        }

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
                    Some(limit) => writeln!(
                        std::io::stdout().lock(),
                        "persistent native pipeline; raw output to stdout; take the first {limit} final records; per-record limit {} bytes\n{pipeline}",
                        args.max_record_bytes
                    )?,
                    None => writeln!(
                        std::io::stdout().lock(),
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
                    total_timeout: args.total_timeout,
                };
                let mut emit =
                    |bytes: &[u8],
                     cancelled: &std::sync::atomic::AtomicBool,
                     deadline: Option<std::time::Instant>| {
                        runtime::write_with_deadline(1, bytes, cancelled, deadline)
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
    let inputs =
        flow::inputs::parse_cli_arguments(&program.inputs, &args.inputs, &args.program_arguments)?;
    let schedule_options = schedule_options(&args)?;
    if program.trigger.is_none() && schedule_options.is_some() {
        bail!(
            "--missed-ticks, --max-catch-up, and --calendar-dedup apply only to triggered programs"
        );
    }
    if args.calendar_dedup.is_some()
        && !matches!(program.trigger.as_ref(), Some(Trigger::Cron { .. }))
    {
        bail!("--calendar-dedup applies only to calendar-triggered programs");
    }
    if args.missed_ticks.is_some()
        && !matches!(
            program.trigger.as_ref(),
            Some(Trigger::Every { .. } | Trigger::Cron { .. })
        )
    {
        bail!("--missed-ticks applies only to interval- or calendar-triggered programs");
    }
    if args.max_runs.is_some() && program.trigger.is_none() {
        bail!("--max-runs applies only to triggered programs");
    }
    if args.max_record_bytes_set
        && !matches!(program.trigger.as_ref(), Some(Trigger::Listen { .. }))
    {
        bail!("--max-record-bytes applies only to listener programs");
    }
    let registry = match registry_hint {
        Some(registry) => install_workflow_policy_state(
            registry,
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        ),
        None => load_workflow_registry(
            args.manifest_dir,
            args.manifest_catalog,
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        )?,
    };
    if capability_policy.is_some() {
        runtime::validate_with_arguments(&program, &registry, &inputs)?;
        flow::capabilities::preflight(&program, &registry, &env::current_dir()?)?;
    }

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
                total_timeout: args.total_timeout,
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
                let schedule = schedule_options.unwrap_or_default();
                let limits = ServiceLimits {
                    max_runs: args.max_runs,
                    max_record_bytes: args.max_record_bytes,
                };
                let mut emit =
                    |bytes: &[u8], cancelled: &std::sync::atomic::AtomicBool, deadline| {
                        runtime::write_with_deadline(1, bytes, cancelled, deadline)
                    };
                if let Some(checkpoint) = checkpoint.as_ref() {
                    runtime::run_service_checkpointed_with_schedule_options(
                        &program, &registry, &options, &limits, checkpoint, &schedule, &mut emit,
                    )?;
                } else {
                    runtime::run_service_with_schedule_options(
                        &program, &registry, &options, &limits, &schedule, emit,
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
    let mut program_arguments = Vec::new();
    let mut manifest_dir = None;
    let mut manifest_catalog = None;
    let mut capability_policy = None;
    let mut data_flow_policy = None;
    let mut cache_dir = None;
    let mut checkpoint_dir = None;
    let mut retry_uncertain = false;
    let mut trace = false;
    let mut headless = false;
    let mut max_output_bytes = 16 * 1024 * 1024;
    let mut max_output_seen = false;
    let mut max_runs = None;
    let mut missed_ticks = None;
    let mut max_catch_up = None;
    let mut calendar_dedup = None;
    let mut command_timeout = None;
    let mut total_timeout = None;
    let mut max_record_bytes = 65_536;
    let mut max_record_bytes_seen = false;
    let mut index = first_argument;
    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--" => {
                index += 1;
                if file.is_none() {
                    let argument = arguments
                        .get(index)
                        .context("missing workflow file after --")?;
                    file = Some(workflow_path(argument)?);
                    index += 1;
                }
                program_arguments.extend_from_slice(&arguments[index..]);
                break;
            }
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
                if manifest_catalog.is_some() {
                    bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
                }
                manifest_dir = Some(PathBuf::from(take_option_value(
                    &arguments,
                    &mut index,
                    "--manifest-dir",
                    "a directory",
                )?));
            }
            "--manifest-catalog" => {
                if manifest_catalog.is_some() {
                    bail!("--manifest-catalog may be specified only once");
                }
                if manifest_dir.is_some() {
                    bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
                }
                manifest_catalog = Some(PathBuf::from(take_option_value(
                    &arguments,
                    &mut index,
                    "--manifest-catalog",
                    "a packed manifest catalog file",
                )?));
            }
            "--capability-policy"
                if matches!(
                    command.as_str(),
                    "check" | "explain" | "inspect" | "run" | "resume"
                ) =>
            {
                if capability_policy.is_some() {
                    bail!("--capability-policy may be specified only once");
                }
                capability_policy = Some(PathBuf::from(take_option_value(
                    &arguments,
                    &mut index,
                    "--capability-policy",
                    "a policy file",
                )?));
            }
            "--data-flow-policy"
                if matches!(
                    command.as_str(),
                    "check" | "explain" | "inspect" | "run" | "resume"
                ) =>
            {
                if data_flow_policy.is_some() {
                    bail!("--data-flow-policy may be specified only once");
                }
                data_flow_policy = Some(PathBuf::from(take_option_value(
                    &arguments,
                    &mut index,
                    "--data-flow-policy",
                    "a policy file",
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
            "--total-timeout-ms" if matches!(command.as_str(), "run" | "resume") => {
                if total_timeout.is_some() {
                    bail!("--total-timeout-ms may be specified only once");
                }
                let value = take_option_value(
                    &arguments,
                    &mut index,
                    "--total-timeout-ms",
                    "a positive integer",
                )?;
                let millis = value
                    .parse::<u64>()
                    .map_err(|_| anyhow!("--total-timeout-ms must be a positive integer"))?;
                if millis == 0 {
                    bail!("--total-timeout-ms must be greater than zero");
                }
                let duration = std::time::Duration::from_millis(millis);
                std::time::Instant::now()
                    .checked_add(duration)
                    .context("total timeout is too large")?;
                total_timeout = Some(duration);
            }
            "--max-runs" if matches!(command.as_str(), "run" | "resume") => {
                if max_runs.is_some() {
                    bail!("--max-runs may be specified only once");
                }
                max_runs = Some(parse_positive_usize(&arguments, &mut index, "--max-runs")?);
            }
            "--missed-ticks" if matches!(command.as_str(), "run" | "resume") => {
                if missed_ticks.is_some() {
                    bail!("--missed-ticks may be specified only once");
                }
                missed_ticks = Some(take_option_value(
                    &arguments,
                    &mut index,
                    "--missed-ticks",
                    "skip, coalesce, or catch-up",
                )?);
            }
            "--max-catch-up" if matches!(command.as_str(), "run" | "resume") => {
                if max_catch_up.is_some() {
                    bail!("--max-catch-up may be specified only once");
                }
                let value = parse_positive_usize(&arguments, &mut index, "--max-catch-up")?;
                if value > 64 {
                    bail!("--max-catch-up must be between 1 and 64");
                }
                max_catch_up = Some(value);
            }
            "--calendar-dedup" if matches!(command.as_str(), "run" | "resume") => {
                if calendar_dedup.is_some() {
                    bail!("--calendar-dedup may be specified only once");
                }
                calendar_dedup = Some(take_option_value(
                    &arguments,
                    &mut index,
                    "--calendar-dedup",
                    "instant or local",
                )?);
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
        program_arguments,
        json,
        command,
        file,
        manifest_dir,
        manifest_catalog,
        capability_policy,
        data_flow_policy,
        cache_dir,
        checkpoint_dir,
        retry_uncertain,
        trace,
        headless,
        max_output_bytes,
        max_runs,
        missed_ticks,
        max_catch_up,
        calendar_dedup,
        command_timeout,
        total_timeout,
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
        bail!(
            "file URI queries and fragments are not supported; encode filename characters as %3F and %23"
        );
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
    #[cfg(unix)]
    return Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)));
    #[cfg(windows)]
    {
        let path = String::from_utf8(decoded).context("Windows file URI must contain UTF-8")?;
        let path = path
            .strip_prefix('/')
            .filter(|p| p.as_bytes().get(1) == Some(&b':'))
            .unwrap_or(&path);
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            bail!("Windows file URI requires an absolute drive path");
        }
        Ok(path)
    }
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

fn schedule_options(args: &Args) -> Result<Option<flow::runtime::ScheduleOptions>> {
    if args.missed_ticks.is_none() && args.max_catch_up.is_none() && args.calendar_dedup.is_none() {
        return Ok(None);
    }
    let missed_ticks = match args.missed_ticks.as_deref().unwrap_or("skip") {
        "skip" => {
            if args.max_catch_up.is_some() {
                bail!("--max-catch-up is valid only with --missed-ticks catch-up");
            }
            flow::runtime::MissedTickPolicy::Skip
        }
        "coalesce" => {
            if args.max_catch_up.is_some() {
                bail!("--max-catch-up is valid only with --missed-ticks catch-up");
            }
            flow::runtime::MissedTickPolicy::Coalesce
        }
        "catch-up" => flow::runtime::MissedTickPolicy::CatchUp {
            max_runs: args
                .max_catch_up
                .context("--missed-ticks catch-up requires --max-catch-up N")?,
        },
        other => bail!("--missed-ticks must be skip, coalesce, or catch-up, got {other:?}"),
    };
    let calendar_dedup = match args.calendar_dedup.as_deref().unwrap_or("instant") {
        "instant" => flow::runtime::CalendarDedupPolicy::AbsoluteMinute,
        "local" => flow::runtime::CalendarDedupPolicy::LocalMinute,
        other => bail!("--calendar-dedup must be instant or local, got {other:?}"),
    };
    Ok(Some(flow::runtime::ScheduleOptions {
        missed_ticks,
        calendar_dedup,
    }))
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

fn applets_cli(arguments: &[String]) -> Result<()> {
    const HELP: &str = "Usage: execute applets list [--manifest-dir PATH | --manifest-catalog FILE]\n       execute applets inspect NAME_OR_PHRASE [--manifest-dir PATH | --manifest-catalog FILE]\n       execute applets discover\n       execute applets match --json REQUEST [--manifest-dir PATH | --manifest-catalog FILE]\n\nThe manifest selectors are mutually exclusive. --manifest-dir keeps its directory or single-manifest-file behavior; --manifest-catalog explicitly selects a packed catalog. No catalog is merged or selected automatically. Inspect emits validated contract metadata without executing applets. Discover reports installed commands using PATH metadata only. Match ranks declared typed ports without running applets. Versioned contracts expose typed I/O, outcomes, effects, backends, and lifecycle while retaining legacy fields for compatibility.\nUse -- before a name or phrase beginning with a hyphen.";
    if arguments == ["--help"] || arguments == ["-h"] {
        writeln!(std::io::stdout().lock(), "{HELP}")?;
        return Ok(());
    }
    let command = arguments.first().context(HELP)?;
    if !matches!(command.as_str(), "list" | "inspect" | "discover" | "match") {
        bail!("unknown applets command {command:?}\n{HELP}");
    }
    let mut manifest_dir = None;
    let mut manifest_catalog = None;
    let mut name = None;
    let mut request = None;
    let mut request_file = None;
    let mut literal = false;
    let mut index = 1;
    while index < arguments.len() {
        let argument = &arguments[index];
        if !literal && argument == "--" {
            literal = true;
        } else if !literal && argument == "--manifest-dir" {
            if manifest_dir.is_some() {
                bail!("--manifest-dir may be specified only once");
            }
            if manifest_catalog.is_some() {
                bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
            }
            manifest_dir = Some(PathBuf::from(take_option_value(
                arguments,
                &mut index,
                "--manifest-dir",
                "a manifest path",
            )?));
        } else if !literal && argument == "--manifest-catalog" {
            if manifest_catalog.is_some() {
                bail!("--manifest-catalog may be specified only once");
            }
            if manifest_dir.is_some() {
                bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
            }
            manifest_catalog = Some(PathBuf::from(take_option_value(
                arguments,
                &mut index,
                "--manifest-catalog",
                "a packed manifest catalog file",
            )?));
        } else if !literal && argument == "--json" && command == "match" {
            if request.is_some() || request_file.is_some() {
                bail!("match accepts one request source");
            }
            request = Some(take_option_value(
                arguments,
                &mut index,
                "--json",
                "a JSON PortMatchRequest",
            )?);
        } else if !literal && argument == "--json-file" && command == "match" {
            if request.is_some() || request_file.is_some() {
                bail!("match accepts one request source");
            }
            request_file = Some(PathBuf::from(take_option_value(
                arguments,
                &mut index,
                "--json-file",
                "a JSON request file",
            )?));
        } else if !literal && argument.starts_with('-') {
            bail!("unknown or unsupported applets option `{argument}`");
        } else if command == "inspect" && name.is_none() {
            name = Some(argument.as_str());
        } else {
            bail!("unexpected applets argument {argument:?}\n{HELP}");
        }
        index += 1;
    }
    if command == "inspect" && name.is_none() {
        bail!("applets inspect requires a name or phrase\n{HELP}");
    }
    if command == "discover" {
        if name.is_some() || request.is_some() || request_file.is_some() {
            bail!("applets discover takes no positional arguments or request JSON");
        }
        if manifest_dir.is_some() {
            bail!("--manifest-dir does not apply to applets discover");
        }
        if manifest_catalog.is_some() {
            bail!("--manifest-catalog does not apply to applets discover");
        }
        return write_json(&serde_json::json!({
            "protocol": "execute.applets-discovery/1",
            "validation": "path_metadata_only",
            "commands": flow::discovery::inventory_commands()?
        }));
    }
    let registry = load_registry(manifest_dir, manifest_catalog)?;
    if command == "match" {
        if name.is_some() {
            bail!("applets match takes no positional arguments");
        }
        let request: flow::discovery::PortMatchRequest = match (request, request_file) {
            (Some(text), None) => serde_json::from_str(&text)
                .context("--json requires a PortMatchRequest JSON object")?,
            (None, Some(path)) => serde_json::from_slice(
                &read_bounded(&path, 1024 * 1024)
                    .with_context(|| format!("reading match request {}", path.display()))?,
            )
            .context("--json-file requires a PortMatchRequest JSON object")?,
            (None, None) => bail!("applets match requires --json or --json-file\n{HELP}"),
            (Some(_), Some(_)) => unreachable!(),
        };
        return write_json(&serde_json::json!({
            "protocol": "execute.applet-matches/1",
            "validation": "declared_contracts",
            "matches": flow::discovery::rank_port_matches(&registry, &request)
        }));
    }
    if let Some(name) = name {
        write_json(&serde_json::json!({
            "protocol": "execute.applet/1", "validation": "manifest_static",
            "applet": registry.resolve(name)?.contract()
        }))
    } else {
        let mut manifests = registry.manifests().iter().collect::<Vec<_>>();
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        write_json(&serde_json::json!({
            "protocol": "execute.applets/1", "validation": "manifest_static",
            "applets": manifests.iter().map(|manifest| manifest.contract()).collect::<Vec<_>>()
        }))
    }
}

fn modules_cli(arguments: &[String]) -> Result<()> {
    const HELP: &str = "Usage: execute modules inspect MANIFEST [--root DIR] [--lock FILE]\n       execute modules verify MANIFEST --lock FILE [--root DIR]\n       execute modules lock MANIFEST --output FILE [--root DIR]\n\nValidate a local program-module package graph. No packages are fetched or installed. A lock command writes only to its explicit --output path.";
    if arguments == ["--help"] || arguments == ["-h"] {
        writeln!(io::stdout().lock(), "{HELP}")?;
        return Ok(());
    }
    let command = arguments.first().context(HELP)?.as_str();
    if !matches!(command, "inspect" | "verify" | "lock") {
        bail!("unknown modules command {command:?}\n{HELP}");
    }
    let mut manifest = None;
    let mut root = None;
    let mut lock = None;
    let mut output = None;
    let mut index = 1;
    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--root" => {
                if root.is_some() {
                    bail!("--root may be specified once");
                }
                root = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--root",
                    "a module root directory",
                )?));
            }
            "--lock" => {
                if lock.is_some() {
                    bail!("--lock may be specified once");
                }
                lock = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--lock",
                    "a lock file",
                )?));
            }
            "--output" if command == "lock" => {
                if output.is_some() {
                    bail!("--output may be specified once");
                }
                output = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--output",
                    "a lock output file",
                )?));
            }
            value if value.starts_with('-') => bail!("unsupported modules option {value}"),
            _ => {
                if manifest.replace(PathBuf::from(argument)).is_some() {
                    bail!("modules command accepts one package manifest");
                }
            }
        }
        index += 1;
    }
    let manifest = manifest.context("modules command requires a package manifest")?;
    let root = root
        .or_else(|| manifest.parent().map(Path::to_path_buf))
        .context("could not derive module root from manifest path")?;
    match command {
        "inspect" => {
            if output.is_some() {
                bail!("--output applies only to modules lock");
            }
            let package =
                flow::modules::load_program_module_package(&manifest, &root, lock.as_deref())?;
            let mut exports = Vec::with_capacity(package.manifest.exports.len());
            for (name, specification) in &package.manifest.exports {
                let loaded = package.export(name)?;
                exports.push(serde_json::json!({
                    "name": name,
                    "entry": specification.entry,
                    "interface_version": specification.interface_version,
                    "inputs": specification.inputs,
                    "output": specification.output,
                    "outcomes": specification.outcomes,
                    "result_envelope": specification.result_envelope,
                    "lifecycle": specification.lifecycle,
                    "capabilities": specification.capabilities,
                    "effects": specification.effects,
                    "source_sha256": loaded.source_sha256
                }));
            }
            write_json(&serde_json::json!({
                "protocol": "execute.modules/1",
                "validation": if lock.is_some() { "package_and_lock" } else { "package" },
                "name": package.manifest.name,
                "version": package.manifest.version,
                "interface_version": package.manifest.interface_version,
                "manifest": package.manifest_path,
                "source": package.source_path,
                "manifest_sha256": package.manifest_sha256,
                "source_sha256": package.source_sha256,
                "dependencies": package.dependencies.keys().collect::<Vec<_>>(),
                "exports": exports
            }))
        }
        "verify" => {
            if output.is_some() {
                bail!("--output applies only to modules lock");
            }
            let lock = lock.context("modules verify requires --lock FILE")?;
            let package =
                flow::modules::load_program_module_package(&manifest, &root, Some(&lock))?;
            for name in package.manifest.exports.keys() {
                package.export(name)?;
            }
            let snapshot = package.lock_snapshot(&root)?;
            write_json(&serde_json::json!({
                "protocol": "execute.modules/1",
                "ok": true,
                "validation": "package_and_lock",
                "name": package.manifest.name,
                "version": package.manifest.version,
                "packages": snapshot.packages.len(),
                "lock": lock
            }))
        }
        "lock" => {
            if lock.is_some() {
                bail!("--lock applies only to modules inspect or verify");
            }
            let output = output.context("modules lock requires --output FILE")?;
            let package = flow::modules::load_program_module_package(&manifest, &root, None)?;
            for name in package.manifest.exports.keys() {
                package.export(name)?;
            }
            let snapshot = package.lock_snapshot(&root)?;
            let bytes = serde_json::to_vec_pretty(&snapshot)?;
            fs::write(&output, bytes)
                .with_context(|| format!("writing module lock {}", output.display()))?;
            write_json(&serde_json::json!({
                "protocol": "execute.modules/1",
                "ok": true,
                "created": output,
                "packages": snapshot.packages.len()
            }))
        }
        _ => unreachable!(),
    }
}

fn service_cli(arguments: &[String]) -> Result<()> {
    const HELP: &str = "Usage: execute service generate <systemd|launchd> FILE [--name ID] [--manifest-dir DIR | --manifest-catalog FILE] [--working-dir DIR] [--capability-policy FILE] [--data-flow-policy FILE] [--input NAME=VALUE ...] [-- ARG...]\n\nThe manifest selectors are mutually exclusive. --manifest-dir keeps its directory or single-manifest-file behavior; --manifest-catalog explicitly selects a packed catalog. No catalog is merged or selected automatically. Print a validated per-user service definition. This command never writes, installs, enables, or starts a service.";
    if arguments == ["--help"] || arguments == ["-h"] {
        writeln!(io::stdout().lock(), "{HELP}")?;
        return Ok(());
    }
    if arguments.first().map(String::as_str) != Some("generate") {
        bail!("service supports only `generate`\n{HELP}");
    }
    let mut positional = Vec::new();
    let mut label = None;
    let mut manifest_dir = None;
    let mut manifest_catalog = None;
    let mut working_directory = None;
    let mut capability_policy = None;
    let mut data_flow_policy = None;
    let mut supplied_inputs = BTreeMap::new();
    let mut program_arguments = Vec::new();
    let mut index = 1;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--" {
            program_arguments.extend_from_slice(&arguments[index + 1..]);
            break;
        }
        match argument.as_str() {
            "--name" => {
                if label.is_some() {
                    bail!("--name may be specified once");
                }
                label = Some(take_option_value(
                    arguments,
                    &mut index,
                    "--name",
                    "an identifier",
                )?);
            }
            "--manifest-dir" => {
                if manifest_dir.is_some() {
                    bail!("--manifest-dir may be specified once");
                }
                if manifest_catalog.is_some() {
                    bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
                }
                manifest_dir = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--manifest-dir",
                    "a manifest directory",
                )?));
            }
            "--manifest-catalog" => {
                if manifest_catalog.is_some() {
                    bail!("--manifest-catalog may be specified once");
                }
                if manifest_dir.is_some() {
                    bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
                }
                manifest_catalog = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--manifest-catalog",
                    "a packed manifest catalog file",
                )?));
            }
            "--working-dir" => {
                if working_directory.is_some() {
                    bail!("--working-dir may be specified once");
                }
                working_directory = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--working-dir",
                    "a directory",
                )?));
            }
            "--capability-policy" => {
                if capability_policy.is_some() {
                    bail!("--capability-policy may be specified once");
                }
                capability_policy = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--capability-policy",
                    "a policy file",
                )?));
            }
            "--data-flow-policy" => {
                if data_flow_policy.is_some() {
                    bail!("--data-flow-policy may be specified once");
                }
                data_flow_policy = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--data-flow-policy",
                    "a policy file",
                )?));
            }
            "--input" => {
                let pair = take_option_value(arguments, &mut index, "--input", "NAME=VALUE")?;
                let (name, value) = pair
                    .split_once('=')
                    .context("--input requires NAME=VALUE")?;
                if supplied_inputs
                    .insert(name.to_owned(), value.to_owned())
                    .is_some()
                {
                    bail!("duplicate program input {name:?}");
                }
            }
            value if value.starts_with('-') => bail!("unsupported service option {value}"),
            _ => positional.push(workflow_path(argument)?),
        }
        index += 1;
    }
    if positional.len() != 2 {
        bail!("service generate requires a format and one workflow file\n{HELP}");
    }
    let capability_policy_path = capability_policy.clone();
    let data_flow_policy_path = data_flow_policy.clone();
    let format = flow::service_deployment::ServiceFormat::parse(
        positional[0]
            .to_str()
            .context("service format must be UTF-8")?,
    )?;
    let capability_policy = capability_policy
        .as_deref()
        .map(load_capability_policy)
        .transpose()?;
    let data_flow_policy = data_flow_policy
        .as_deref()
        .map(load_data_flow_policy)
        .transpose()?;
    if capability_policy.is_some() && data_flow_policy.is_none() {
        runtime::write_diagnostic(
            b"warning: --capability-policy grants applet capabilities only; without --data-flow-policy, remote egress and persistence are not protected by data-flow grants\n",
        );
    }
    let executable = fs::canonicalize(env::current_exe()?)?;
    let working_directory = fs::canonicalize(working_directory.unwrap_or(env::current_dir()?))?;
    let registry = load_workflow_registry(
        manifest_dir.clone(),
        manifest_catalog.clone(),
        capability_policy.as_ref(),
        data_flow_policy.as_ref(),
    )?;
    let source = fs::read_to_string(&positional[1])
        .with_context(|| format!("reading workflow {}", positional[1].display()))?;
    let program = parser::parse(&source)
        .with_context(|| format!("parsing workflow {}", positional[1].display()))?;
    let resolved_inputs =
        flow::inputs::parse_cli_arguments(&program.inputs, &supplied_inputs, &program_arguments)?;
    runtime::validate_with_arguments(&program, &registry, &resolved_inputs)?;
    flow::capabilities::preflight(&program, &registry, &working_directory)?;
    let request = flow::service_deployment::WorkflowServiceRequest {
        format,
        workflow_path: &positional[1],
        executable: &executable,
        working_directory: &working_directory,
        manifest_directory: manifest_dir.as_deref(),
        capability_policy: capability_policy_path.as_deref(),
        data_flow_policy: data_flow_policy_path.as_deref(),
        label: label.as_deref(),
        inputs: &supplied_inputs,
        program_arguments: &program_arguments,
    };
    let rendered = match manifest_catalog.as_deref() {
        Some(path) => {
            flow::service_deployment::render_for_workflow_with_manifest_catalog(&request, path)?
        }
        None => flow::service_deployment::render_for_workflow(&request)?,
    };
    print!("{rendered}");
    Ok(())
}

fn load_registry(
    explicit_path: Option<PathBuf>,
    explicit_catalog: Option<PathBuf>,
) -> Result<Registry> {
    if explicit_path.is_some() && explicit_catalog.is_some() {
        bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
    }
    match (explicit_path, explicit_catalog) {
        (Some(path), None) => Registry::load(&path)
            .with_context(|| format!("could not load manifests from {}", path.display())),
        (None, Some(path)) => Registry::load_catalog(&path)
            .with_context(|| format!("could not load packed manifest catalog {}", path.display())),
        (None, None) => Registry::load_optional(&default_manifest_dir()),
        (Some(_), Some(_)) => unreachable!("selector conflict checked above"),
    }
}

const MAX_CAPABILITY_POLICY_BYTES: usize = 1024 * 1024;

fn load_capability_policy(path: &Path) -> Result<CapabilityPolicy> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("opening capability policy {}", path.display()))?
        .take((MAX_CAPABILITY_POLICY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading capability policy {}", path.display()))?;
    if bytes.len() > MAX_CAPABILITY_POLICY_BYTES {
        bail!("capability policy exceeds {MAX_CAPABILITY_POLICY_BYTES} bytes");
    }
    let source = String::from_utf8(bytes).context("capability policy must be UTF-8 JSON")?;
    CapabilityPolicy::from_json(&source)
        .with_context(|| format!("loading capability policy {}", path.display()))
}

fn load_data_flow_policy(path: &Path) -> Result<flow::security::DataFlowPolicy> {
    flow::security::DataFlowPolicy::load(path)
        .map_err(|error| anyhow!("loading data-flow policy {}: {error}", path.display()))
}

fn load_workflow_registry(
    explicit_path: Option<PathBuf>,
    explicit_catalog: Option<PathBuf>,
    capability_policy: Option<&CapabilityPolicy>,
    data_flow_policy: Option<&flow::security::DataFlowPolicy>,
) -> Result<Registry> {
    let registry = load_registry(explicit_path, explicit_catalog)?;
    Ok(install_workflow_policy_state(
        registry,
        capability_policy,
        data_flow_policy,
    ))
}

fn install_workflow_policy_state(
    registry: Registry,
    capability_policy: Option<&CapabilityPolicy>,
    data_flow_policy: Option<&flow::security::DataFlowPolicy>,
) -> Registry {
    if let Some(policy) = capability_policy {
        registry.set_host_state(Some(policy.clone()));
    }
    if let Some(policy) = data_flow_policy {
        registry.set_host_state(policy.clone());
    }
    registry
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
            let system = parent
                .join("..")
                .join("share")
                .join("execute")
                .join("manifests");
            if system.is_dir() {
                return system;
            }
        }
    }

    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn usage() -> &'static str {
    "Usage: execute FILE_OR_FILE_URI [options] [-- ARG...]\n       execute <check|explain|inspect|run> FILE [options]\n       execute resume FILE --checkpoint DIR [options]\n       execute applets <list|inspect|discover|match> [options]\n       execute modules <inspect|verify|lock> MANIFEST [options]\n       execute service generate <systemd|launchd> FILE [options]\n       execute lsp [--stdio]\n       execute live <serve|read|create|update|delete|patch|control|send|event|wait|schema> ...\n       execute model <classify|judge|score|generate|extract> [options]\n       execute model --help\n       execute --help\n\nOptions:\n  --json                  Structured static validation result (check only)\n  --input NAME=VALUE     Supply a declared program input (repeat for different names)\n  -- ARG...               Literal program arguments for declared positional inputs\n  --manifest-dir DIR       Directory or one legacy JSON file containing applet manifests\n  --manifest-catalog FILE Explicit packed catalog; mutually exclusive with --manifest-dir\n  --capability-policy FILE Enforce a strict, versioned applet policy (check/explain/inspect/run/resume/live serve)\n  --data-flow-policy FILE Enforce origin and persistence data-flow grants\n  --cache-dir DIR          Cache directory (run/resume)\n  --checkpoint DIR         Durable checkpoint directory (run/resume)\n  --retry-uncertain        Retry an invocation with an uncertain prior outcome (resume only)\n  --headless               Run a UI once and print its JSON state\n  --trace                  Print execution details to stderr (run/resume/stream)\n  --max-output-bytes N     Maximum captured output size (run/resume)\n  --max-runs N             Maximum trigger runs or interactive UI jobs (not --headless)\n  --missed-ticks POLICY   Trigger backlog policy: skip, coalesce, or catch-up (run/resume)\n  --max-catch-up N         Bound catch-up runs to 1..64 (requires catch-up)\n  --calendar-dedup MODE   Calendar fold policy: instant or local (run/resume)\n  --command-timeout-ms N  Maximum lifetime of each native pipeline (run/resume/stream)\n  --max-record-bytes N     Maximum listener/stream record size"
}

fn write_json(value: &serde_json::Value) -> Result<()> {
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string_pretty(value)?
    )?;
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let read_limit = limit
        .checked_add(1)
        .context("file byte limit is too large")?;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .take(u64::try_from(read_limit)?)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() > limit {
        bail!("file exceeds {limit} bytes");
    }
    Ok(bytes)
}

fn print_help() -> Result<()> {
    writeln!(
        std::io::stdout().lock(),
        "{}\n  --total-timeout-ms N    Maximum workflow lifetime (run/resume)\nData-flow policies can declare applet-scoped secrets.environment names for output taint and redaction.\nSecurity scope: --capability-policy grants applet capabilities; remote egress and persistence need --data-flow-policy.",
        usage()
    )?;
    Ok(())
}

fn signal_status(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        status.signal().map_or(1, |signal| 128 + signal)
    }
    #[cfg(windows)]
    {
        let _ = status;
        1
    }
}
