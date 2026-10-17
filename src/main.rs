use anyhow::{anyhow, bail, Context, Result};
use flow::ast::Trigger;
use flow::capabilities::CapabilityPolicy;
use flow::manifest::Registry;
use flow::runtime::{self, CheckpointOptions, RunOptions, ServiceLimits};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::ToSocketAddrs;
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};

mod cli;
use cli::{parse_args, take_option_value, workflow_path, Args};
mod github_annotations;
mod record_cli;
mod repair_cli;
mod repl_cli;
mod run_parts;
mod test_runner;

struct FiniteCheckSnapshot {
    source: String,
    registry: Registry,
}

/// A closed stdout reader surfaces as a broken pipe: EPIPE on Unix, and on
/// Windows ERROR_BROKEN_PIPE (109) or ERROR_NO_DATA (232) from pipe writes.
/// Such an error ends the process quietly, as SIGPIPE would.
fn is_closed_stdout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|io_error| {
            io_error.kind() == io::ErrorKind::BrokenPipe
                || (cfg!(windows) && matches!(io_error.raw_os_error(), Some(109 | 232)))
        })
    })
}

/// Exit status when the stdout reader has gone: the SIGPIPE convention (128 + 13).
const STDOUT_CLOSED_EXIT: i32 = 141;

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
    // An interrupt is reported as SIGINT on every platform (Ctrl-C: 130).
    #[cfg(windows)]
    let reported = signal_hook::consts::SIGINT;
    #[cfg(not(windows))]
    let reported = signal;
    // SAFETY: the action only performs atomic stores, which are async-signal-safe.
    unsafe {
        signal_hook::low_level::register(signal, move || {
            runtime::record_interrupt(reported);
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    }
    .map(SignalRegistration)
    .context("registering graph cancellation handler")
}

fn main() {
    if let Err(error) = execute() {
        std::process::exit(report_error(&error));
    }
}

/// Print a failed run's diagnostic and return its exit status. The normal entry point and
/// a forwarded run in a daemon child both use it.
fn report_error(error: &anyhow::Error) -> i32 {
    if is_closed_stdout(error) {
        // The reader went away; stop quietly without a diagnostic.
        return STDOUT_CLOSED_EXIT;
    }
    // The run has already cleaned up its children before returning this error.
    // An interrupt keeps its status; otherwise the failing command decides.
    let interrupted = runtime::interrupt_exit_code();
    let status = interrupted.unwrap_or_else(|| {
        error
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
            .unwrap_or(1)
    });
    if flow::agent::errors::enabled() {
        flow::agent::errors::write_stderr(error, interrupted.is_some(), status);
    } else {
        runtime::write_diagnostic(
            format!(
                "execute: {}\n",
                flow::terminal_text::escape_controls(&format!("{error:#}"))
            )
            .as_bytes(),
        );
    }
    status
}

/// `run --daemon`, or `run` with `EXECUTE_DAEMON=1`: the run goes to a warm daemon when one
/// answers. Returns the status when the daemon ran it; `None` means run here.
fn daemon_forward(arguments: &[String]) -> Option<i32> {
    if arguments.first().map(String::as_str) != Some("run") {
        return None;
    }
    let separator = arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(arguments.len());
    let flag = arguments[..separator]
        .iter()
        .position(|argument| argument == "--daemon");
    let selected = env::var("EXECUTE_DAEMON").is_ok_and(|value| value == "1");
    if flag.is_none() && !selected {
        return None;
    }
    let forwarded: Vec<String> = arguments
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != flag)
        .map(|(_, argument)| argument.clone())
        .collect();
    match flow::agent::daemon::forward_run(&forwarded) {
        flow::agent::daemon::Forwarding::Ran(status) => Some(status),
        flow::agent::daemon::Forwarding::Cold { note } => {
            if let Some(note) = note {
                eprintln!("{note}");
            }
            None
        }
    }
}

/// The entry point of a daemon child: the run as the normal path does it. A program that
/// may take the terminal is declined before it runs, so the client runs it instead.
/// Run a forwarded request. A plain `run` with no manifest or policy selectors uses the
/// daemon's preloaded registry through the finite-check snapshot seam. Every other request
/// takes the normal path, which loads its own registry.
fn execute_forwarded(arguments: Vec<String>, registry: &Registry) -> Result<()> {
    let Some(args) = parse_args(arguments.clone())? else {
        return execute_arguments(arguments);
    };
    let plain_run = args.command == "run"
        && args.manifest_dir.is_none()
        && args.manifest_catalog.is_none()
        && args.capability_policy.is_none()
        && args.data_flow_policy.is_none();
    if !plain_run {
        return execute_arguments(arguments);
    }
    let source = fs::read_to_string(&args.file)
        .with_context(|| format!("could not read workflow {}", args.file.display()))?;
    execute_workflow(
        args,
        Some(FiniteCheckSnapshot {
            source,
            registry: registry.clone(),
        }),
    )
}

/// The registry a daemon serves from: the default manifests, loaded and fully parsed
/// before the first request, so each forked run inherits the parsed applets.
fn preload_daemon_registry() -> Result<Registry> {
    let registry = load_registry(None, None)?;
    registry.manifests()?;
    Ok(registry)
}

fn forwarded_run(arguments: &[String], context: &mut flow::agent::daemon::RunContext) -> i32 {
    if program_takes_terminal(arguments) {
        context.decline("the program takes the terminal");
        return 0;
    }
    match execute_forwarded(arguments.to_vec(), context.registry()) {
        Ok(()) => 0,
        Err(error) => report_error(&error),
    }
}

/// Whether a run may take the terminal: `interactively`, `hand over`, or a prompt input
/// (`from prompt`). A forked daemon child is not in the terminal's foreground process
/// group, so a terminal read there would get SIGTTIN. The check is textual, with spacing
/// normalized, and conservative: a program whose source cannot be read is declined, and so
/// is one that mentions a form even inside a string.
fn program_takes_terminal(arguments: &[String]) -> bool {
    let Ok(Some(args)) = parse_args(arguments.to_vec()) else {
        return true;
    };
    let Ok(source) = fs::read_to_string(&args.file) else {
        return true;
    };
    let normalized = source
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    ["interactively", "hand over", "from prompt"]
        .iter()
        .any(|form| normalized.contains(form))
}

fn execute() -> Result<()> {
    let arguments = flow::agent::errors::strip_flag(env::args_os().skip(1))
        .into_iter()
        .enumerate()
        .map(|(index, argument)| {
            argument.into_string().map_err(|_| {
                anyhow!("argument {} is not valid UTF-8; use a percent-encoded file:// URI for non-UTF-8 workflow paths", index + 1)
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(status) = daemon_forward(&arguments) {
        std::process::exit(status);
    }
    execute_arguments(arguments)
}

fn execute_arguments(arguments: Vec<String>) -> Result<()> {
    if let Some(worker_arguments) = init_worker_arguments(&arguments)? {
        #[cfg(unix)]
        {
            let code = runtime::supervise_init(&worker_arguments)?;
            std::process::exit(code);
        }
        #[cfg(not(unix))]
        {
            let _ = worker_arguments;
            bail!("--init is available only on Unix");
        }
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "__remote_resolve")
    {
        return remote_resolve(&arguments[1..]);
    }
    #[cfg(feature = "live")]
    if arguments.first().is_some_and(|a| a == "live") {
        return flow::live::cli(&arguments[1..], default_manifest_dir(), workflow_path);
    }
    #[cfg(not(feature = "live"))]
    if arguments
        .first()
        .is_some_and(|a| matches!(a.as_str(), "live" | "attach"))
    {
        bail!(
            "`{}` needs the live control plane, and this build has none; use the full build",
            arguments[0]
        );
    }
    if arguments.first().is_some_and(|a| a == "applets") {
        return applets_cli(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "test") {
        return test_runner::cli(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "run-parts") {
        return run_parts::cli(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "show-run") {
        return record_cli::show_run(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "replay") {
        return record_cli::replay(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "repl") {
        return repl_cli::repl(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "repair") {
        return repair_cli::repair(&arguments[1..]);
    }
    #[cfg(feature = "lsp")]
    if arguments.first().is_some_and(|a| a == "lsp") {
        if arguments.len() != 1 && !(arguments.len() == 2 && arguments[1] == "--stdio") {
            bail!("execute lsp accepts no arguments other than optional --stdio");
        }
        return flow::language_server::run_stdio();
    }
    #[cfg(not(feature = "lsp"))]
    if arguments.first().is_some_and(|a| a == "lsp") {
        bail!("`lsp` needs the language server, and this build has none; use the full build");
    }
    if arguments.first().is_some_and(|a| a == "modules") {
        return modules_cli(&arguments[1..]);
    }
    if arguments.first().is_some_and(|a| a == "compile") {
        return compile_cli(&arguments[1..]);
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
    if arguments.first().is_some_and(|a| a == "checkpoint") {
        std::process::exit(flow::agent::checkpoint::cli(&arguments[1..])?);
    }
    if arguments.first().is_some_and(|a| a == "help") {
        std::process::exit(flow::agent::help::cli(&arguments[1..])?);
    }
    #[cfg(feature = "live")]
    if arguments.first().is_some_and(|a| a == "attach") {
        std::process::exit(flow::agent::attach::cli(&arguments[1..])?);
    }
    #[cfg(feature = "dap")]
    if arguments.first().is_some_and(|a| a == "dap") {
        std::process::exit(flow::agent::dap::cli(&arguments[1..])?);
    }
    if arguments.first().is_some_and(|a| a == "daemon") {
        std::process::exit(flow::agent::daemon::cli(
            &arguments[1..],
            forwarded_run,
            preload_daemon_registry,
        )?);
    }
    let args = match parse_args(arguments)? {
        Some(args) => args,
        None => return Ok(()),
    };

    if args.command == "check" {
        let json = args.json;
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
                        // The policy must be installed here too: static analysis reads
                        // the files that the program calls, and those reads are authorized.
                        let policies = args
                            .capability_policy
                            .as_deref()
                            .map(load_capability_policy)
                            .transpose()
                            .and_then(|capability| {
                                args.data_flow_policy
                                    .as_deref()
                                    .map(load_data_flow_policy)
                                    .transpose()
                                    .map(|data_flow| (capability, data_flow))
                            });
                        match policies {
                            Ok((capability, data_flow)) => {
                                let registry = install_workflow_policy_state(
                                    registry,
                                    capability.as_ref(),
                                    data_flow.as_ref(),
                                );
                                let diagnostics = flow::diagnostics::analyze_source_at(
                                    &source,
                                    Some(&args.file),
                                    &registry,
                                );
                                (diagnostics, Some(FiniteCheckSnapshot { source, registry }))
                            }
                            Err(_) => (Vec::new(), None),
                        }
                    }
                    // Diagnostics are best effort. Keep the authoritative load at
                    // its existing point so catalog errors retain their ordering.
                    Err(_) => (Vec::new(), None),
                }
            }
            _ => (Vec::new(), None),
        };
        let data_flow_warning = args.capability_policy.is_some() && args.data_flow_policy.is_none();
        let github = args.github;
        let result = execute_workflow(args, finite_check_snapshot);
        if !json {
            // Plain check and --json share this analysis: the same static errors fail the
            // check, after the authoritative load's own error.
            result?;
            let errors: Vec<String> = static_diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.severity == flow::diagnostics::DiagnosticSeverity::Error
                })
                .map(|diagnostic| format!("line {}: {}", diagnostic.span.line, diagnostic.message))
                .collect();
            if !errors.is_empty() {
                bail!(
                    "{}",
                    errors.join(
                        "
"
                    )
                );
            }
            return Ok(());
        }
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
        if github {
            github_annotations::write(&path, &diagnostics)?;
        } else {
            write_json(&serde_json::json!({
                "protocol": "execute.check/1", "validation": "static",
                "ok": result.is_ok(), "path": path, "path_bytes": path_bytes,
                "diagnostics": diagnostics
            }))?;
        }
        if result.is_err() {
            // Validation has no spawned workers/effects. The JSON response is
            // complete; preserve the ordinary failing-check exit status.
            std::process::exit(1);
        }
        return Ok(());
    }
    execute_workflow(args, None)
}

/// `--init` before `--` runs the workflow under the PID 1 supervisor. Returns
/// the worker's arguments, with the flag removed, or `None` when it is absent.
fn init_worker_arguments(arguments: &[String]) -> Result<Option<Vec<String>>> {
    let literal_start = arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(arguments.len());
    if !arguments[..literal_start]
        .iter()
        .any(|argument| argument == "--init")
    {
        return Ok(None);
    }
    if arguments.first().is_some_and(|first| {
        matches!(
            first.as_str(),
            "check"
                | "explain"
                | "inspect"
                | "resume"
                | "applets"
                | "modules"
                | "compile"
                | "service"
                | "lsp"
                | "live"
                | "model"
                | "__remote_resolve"
        )
    }) {
        bail!("--init applies only to `run`");
    }
    Ok(Some(
        arguments
            .iter()
            .enumerate()
            .filter(|(index, argument)| *index >= literal_start || argument.as_str() != "--init")
            .map(|(_, argument)| argument.clone())
            .collect(),
    ))
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
    let mut stdout = io::stdout();
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
    // Held until this function returns, so a second instance is refused for the
    // whole run. Graph, screen, and stream runs are covered too.
    let _single_instance = match args.single_instance.as_deref() {
        Some(path) => Some(flow::agent::instance::acquire(path)?),
        None => None,
    };
    if flow::graph::is_graph(&source) {
        reject_finite_only_options(&args, "graph")?;
        // `run` and `resume` accept `--checkpoint`; the trigger and UI options are not
        // supported for graphs.
        if args.max_runs.is_some()
            || args.max_record_bytes_set
            || args.headless
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
            "explain" => writeln!(io::stdout(), "{}", graph.to_dot()).map_err(Into::into),
            "run" | "resume" => {
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
                let result = match args.checkpoint_dir.as_deref() {
                    Some(directory) => {
                        let cwd = fs::canonicalize(&options.working_directory)?;
                        let identity = flow::graph::checkpoint_identity(&graph, &cwd)?;
                        let resume = args.command == "resume";
                        let store = flow::graph::open_checkpoint_directory(
                            directory, resume, &identity, &registry,
                        )?;
                        flow::graph::run_checkpointed(
                            graph,
                            &registry,
                            &options,
                            cancelled,
                            store,
                            args.retry_uncertain,
                        )?
                    }
                    None => flow::graph::run(graph, &registry, &options, cancelled)?,
                };
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
        reject_finite_only_options(&args, "screen")?;
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
                    io::stdout(),
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
                                    io::stdout(),
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
                    io::stdout(),
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
        reject_finite_only_options(&args, "stream")?;
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
                        io::stdout(),
                        "persistent native pipeline; raw output to stdout; take the first {limit} final records; per-record limit {} bytes\n{pipeline}",
                        args.max_record_bytes
                    )?,
                    None => writeln!(
                        io::stdout(),
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
    let registry = match registry_hint {
        Some(registry) => install_workflow_policy_state(
            registry,
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        ),
        None => match load_workflow_registry(
            args.manifest_dir.clone(),
            args.manifest_catalog.clone(),
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        ) {
            Ok(registry) => registry,
            Err(error) => {
                // The program's own syntax error is the more useful report, so parse
                // first. A file call that needs the policy is not such an error here.
                if let Err(parse_error) =
                    flow::program_files::parse_source(&args.file, &source, None)
                {
                    if !format!("{parse_error:#}").contains("needs the program's capability policy")
                    {
                        return Err(parse_error).with_context(|| {
                            format!("could not parse workflow {}", args.file.display())
                        });
                    }
                }
                return Err(error);
            }
        },
    };
    // The registry carries the capability policy, which authorizes the reads of any
    // program file the program calls, so it is loaded before the program is parsed.
    let cwd = env::current_dir()?;
    let program = flow::program_files::parse_source(
        &args.file,
        &source,
        Some(flow::program_files::Access {
            registry: &registry,
            cwd: &cwd,
        }),
    )
    .with_context(|| format!("could not parse workflow {}", args.file.display()))?;
    // A script's own `--help` (from a shebang line) shows its accept declarations.
    // Literal arguments keep `--help` as a value for positional programs; a program with
    // flags or options always answers `--help` with its generated help.
    if args.command == "run"
        && (!args.program_arguments_literal || flow::inputs::declares_flags(&program.inputs))
        && !program.inputs.is_empty()
        && flow::inputs::is_help_request(&program.inputs, &args.program_arguments)
    {
        write!(
            io::stdout(),
            "{}",
            flow::agent::help::render_program_help(&program, &args.file.to_string_lossy())
        )?;
        return Ok(());
    }
    // A journal keeps the bindings and the pipeline data of every completed step, and a
    // secret answer, or any value computed from it, would be written to disk. Such a run
    // is refused rather than checkpointed.
    if args.checkpoint_dir.is_some()
        && program.inputs.iter().any(|input| {
            matches!(
                input.source,
                Some(flow::ast::InputSource::Prompt { secret: true, .. })
            )
        })
    {
        bail!(
            "--checkpoint cannot be used with a secret prompt input: a journal would keep the answer or data computed from it"
        );
    }
    // Supplied answers win over the answers file, which wins over a terminal prompt.
    let mut supplied = args.inputs.clone();
    if let Some(path) = &args.answers {
        for (name, text) in flow::prompt::answers_from_file(path)? {
            supplied.entry(name).or_insert(text);
        }
    }
    let mut inputs =
        flow::inputs::parse_cli_arguments(&program.inputs, &supplied, &args.program_arguments)?;
    if args.command == "run" {
        flow::prompt::fill_missing(
            &program.inputs,
            &mut inputs,
            &mut flow::prompt::terminal_ask,
        )?;
    }
    if args.control {
        #[cfg(feature = "live")]
        return run_with_control(&args, program.trigger.is_some());
        #[cfg(not(feature = "live"))]
        bail!(
            "run --control{} needs the live control endpoint, and this build has none; use the full build",
            if args.break_at.is_empty() { "" } else { " --break-at" }
        );
    }
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
    if capability_policy.is_some() {
        runtime::validate_with_arguments(&program, &registry, &inputs)?;
        flow::capabilities::preflight(&program, &registry, &env::current_dir()?)?;
    }

    if args.what_if || args.confirm.is_some() {
        if args.command != "run" {
            bail!("--what-if and --confirm apply only to run");
        }
        runtime::validate_with_arguments(&program, &registry, &inputs)?;
        let path = args.file.to_string_lossy().into_owned();
        let plan = flow::agent::explain::explain_json(&program, &registry, &path)?;
        let run_directory = env::current_dir()?;
        let context = flow::agent::what_if::RunSettings {
            declarations: &program.inputs,
            inputs: &inputs,
            cwd: &run_directory,
            capability_policy: args.capability_policy.as_deref(),
            data_flow_policy: args.data_flow_policy.as_deref(),
            manifest_dir: args.manifest_dir.as_deref(),
            manifest_catalog: args.manifest_catalog.as_deref(),
        };
        let (fingerprint, settings) = flow::agent::what_if::fingerprint(&plan, &context)?;
        if args.what_if {
            write_json(&flow::agent::what_if::report(
                &path,
                &fingerprint,
                plan,
                settings,
            ))?;
            return Ok(());
        }
        if let Some(expected) = &args.confirm {
            if *expected != fingerprint {
                bail!(
                    "plan changed since --what-if: confirmed {expected}, current {fingerprint}; review again with --what-if"
                );
            }
        }
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
        "explain" if args.json => {
            // Validate with the same inputs as `check` before describing effects.
            runtime::validate_with_arguments(&program, &registry, &inputs)?;
            let path = args.file.to_string_lossy().into_owned();
            let plan = flow::agent::explain::explain_json(&program, &registry, &path)?;
            if args.allow.is_empty() && !args.policy_header {
                write_json(&plan)
            } else {
                // Gate mode: report violations, and exit nonzero unless all are approved.
                // Approvals come only from --allow. The policy header declares intent:
                // with --policy-header, an undeclared category is a violation too.
                let allowed: std::collections::BTreeSet<String> =
                    args.allow.iter().cloned().collect();
                let declared = if args.policy_header {
                    let source = fs::read_to_string(&args.file)
                        .with_context(|| format!("reading {}", args.file.display()))?;
                    Some(flow::agent::policy::header_categories(&source)?)
                } else {
                    None
                };
                let verdict = flow::agent::gate::verdict(&plan, &allowed, declared.as_ref());
                let passed = verdict["allowed"] == true;
                write_json(&verdict)?;
                if !passed {
                    std::process::exit(1);
                }
                Ok(())
            }
        }
        "explain" => {
            if !args.allow.is_empty() {
                bail!("--allow requires --json");
            }
            if args.policy_header {
                bail!("--policy-header requires --json");
            }
            writeln!(
                io::stdout(),
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
            if args.once {
                if args.checkpoint_dir.is_some() {
                    bail!("--once cannot be combined with checkpoints");
                }
                if !matches!(program.trigger, Some(Trigger::Cron { .. })) {
                    bail!("--once applies only to calendar-triggered programs");
                }
                // A timer activation runs the handler now; the schedule is not re-armed.
                let mut body = program.clone();
                body.trigger = None;
                let result = runtime::run(&body, &registry, &options)?;
                io::stdout()
                    .write_all(&result.output)
                    .context("could not write workflow output")?;
                if let Some(exit) = result.exit {
                    // Same contract as the finite path: flush output, report the
                    // message, then exit with the program's status.
                    io::stdout()
                        .flush()
                        .context("could not write workflow output")?;
                    if let Some(message) = &exit.message {
                        runtime::write_diagnostic(format!("{message}\n").as_bytes());
                    }
                    std::process::exit(i32::from(exit.status));
                }
                return Ok(());
            }
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
                let result = if let Some(directory) = args.record_dir.as_deref() {
                    if checkpoint.is_some() {
                        bail!("--record cannot be combined with checkpoints");
                    }
                    record_cli::run_recorded(directory, &args.file, &program, &registry, &options)?
                } else {
                    // Uncaptured stdout streams to this terminal as it is produced.
                    runtime::with_streamed_stdout(|| {
                        if let Some(checkpoint) = checkpoint.as_ref() {
                            runtime::run_checkpointed(&program, &registry, &options, checkpoint)
                        } else {
                            runtime::run_with_drain_timeout(
                                &program,
                                &registry,
                                &options,
                                args.drain_timeout.unwrap_or(runtime::DEFAULT_DRAIN_TIMEOUT),
                            )
                        }
                    })?
                };
                io::stdout()
                    .write_all(&result.output)
                    .context("could not write workflow output")?;
                if let Some(exit) = result.exit {
                    // The pipeline output above is already delivered. Flush it
                    // before exiting, since process::exit skips buffer drops.
                    io::stdout()
                        .flush()
                        .context("could not write workflow output")?;
                    if let Some(message) = &exit.message {
                        runtime::write_diagnostic(format!("{message}\n").as_bytes());
                    }
                    std::process::exit(i32::from(exit.status));
                }
            }
            Ok(())
        }
        _ => unreachable!("argument parser only accepts supported commands"),
    }
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

/// The gate and the what-if/confirm checks cover finite programs only. Refuse them
/// on graph, screen, and stream programs rather than printing an unchecked plan.
fn reject_finite_only_options(args: &Args, program_kind: &str) -> Result<()> {
    if !args.allow.is_empty() {
        bail!("--allow applies only to finite programs, not {program_kind} programs");
    }
    if args.command == "explain" && args.json {
        bail!("explain --json applies only to finite programs, not {program_kind} programs");
    }
    if args.what_if || args.confirm.is_some() {
        bail!("--what-if and --confirm apply only to finite programs, not {program_kind} programs");
    }
    if args.policy_header {
        bail!("--policy-header applies only to finite programs, not {program_kind} programs");
    }
    if args.control {
        bail!("--control applies only to finite programs, not {program_kind} programs");
    }
    Ok(())
}

/// `run --control`: host a finite program under the live control endpoint. Options
/// the live host cannot honour are refused by name, not ignored.
#[cfg(feature = "live")]
fn run_with_control(args: &Args, triggered: bool) -> Result<()> {
    let refused = [
        (args.checkpoint_dir.is_some(), "--checkpoint"),
        (args.once, "--once"),
        (args.trace, "--trace"),
        (args.headless, "--headless"),
        (args.max_runs.is_some(), "--max-runs"),
        (args.missed_ticks.is_some(), "--missed-ticks"),
        (args.max_catch_up.is_some(), "--max-catch-up"),
        (args.calendar_dedup.is_some(), "--calendar-dedup"),
        (args.total_timeout.is_some(), "--total-timeout-ms"),
        (args.retry_uncertain, "--retry-uncertain"),
        (args.max_record_bytes_set, "--max-record-bytes"),
        (args.record_dir.is_some(), "--record"),
        (!args.allow.is_empty(), "--allow"),
        (args.what_if, "--what-if"),
        (args.confirm.is_some(), "--confirm"),
        (args.policy_header, "--policy-header"),
        (
            !args.program_arguments.is_empty(),
            "positional program arguments (pass named inputs as --input NAME=VALUE)",
        ),
        (triggered, "a triggered program"),
    ];
    let names: Vec<&str> = refused
        .iter()
        .filter(|(set, _)| *set)
        .map(|(_, name)| *name)
        .collect();
    if !names.is_empty() {
        bail!("--control cannot be combined with {}", names.join(", "));
    }
    let run = flow::live::HostRun {
        file: args.file.clone(),
        inputs: args.inputs.clone(),
        manifest_dir: args.manifest_dir.clone(),
        manifest_catalog: args.manifest_catalog.clone(),
        capability_policy: args.capability_policy.clone(),
        data_flow_policy: args.data_flow_policy.clone(),
        cache_dir: args.cache_dir.clone(),
        command_timeout_ms: args
            .command_timeout
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        max_output_bytes: args.max_output_bytes,
        breakpoints: args.break_at.clone(),
    };
    // The host has already written the program's output and diagnostics. Exit with
    // the program's status, as plain run does; process::exit skips buffer drops.
    let status = flow::live::host_run(&run, default_manifest_dir(), workflow_path)?;
    if status != 0 {
        std::process::exit(status);
    }
    Ok(())
}

fn applets_cli(arguments: &[String]) -> Result<()> {
    const HELP: &str = "Usage: execute applets list [--manifest-dir PATH | --manifest-catalog FILE]\n       execute applets inspect NAME_OR_PHRASE [--manifest-dir PATH | --manifest-catalog FILE]\n       execute applets discover\n       execute applets match --json REQUEST [--manifest-dir PATH | --manifest-catalog FILE]\n\nThe manifest selectors are mutually exclusive. --manifest-dir keeps its directory or single-manifest-file behavior; --manifest-catalog explicitly selects a packed catalog. No catalog is merged or selected automatically. Inspect emits validated contract metadata without executing applets. Discover reports installed commands using PATH metadata only. Match ranks declared typed ports without running applets. Versioned contracts expose typed I/O, outcomes, effects, backends, and lifecycle while retaining legacy fields for compatibility.\nUse -- before a name or phrase beginning with a hyphen.";
    if arguments == ["--help"] || arguments == ["-h"] {
        writeln!(io::stdout(), "{HELP}")?;
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
        let matches = flow::discovery::rank_port_matches(&registry, &request)?;
        return write_json(&serde_json::json!({
            "protocol": "execute.applet-matches/1",
            "validation": "declared_contracts",
            "matches": matches
        }));
    }
    if let Some(name) = name {
        write_json(&serde_json::json!({
            "protocol": "execute.applet/1", "validation": "manifest_static",
            "applet": registry.resolve(name)?.contract()
        }))
    } else {
        let mut manifests = registry.manifests()?;
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        write_json(&serde_json::json!({
            "protocol": "execute.applets/1", "validation": "manifest_static",
            "applets": manifests.iter().map(|manifest| manifest.contract()).collect::<Vec<_>>()
        }))
    }
}

/// `execute compile --to sh FILE [--out PATH]`: write a checked POSIX sh script.
/// Without `--out` the script goes to standard output; with it, only that path is written.
fn compile_cli(arguments: &[String]) -> Result<()> {
    const HELP: &str = "Usage: execute compile --to sh FILE [--out PATH] [--manifest-dir DIR | --manifest-catalog FILE]\n\nCompile a program to a POSIX sh script. Statements with no faithful sh equivalent are refused by name and line; nothing is approximated. Output goes to standard output unless --out names a file.";
    if arguments == ["--help"] || arguments == ["-h"] {
        writeln!(io::stdout(), "{HELP}")?;
        return Ok(());
    }
    let mut target = None;
    let mut output = None;
    let mut file = None;
    let mut manifest_dir = None;
    let mut manifest_catalog = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--to" => {
                let value = arguments.get(index + 1).context(HELP)?;
                if value != "sh" {
                    bail!("compile supports only --to sh\n{HELP}");
                }
                target = Some(value.as_str());
                index += 2;
            }
            "--out" => {
                output = Some(PathBuf::from(arguments.get(index + 1).context(HELP)?));
                index += 2;
            }
            "--manifest-dir" => {
                if manifest_dir.is_some() {
                    bail!("--manifest-dir may be specified only once");
                }
                if manifest_catalog.is_some() {
                    bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
                }
                manifest_dir = Some(PathBuf::from(arguments.get(index + 1).context(HELP)?));
                index += 2;
            }
            "--manifest-catalog" => {
                if manifest_catalog.is_some() {
                    bail!("--manifest-catalog may be specified only once");
                }
                if manifest_dir.is_some() {
                    bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
                }
                manifest_catalog = Some(PathBuf::from(arguments.get(index + 1).context(HELP)?));
                index += 2;
            }
            flag if flag.starts_with('-') => bail!("unknown compile option {flag:?}\n{HELP}"),
            path => {
                if file.replace(PathBuf::from(path)).is_some() {
                    bail!("compile takes one program file\n{HELP}");
                }
                index += 1;
            }
        }
    }
    if target.is_none() {
        bail!("compile needs --to sh\n{HELP}");
    }
    let file = file.context(HELP)?;
    let registry = load_registry(manifest_dir, manifest_catalog)?;
    let bytes = fs::read(&file).with_context(|| format!("reading program {}", file.display()))?;
    let source = std::str::from_utf8(&bytes)
        .with_context(|| format!("program {} is not valid UTF-8", file.display()))?;
    let cwd = fs::canonicalize(env::current_dir()?)?;
    let program = flow::program_files::parse_source(
        &file,
        source,
        Some(flow::program_files::Access {
            registry: &registry,
            cwd: &cwd,
        }),
    )
    .with_context(|| format!("parsing program {}", file.display()))?;
    let script =
        flow::compile_sh::compile(&program, &registry, &file.display().to_string(), &bytes)?;
    match output {
        Some(path) => fs::write(&path, script)
            .with_context(|| format!("writing compiled script {}", path.display()))?,
        None => io::stdout().write_all(script.as_bytes())?,
    }
    Ok(())
}

fn modules_cli(arguments: &[String]) -> Result<()> {
    const HELP: &str = "Usage: execute modules inspect MANIFEST [--root DIR] [--lock FILE]\n       execute modules verify MANIFEST --lock FILE [--root DIR]\n       execute modules lock MANIFEST --output FILE [--root DIR]\n\nValidate a local program-module package graph. No packages are fetched or installed. A lock command writes only to its explicit --output path.";
    if arguments == ["--help"] || arguments == ["-h"] {
        writeln!(io::stdout(), "{HELP}")?;
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
    const HELP: &str = "Usage: execute service generate <systemd|systemd-timer|launchd> FILE [--name ID] [--persistent] [--manifest-dir DIR | --manifest-catalog FILE] [--working-dir DIR] [--capability-policy FILE] [--data-flow-policy FILE] [--input NAME=VALUE ...] [-- ARG...]\n\nThe manifest selectors are mutually exclusive. --manifest-dir keeps its directory or single-manifest-file behavior; --manifest-catalog explicitly selects a packed catalog. No catalog is merged or selected automatically. Print a validated per-user service definition. This command never writes, installs, enables, or starts a service.";
    if arguments == ["--help"] || arguments == ["-h"] {
        writeln!(io::stdout(), "{HELP}")?;
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
    let mut persistent = false;
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
            "--persistent" => {
                if persistent {
                    bail!("--persistent may be specified once");
                }
                persistent = true;
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
    let program = flow::program_files::parse_source(
        &positional[1],
        &source,
        Some(flow::program_files::Access {
            registry: &registry,
            cwd: &working_directory,
        }),
    )
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
        persistent,
    };
    let rendered = match manifest_catalog.as_deref() {
        Some(path) => {
            flow::service_deployment::render_for_workflow_with_manifest_catalog(&request, path)?
        }
        None => flow::service_deployment::render_for_workflow(&request)?,
    };
    write!(io::stdout(), "{rendered}")?;
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
        (None, None) => Registry::load_default(&default_manifest_dir()),
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
    // The working directory is never searched: any directory a script runs in
    // could otherwise shadow bundled applets. Use --manifest-dir or
    // --manifest-catalog to select other manifests explicitly.
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

pub(crate) fn usage() -> &'static str {
    "Usage: execute FILE_OR_FILE_URI [options] [-- ARG...]\n       execute <check|explain|inspect|run> FILE [options]\n       execute resume FILE --checkpoint DIR [options]\n       execute applets <list|inspect|discover|match> [options]\n       execute modules <inspect|verify|lock> MANIFEST [options]\n       execute service generate <systemd|launchd> FILE [options]\n       execute lsp [--stdio]\n       execute live <serve|read|create|update|delete|patch|control|send|event|wait|schema> ...\n       execute model <classify|judge|score|generate|extract> [options]\n       execute model --help\n       execute --help\n\nOptions:\n  --json                  Structured static validation result (check only)\n  --input NAME=VALUE     Supply a declared program input (repeat for different names)\n  -- ARG...               Literal program arguments for declared positional inputs\n  --manifest-dir DIR       Directory or one legacy JSON file containing applet manifests\n  --manifest-catalog FILE Explicit packed catalog; mutually exclusive with --manifest-dir\n  --capability-policy FILE Enforce a strict, versioned applet policy (check/explain/inspect/run/resume/live serve)\n  --data-flow-policy FILE Enforce origin and persistence data-flow grants\n  --cache-dir DIR          Cache directory (run/resume)\n  --checkpoint DIR         Durable checkpoint directory (run/resume)\n  --retry-uncertain        Retry an invocation with an uncertain prior outcome (resume only)\n  --headless               Run a UI once and print its JSON state\n  --once                  Run one activation of a calendar-triggered handler and exit (run only)\n  --init                  Run as PID 1: supervise the worker, reap orphans, forward SIGINT/SIGTERM/SIGHUP (run only, Unix)\n  --trace                  Print execution details to stderr (run/resume/stream)\n  --max-output-bytes N     Maximum captured output size (run/resume)\n  --max-runs N             Maximum trigger runs or interactive UI jobs (not --headless)\n  --missed-ticks POLICY   Trigger backlog policy: skip, coalesce, or catch-up (run/resume)\n  --max-catch-up N         Bound catch-up runs to 1..64 (requires catch-up)\n  --calendar-dedup MODE   Calendar fold policy: instant or local (run/resume)\n  --command-timeout-ms N  Maximum lifetime of each native pipeline (run/resume/stream)\n  --drain-timeout-ms N    Budget for an interrupt drain before it cancels (run only; default 10000)\n  --max-record-bytes N     Maximum listener/stream record size"
}

fn write_json(value: &serde_json::Value) -> Result<()> {
    writeln!(io::stdout(), "{}", serde_json::to_string_pretty(value)?)?;
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

pub(crate) fn print_help() -> Result<()> {
    writeln!(
        io::stdout(),
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
