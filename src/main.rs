use anyhow::{anyhow, bail, Context, Result};
use flow::ast::Trigger;
use flow::capabilities::{CapabilityPolicy, ExecutionMode};
use flow::manifest::{ManifestCheck, Registry};
use flow::runtime::{self, CheckpointOptions, RunOptions, ServiceLimits};
use std::collections::BTreeMap;
use std::env;
use std::fs;
#[cfg(feature = "tooling")]
use std::io::Read;
use std::io::{self, Write};
use std::net::ToSocketAddrs;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};

mod cli;
#[cfg(any(feature = "tooling", feature = "live"))]
use cli::workflow_path;
use cli::{parse_args, take_option_value, Args};
#[cfg(any(feature = "tooling", feature = "records"))]
mod execution_policy_options;
mod github_annotations;
mod manifest_cache;
#[cfg(feature = "records")]
mod record_cli;
#[cfg(feature = "records")]
mod repair_cli;
#[cfg(feature = "records")]
mod repl_cli;
#[cfg(feature = "tooling")]
mod run_parts;
#[cfg(feature = "tooling")]
mod test_runner;

struct FiniteCheckSnapshot {
    source: String,
    registry: Registry,
}

#[derive(Debug)]
struct RedactedRuntimeDiagnostic {
    message: String,
    status: Option<i32>,
    code: &'static str,
    category: &'static str,
    closed_stdout: bool,
}

impl std::fmt::Display for RedactedRuntimeDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RedactedRuntimeDiagnostic {}

fn redact_runtime_error(registry: &Registry, error: anyhow::Error) -> anyhow::Error {
    let closed_stdout = is_closed_stdout(&error);
    let (code, category) = flow::agent::errors::classify(&error);
    let status = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<runtime::CommandFailed>()
            .map(|failure| {
                failure
                    .status
                    .code()
                    .unwrap_or_else(|| signal_status(&failure.status))
            })
    });
    let message = flow::security::secret_redactor(registry).redact_text(&format!("{error:#}"));
    anyhow::Error::new(RedactedRuntimeDiagnostic {
        message,
        status,
        code,
        category,
        closed_stdout,
    })
}

#[cfg(any(not(feature = "screens"), not(feature = "graph")))]
fn strip_source_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in line.bytes().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b'#' {
            return &line[..index];
        }
    }
    line
}

#[cfg(feature = "screens")]
fn is_screen_source(source: &str) -> bool {
    flow::ui::syntax::is_screen(source)
}

#[cfg(not(feature = "screens"))]
fn is_screen_source(source: &str) -> bool {
    source
        .lines()
        .map(strip_source_comment)
        .map(str::trim)
        .find(|line| !line.is_empty())
        .is_some_and(|line| line.starts_with("open a screen "))
}

#[cfg(feature = "graph")]
fn is_graph_source(source: &str) -> bool {
    flow::graph::is_graph(source)
}

#[cfg(not(feature = "graph"))]
fn is_graph_source(source: &str) -> bool {
    for line in source.lines().map(strip_source_comment).map(str::trim) {
        if line.is_empty() || line.split_ascii_whitespace().next() == Some("accept") {
            continue;
        }
        return line.starts_with("graph ");
    }
    false
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

/// Only graph runs register this cancellation handler.
#[cfg(feature = "graph")]
struct SignalRegistration(signal_hook::SigId);

#[cfg(feature = "graph")]
impl Drop for SignalRegistration {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.0);
    }
}

#[cfg(feature = "graph")]
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
    #[cfg(unix)]
    if let Err(error) = runtime::mark_inherited_descriptors_close_on_exec() {
        std::process::exit(report_error(&error));
    }
    if let Err(error) = execute() {
        std::process::exit(report_error(&error));
    }
}

/// Print a failed run's diagnostic and return its exit status. The normal entry point and
/// a forwarded run in a daemon child both use it.
fn report_error(error: &anyhow::Error) -> i32 {
    if is_closed_stdout(error)
        || error
            .downcast_ref::<RedactedRuntimeDiagnostic>()
            .is_some_and(|diagnostic| diagnostic.closed_stdout)
    {
        // The reader went away; stop quietly without a diagnostic.
        return STDOUT_CLOSED_EXIT;
    }
    // The run has already cleaned up its children before returning this error.
    // An interrupt keeps its status; otherwise the failing command decides.
    let interrupted = runtime::interrupt_exit_code();
    let status = interrupted.unwrap_or_else(|| {
        error
            .downcast_ref::<RedactedRuntimeDiagnostic>()
            .and_then(|diagnostic| diagnostic.status)
            .or_else(|| {
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
            })
            .unwrap_or(1)
    });
    if flow::agent::errors::enabled() {
        if let Some(diagnostic) = error.downcast_ref::<RedactedRuntimeDiagnostic>() {
            let (code, category) = interrupted
                .map_or((diagnostic.code, diagnostic.category), |_| {
                    ("interrupted", "signal")
                });
            let mut line = serde_json::json!({
                "protocol": flow::agent::errors::PROTOCOL,
                "code": code,
                "category": category,
                "message": diagnostic.message,
                "exit_code": status,
            })
            .to_string();
            line.push('\n');
            runtime::write_exit_diagnostic(line.as_bytes());
        } else {
            flow::agent::errors::write_stderr(error, interrupted.is_some(), status);
        }
    } else {
        runtime::write_exit_diagnostic(
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
    flow::security::install_panic_hook("daemon-run", None);
    let Some(args) = parse_args(&arguments)? else {
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
    let registry = load_registry(None, None, ManifestCheck::Lazy)?;
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
    let Ok(Some(args)) = parse_args(arguments) else {
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
    let raw_arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let arguments = apply_child_environment_options(parse_os_arguments(raw_arguments)?)?;
    if let Some(status) = daemon_forward(&arguments) {
        std::process::exit(status);
    }
    execute_arguments(arguments)
}

/// Consume host-level child-environment options before dispatch. Keeping this
/// outside the workflow parser makes the same flags available to `run-parts`,
/// service generation, daemon forwarding, and the `model` helper subcommand.
fn apply_child_environment_options(arguments: Vec<String>) -> Result<Vec<String>> {
    let boundary = arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(arguments.len());
    let mut filtered = Vec::with_capacity(arguments.len());
    let mut allow_names = Vec::new();
    let mut saw_clear = false;
    let mut saw_allow = false;
    let mut index = 0;
    while index < arguments.len() {
        if index >= boundary {
            filtered.extend(arguments[index..].iter().cloned());
            break;
        }
        match arguments[index].as_str() {
            "--clear-env" => {
                if saw_clear || saw_allow {
                    bail!("--clear-env may be specified only once and cannot be combined with --env-allow");
                }
                saw_clear = true;
            }
            "--env-allow" => {
                if saw_clear {
                    bail!("--clear-env and --env-allow cannot be combined");
                }
                saw_allow = true;
                index += 1;
                let value = arguments
                    .get(index)
                    .context("--env-allow requires NAME[,NAME...]")?;
                if value.is_empty() {
                    bail!("--env-allow requires at least one name; use --clear-env to pass none");
                }
                allow_names.extend(flow::capabilities::parse_child_environment_allowlist(
                    value,
                )?);
            }
            _ => filtered.push(arguments[index].clone()),
        }
        index += 1;
    }
    if saw_clear {
        env::set_var("EXECUTE_CHILD_ENV_ALLOW", "");
    } else if saw_allow {
        allow_names.sort();
        allow_names.dedup();
        let encoded = allow_names.join(",");
        flow::capabilities::parse_child_environment_allowlist(&encoded)?;
        env::set_var("EXECUTE_CHILD_ENV_ALLOW", encoded);
    }
    Ok(filtered)
}

fn parse_os_arguments(raw_arguments: Vec<std::ffi::OsString>) -> Result<Vec<String>> {
    let mut json_errors = false;
    let literal_boundary = raw_arguments.iter().position(|argument| {
        if argument == "--" {
            true
        } else {
            json_errors |= argument == flow::agent::errors::FLAG;
            false
        }
    });
    let parsed = if !json_errors {
        // The common path already owns the argument vector. Reuse it directly rather than
        // allocating another Vec<OsString> in `strip_flag` before collecting the strings.
        raw_arguments
    } else {
        match literal_boundary {
            Some(boundary) => {
                let mut parsed =
                    flow::agent::errors::strip_flag(raw_arguments[..boundary].iter().cloned());
                parsed.extend(raw_arguments[boundary..].iter().cloned());
                parsed
            }
            None => flow::agent::errors::strip_flag(raw_arguments),
        }
    };
    parsed
        .into_iter()
        .enumerate()
        .map(|(index, argument)| {
            argument.into_string().map_err(|_| {
                anyhow!("argument {} is not valid UTF-8; use a percent-encoded file:// URI for non-UTF-8 workflow paths", index + 1)
            })
        })
        .collect()
}

fn execute_arguments(arguments: Vec<String>) -> Result<()> {
    if arguments.len() == 2
        && matches!(arguments[1].as_str(), "--help" | "-h")
        && is_help_subcommand(&arguments[0])
    {
        let name = &arguments[0];
        let synopsis = match name.as_str() {
            "test" => "Usage: execute test [OPTIONS] FILE...",
            "run-parts" => "Usage: execute run-parts [OPTIONS] DIRECTORY",
            "show-run" => "Usage: execute show-run [OPTIONS] RECORD",
            "replay" => "Usage: execute replay [OPTIONS] RECORD",
            "repl" => "Usage: execute repl [OPTIONS] RECORD",
            "repair" => "Usage: execute repair CHECKPOINT --op OPERATION",
            "lsp" => "Usage: execute lsp [--stdio]",
            "help" => "Usage: execute help [--json] FILE",
            "attach" => "Usage: execute attach [OPTIONS] SESSION",
            _ => "Usage: execute SUBCOMMAND [OPTIONS]",
        };
        writeln!(io::stdout(), "{synopsis}")?;
        return Ok(());
    }
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
        flow::security::install_panic_hook("live", None);
        return flow::live::cli(&arguments[1..], default_manifest_dir()?, workflow_path);
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
        #[cfg(feature = "tooling")]
        return test_runner::cli(&arguments[1..]);
        #[cfg(not(feature = "tooling"))]
        bail!("`test` requires the tooling feature; rebuild with --features tooling");
    }
    if arguments.first().is_some_and(|a| a == "run-parts") {
        #[cfg(feature = "tooling")]
        return run_parts::cli(&arguments[1..]);
        #[cfg(not(feature = "tooling"))]
        bail!("`run-parts` requires the tooling feature; rebuild with --features tooling");
    }
    if arguments.first().is_some_and(|a| a == "show-run") {
        #[cfg(feature = "records")]
        return record_cli::show_run(&arguments[1..]);
        #[cfg(not(feature = "records"))]
        bail!("`show-run` requires the records feature; rebuild with --features records");
    }
    if arguments.first().is_some_and(|a| a == "replay") {
        #[cfg(feature = "records")]
        return record_cli::replay(&arguments[1..]);
        #[cfg(not(feature = "records"))]
        bail!("`replay` requires the records feature; rebuild with --features records");
    }
    if arguments.first().is_some_and(|a| a == "repl") {
        flow::security::install_panic_hook("repl", None);
        #[cfg(feature = "records")]
        return repl_cli::repl(&arguments[1..]);
        #[cfg(not(feature = "records"))]
        bail!("`repl` requires the records feature; rebuild with --features records");
    }
    if arguments.first().is_some_and(|a| a == "repair") {
        #[cfg(feature = "records")]
        return repair_cli::repair(&arguments[1..]);
        #[cfg(not(feature = "records"))]
        bail!("`repair` requires the records feature; rebuild with --features records");
    }
    #[cfg(feature = "lsp")]
    if arguments.first().is_some_and(|a| a == "lsp") {
        flow::security::install_panic_hook("lsp", None);
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
        #[cfg(feature = "module-tools")]
        return modules_cli(&arguments[1..]);
        #[cfg(not(feature = "module-tools"))]
        bail!("`modules` requires the module-tools feature; rebuild with --features module-tools");
    }
    if arguments.first().is_some_and(|a| a == "compile") {
        #[cfg(feature = "tooling")]
        return compile_cli(&arguments[1..]);
        #[cfg(not(feature = "tooling"))]
        bail!("`compile` requires the tooling feature; rebuild with --features tooling");
    }
    if arguments.first().is_some_and(|a| a == "service") {
        #[cfg(feature = "tooling")]
        return service_cli(&arguments[1..]);
        #[cfg(not(feature = "tooling"))]
        bail!("`service` requires the tooling feature; rebuild with --features tooling");
    }
    if arguments.first().is_some_and(|a| a == "model") {
        #[cfg(not(feature = "model"))]
        bail!(
            "`model` requires the model feature; rebuild with --features model (or --features full)"
        );
        #[cfg(feature = "model")]
        {
            #[cfg(unix)]
            {
                let helper = runtime::bundled_model_applet()?;
                let mut command = std::process::Command::new(helper);
                flow::capabilities::apply_host_child_environment(&mut command)?;
                command.args(&arguments[1..]);
                return Err(std::os::unix::process::CommandExt::exec(&mut command))
                    .context("starting the bundled model applet");
            }
            #[cfg(windows)]
            {
                std::process::exit(runtime::model_cli(&arguments[1..])?);
            }
        }
    }
    if arguments.first().is_some_and(|a| a == "checkpoint") {
        #[cfg(feature = "agent-advisory")]
        std::process::exit(flow::agent::checkpoint::cli(&arguments[1..])?);
        #[cfg(not(feature = "agent-advisory"))]
        bail!(
            "`checkpoint` requires the agent-advisory feature; rebuild with --features agent-advisory"
        );
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
        flow::security::install_panic_hook("dap", None);
        std::process::exit(flow::agent::dap::cli(&arguments[1..])?);
    }
    #[cfg(not(feature = "dap"))]
    if arguments.first().is_some_and(|a| a == "dap") {
        bail!("`dap` requires the dap feature; rebuild with --features dap (or --features full)");
    }
    if arguments.first().is_some_and(|a| a == "daemon") {
        flow::security::install_panic_hook("daemon", None);
        std::process::exit(flow::agent::daemon::cli(
            &arguments[1..],
            forwarded_run,
            preload_daemon_registry,
            warm_forwarded_manifests,
        )?);
    }
    let args = match parse_args(&arguments)? {
        Some(args) => args,
        None => return Ok(()),
    };

    if let Some(code) = args.diagnostic_explain.as_deref() {
        let info = flow::diagnostics::code_info(code)
            .ok_or_else(|| anyhow!("unknown diagnostic code {code:?}"))?;
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "Diagnostic code: {}", info.code)?;
        writeln!(stdout, "Family: {}", info.family)?;
        writeln!(stdout, "Meaning: {}", info.meaning)?;
        writeln!(stdout, "Example:\n{}", info.example)?;
        writeln!(stdout, "Typical fix: {}", info.typical_fix)?;
        return Ok(());
    }

    if args.command == "check" {
        let json = args.json;
        let path = args.file.to_string_lossy().into_owned();
        let path_bytes = args.file.as_os_str().as_encoded_bytes().to_vec();
        let source = fs::read_to_string(&args.file).ok();
        #[cfg(not(feature = "screens"))]
        if source.as_deref().is_some_and(is_screen_source) {
            bail!("screen programs require the screens feature; rebuild with --features screens");
        }
        #[cfg(not(feature = "graph"))]
        if source.as_deref().is_some_and(is_graph_source) {
            bail!("graph programs require the graph feature; rebuild with --features graph");
        }
        let (static_diagnostics, finite_check_snapshot) = match source {
            Some(source)
                if !is_screen_source(&source)
                    && !flow::stream::is_stream(&source)
                    && !is_graph_source(&source) =>
            {
                match load_registry(
                    args.manifest_dir.clone(),
                    args.manifest_catalog.clone(),
                    ManifestCheck::Full,
                ) {
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
                                let mut diagnostics = flow::diagnostics::analyze_source_at(
                                    &source,
                                    Some(&args.file),
                                    &registry,
                                );
                                if args.resolve {
                                    diagnostics.extend(
                                        flow::diagnostics::resolve_executable_diagnostics(
                                            &source,
                                            Some(&args.file),
                                            &registry,
                                        ),
                                    );
                                }
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
        let diagnostic_source = if json {
            finite_check_snapshot
                .as_ref()
                .map(|snapshot| snapshot.source.clone())
        } else {
            None
        };
        let data_flow_warning = args.capability_policy.is_some() && args.data_flow_policy.is_none();
        let github = args.github;
        let deny_warnings = args.deny_warnings;
        let result = execute_workflow(args, finite_check_snapshot);
        if !json {
            // Plain check and --json share this analysis: the same static errors fail the
            // check, after the authoritative load's own error.
            result?;
            if data_flow_warning {
                eprintln!(
                    "warning[data_flow_policy_not_configured]: --capability-policy grants applet capabilities only; without --data-flow-policy, remote egress and persistence are not protected by data-flow grants"
                );
            }
            for diagnostic in static_diagnostics.iter().filter(|diagnostic| {
                diagnostic.severity == flow::diagnostics::DiagnosticSeverity::Warning
            }) {
                eprintln!(
                    "warning[{}]: line {}: {}",
                    diagnostic.code, diagnostic.span.line, diagnostic.message
                );
            }
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
            if deny_warnings
                && (data_flow_warning
                    || static_diagnostics.iter().any(|diagnostic| {
                        diagnostic.severity == flow::diagnostics::DiagnosticSeverity::Warning
                    }))
            {
                bail!("warnings are denied (--deny-warnings)");
            }
            return Ok(());
        }
        let has_static_error = static_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == flow::diagnostics::DiagnosticSeverity::Error);
        let has_denied_warning = deny_warnings
            && (data_flow_warning
                || static_diagnostics.iter().any(|diagnostic| {
                    diagnostic.severity == flow::diagnostics::DiagnosticSeverity::Warning
                }));
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
            if !has_static_error {
                let loop_error = error.downcast_ref::<runtime::ForEachInputTypeError>();
                let span = loop_error
                    .and_then(|error| error.line)
                    .zip(diagnostic_source.as_deref())
                    .map(|(line, source)| flow::diagnostics::source_line_span(source, line));
                let code = if span.is_some() {
                    "check.type_mismatch"
                } else {
                    "check_failed"
                };
                let mut diagnostic = serde_json::json!({
                    "severity": "error", "code": code,
                    "message": format!("{error:#}"),
                    "causes": error.chain().map(ToString::to_string).collect::<Vec<_>>()
                });
                if let Some(span) = span {
                    diagnostic["span"] = serde_json::to_value(span)?;
                }
                diagnostics.push(diagnostic);
            }
        }
        if github {
            github_annotations::write(&path, &diagnostics)?;
        } else {
            write_json(&serde_json::json!({
                "protocol": "execute.check/1", "validation": "static",
                "ok": result.is_ok() && !has_static_error && !has_denied_warning, "path": path, "path_bytes": path_bytes,
                "diagnostics": diagnostics
            }))?;
        }
        if result.is_err() || has_static_error || has_denied_warning {
            // Validation has no spawned workers/effects. The JSON response is
            // complete; preserve the ordinary failing-check exit status.
            std::process::exit(1);
        }
        return Ok(());
    }
    execute_workflow(args, None)
}

fn is_help_subcommand(name: &str) -> bool {
    matches!(
        name,
        "test"
            | "run-parts"
            | "show-run"
            | "replay"
            | "repl"
            | "repair"
            | "lsp"
            | "help"
            | "attach"
    )
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

/// Whether a source's leading comments mention an `execute-policy` header (after an
/// optional BOM), well-formed or not. Used by the graph path and by builds without the
/// policy checker.
#[cfg(any(feature = "graph", not(feature = "agent-advisory")))]
fn declares_policy_header(source: &str) -> bool {
    source
        .strip_prefix('\u{feff}')
        .unwrap_or(source)
        .lines()
        .map(str::trim)
        .take_while(|line| line.is_empty() || line.starts_with('#'))
        .any(|line| line.contains("execute-policy"))
}

/// A program's `# execute-policy:` header is enforced before any input is read or any
/// effect runs: a static plan with effects outside the declared categories is refused.
/// A build without the policy checker refuses a program that declares a header rather
/// than run it unchecked.
fn enforce_policy_header(
    source: &str,
    program: &flow::ast::Program,
    registry: &flow::manifest::Registry,
    path: &Path,
) -> Result<()> {
    #[cfg(feature = "agent-advisory")]
    {
        flow::agent::policy::enforce_header(source, program, registry, &path.to_string_lossy())
    }
    #[cfg(not(feature = "agent-advisory"))]
    {
        let _ = (program, registry, path);
        if declares_policy_header(source) {
            bail!(
                "this build cannot enforce the program's execute-policy header; rebuild with the agent-advisory feature"
            );
        }
        Ok(())
    }
}

fn execute_workflow(args: Args, finite_check_snapshot: Option<FiniteCheckSnapshot>) -> Result<()> {
    // Actions hardening for `run`/`resume`: neutralize forged workflow
    // commands in terminal-bound output and mask registered secrets. `check
    // --format github` intentionally emits workflow commands and never
    // enables this; only execution paths opt in.
    if matches!(args.command, "run" | "resume") {
        flow::github_actions::enable_if_ci();
    }
    let capability_policy = args
        .capability_policy
        .as_deref()
        .map(load_capability_policy)
        .transpose()?;
    if args.execution_mode == Some(ExecutionMode::Untrusted) && capability_policy.is_none() {
        bail!("untrusted execution requires a capability policy");
    }
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
    let _single_instance = if args.what_if {
        None
    } else {
        match args.single_instance.as_deref() {
            Some(path) => Some(flow::agent::instance::acquire(path)?),
            None => None,
        }
    };
    #[cfg(not(feature = "graph"))]
    if is_graph_source(&source) {
        bail!("graph programs require the graph feature; rebuild with --features graph");
    }
    #[cfg(feature = "graph")]
    if flow::graph::is_graph(&source) {
        flow::security::install_panic_hook("graph", None);
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
            ManifestCheck::for_command(args.command),
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        )?;
        if args.execution_mode.is_some() {
            registry.set_host_state(args.execution_mode);
        }
        let graph = flow::graph::parse(&source, &registry).context("parsing graph")?;
        if args.checkpoint_dir.is_some()
            && graph
                .input_declarations()
                .iter()
                .any(flow::ast::is_sensitive)
        {
            bail!(
                "--checkpoint cannot be used with a sensitive input: a journal would keep the value or data computed from it"
            );
        }
        // Graph plans are not checked against an execute-policy header yet, so a graph
        // that declares one is refused rather than run unchecked.
        if matches!(args.command, "run" | "resume")
            && !args.what_if
            && declares_policy_header(&source)
        {
            bail!(
                "execute-policy headers are not enforced for graphs yet; remove the header or express the work as a program"
            );
        }
        // A capability policy decides which environment variables a graph may read,
        // before any input is resolved.
        flow::capabilities::authorize_environment_inputs(graph.input_declarations(), &registry)?;
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
        let cwd = if capability_policy.is_some() || matches!(args.command, "run" | "resume") {
            Some(env::current_dir()?)
        } else {
            None
        };
        let resolved_cwd = if capability_policy.is_some()
            || (matches!(args.command, "run" | "resume") && args.checkpoint_dir.is_some())
        {
            Some(fs::canonicalize(
                cwd.as_deref()
                    .context("could not determine current directory")?,
            )?)
        } else {
            None
        };
        if capability_policy.is_some() {
            let cwd = resolved_cwd
                .as_deref()
                .context("could not resolve current directory")?;
            if args.command == "explain" {
                flow::capabilities::preflight_invocations_plan(
                    graph.invocations(),
                    &registry,
                    cwd,
                )?;
            } else {
                flow::capabilities::preflight_invocations(graph.invocations(), &registry, cwd)?;
            }
        }
        return match args.command {
            "check" => Ok(()),
            "inspect" => write_json(&serde_json::json!({
                "protocol": "execute.graph/1",
                "validation": "static",
                "interface": interface,
                "graph": serde_json::to_value(&graph)?
            })),
            "explain" => {
                writeln!(
                    io::stdout(),
                    "// Execution mode: {}",
                    flow::capabilities::execution_mode(&registry).as_str()
                )?;
                for node in graph.nodes() {
                    for call in node.invocations() {
                        writeln!(
                            io::stdout(),
                            "// node {:?}: {}",
                            node.id,
                            flow::capabilities::invocation_enforcement(&registry, call)?
                        )?;
                    }
                }
                writeln!(io::stdout(), "{}", graph.to_dot()).map_err(Into::into)
            }
            "run" | "resume" => {
                let options = RunOptions {
                    inputs,
                    cache_directory: args.cache_dir,
                    trace: args.trace,
                    max_output_bytes: args.max_output_bytes,
                    max_loop_iterations: args.max_loop_iterations,
                    command_timeout: args.command_timeout,
                    total_timeout: args.total_timeout,
                    ..RunOptions::new(
                        cwd.clone()
                            .context("could not determine current directory")?,
                    )
                };
                let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let _signal_registration =
                    register_cancellation_signal(std::sync::Arc::clone(&cancelled))?;
                let result = match args.checkpoint_dir.as_deref() {
                    Some(directory) => {
                        let cwd = resolved_cwd
                            .as_deref()
                            .context("could not resolve current directory")?;
                        let identity = flow::graph::checkpoint_identity(&graph, cwd)?;
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
    #[cfg(not(feature = "screens"))]
    if is_screen_source(&source) {
        flow::security::install_panic_hook("screen", None);
        bail!("screen programs require the screens feature; rebuild with --features screens");
    }
    #[cfg(feature = "screens")]
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
            ManifestCheck::for_command(args.command),
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        )?;
        let cwd = env::current_dir()?;
        if args.execution_mode.is_some() {
            registry.set_host_state(args.execution_mode);
        }
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
            if args.command == "explain" {
                flow::capabilities::preflight_plan(&policy_program, &registry, &cwd)?;
            } else {
                flow::capabilities::preflight(&policy_program, &registry, &cwd)?;
            }
        }
        if args.command == "resume" || args.checkpoint_dir.is_some() || args.max_record_bytes_set {
            bail!("UI programs do not support resume, checkpoints, or listener record limits");
        }
        let options = RunOptions {
            cache_directory: args.cache_dir,
            trace: args.trace,
            max_output_bytes: args.max_output_bytes,
            max_loop_iterations: args.max_loop_iterations,
            command_timeout: args.command_timeout,
            total_timeout: args.total_timeout,
            ..RunOptions::new(cwd.clone())
        };
        return match args.command {
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
                    "Execution mode: {}",
                    flow::capabilities::execution_mode(&registry).as_str()
                )?;
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
                                    "line {}: {:?} parameters {:?}; enforcement {}",
                                    step.line,
                                    manifest.binary_for(&call),
                                    call.parameters.keys().collect::<Vec<_>>(),
                                    flow::capabilities::invocation_enforcement(&registry, &call)?
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
        flow::security::install_panic_hook("stream", None);
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
            ManifestCheck::for_command(args.command),
            capability_policy.as_ref(),
            data_flow_policy.as_ref(),
        )?;
        let cwd = env::current_dir()?;
        if capability_policy.is_some() {
            if args.execution_mode.is_some() {
                registry.set_host_state(args.execution_mode);
            }
            flow::stream::validate(&stream, &registry)?;
            runtime::validate_with_arguments(&stream.pipeline, &registry, &inputs)?;
            if args.command == "explain" {
                flow::capabilities::preflight_plan(&stream.pipeline, &registry, &cwd)?;
            } else {
                flow::capabilities::preflight(&stream.pipeline, &registry, &cwd)?;
            }
        }

        return match args.command {
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
                writeln!(
                    io::stdout(),
                    "Execution mode: {}",
                    flow::capabilities::execution_mode(&registry).as_str()
                )?;
                let mut plan = flow::agent::explain::explain_json(
                    &stream.pipeline,
                    &registry,
                    &args.file.to_string_lossy(),
                )?;
                flow::capabilities::annotate_enforcement(&mut plan, &registry);
                if let Some(steps) = plan["steps"].as_array() {
                    for step in steps.iter().filter(|step| step["process"] == true) {
                        writeln!(
                            io::stdout(),
                            "  line {}: {}",
                            step["line"],
                            step["enforcement"].as_str().unwrap_or("refused")
                        )?;
                    }
                }
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
                    cache_directory: args.cache_dir,
                    trace: args.trace,
                    max_output_bytes: args.max_output_bytes,
                    max_loop_iterations: args.max_loop_iterations,
                    command_timeout: args.command_timeout,
                    total_timeout: args.total_timeout,
                    ..RunOptions::new(cwd.clone())
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
                )
                .map_err(|error| redact_runtime_error(&registry, error))?;
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
            ManifestCheck::for_command(args.command),
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
    if args.execution_mode.is_some() {
        registry.set_host_state(args.execution_mode);
    }
    // The registry carries the capability policy, which authorizes the reads of any
    // program file the program calls, so it is loaded before the program is parsed.
    let cwd = env::current_dir()?;
    let canonical_file = fs::canonicalize(&args.file)
        .with_context(|| format!("resolving {}", args.file.display()))?;
    let program = flow::program_files::parse_source_resolved(
        &canonical_file,
        &source,
        Some(flow::program_files::Access {
            registry: &registry,
            cwd: &cwd,
        }),
    )
    .with_context(|| format!("could not parse workflow {}", args.file.display()))?;
    if args.checkpoint_dir.is_some() {
        flow::security::install_panic_hook("checkpoint", Some(&program.steps));
    } else if program.trigger.is_some() {
        let host = if matches!(program.trigger, Some(Trigger::Listen { .. })) {
            "listener"
        } else {
            "service"
        };
        flow::security::install_panic_hook(host, Some(&program.steps));
    }
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
    // Only a real run or resume is refused; explain, check and --what-if report the plan.
    if matches!(args.command, "run" | "resume") && !args.what_if {
        enforce_policy_header(&source, &program, &registry, &canonical_file)?;
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
            if args.break_at.is_empty() {
                ""
            } else {
                " --break-at"
            }
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
        if args.command == "explain" || args.what_if {
            flow::capabilities::preflight_plan(&program, &registry, &cwd)?;
        } else {
            flow::capabilities::preflight(&program, &registry, &cwd)?;
        }
    }

    #[cfg(feature = "agent-advisory")]
    if args.what_if || args.confirm.is_some() {
        if args.command != "run" {
            bail!("--what-if and --confirm apply only to run");
        }
        runtime::validate_with_arguments(&program, &registry, &inputs)?;
        let path = args.file.to_string_lossy().into_owned();
        let legacy = args.explain_version == Some(1);
        let mut plan = if legacy {
            flow::agent::explain::explain_json_v1(&program, &registry, &path)?
        } else {
            flow::agent::explain::explain_json_with_inputs(&program, &registry, &path, &inputs)?
        };
        let has_native_process = plan["steps"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["process"] == true));
        if has_native_process {
            let path_hmac_key = flow::agent::what_if_key::load_or_create()
                .context("what-if review needs its private key to bind executable search paths")?;
            plan = flow::agent::explain::explain_json_for_what_if(
                &program,
                &registry,
                &path,
                &inputs,
                &path_hmac_key,
                legacy,
            )?;
        }
        flow::capabilities::annotate_enforcement(&mut plan, &registry);
        let run_directory = cwd.clone();
        let context = flow::agent::what_if::RunSettings {
            declarations: &program.inputs,
            inputs: &inputs,
            cwd: &run_directory,
            capability_policy: args.capability_policy.as_deref(),
            data_flow_policy: args.data_flow_policy.as_deref(),
            manifest_dir: args.manifest_dir.as_deref(),
            manifest_catalog: args.manifest_catalog.as_deref(),
            max_loop_iterations: args.max_loop_iterations,
            loop_budget_scope: runtime::LoopBudgetScope::PerLoop,
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
    #[cfg(not(feature = "agent-advisory"))]
    if args.what_if || args.confirm.is_some() {
        bail!(
            "--what-if and --confirm require the agent-advisory feature; rebuild with --features agent-advisory"
        );
    }

    match args.command {
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
            let mut plan = if args.explain_version == Some(1) {
                flow::agent::explain::explain_json_v1(&program, &registry, &path)?
            } else {
                flow::agent::explain::explain_json_with_inputs(&program, &registry, &path, &inputs)?
            };
            flow::capabilities::annotate_enforcement(&mut plan, &registry);
            if args.allow.is_empty() && !args.policy_header {
                write_json(&plan)
            } else {
                #[cfg(feature = "agent-advisory")]
                {
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
                #[cfg(not(feature = "agent-advisory"))]
                {
                    bail!(
                        "explain --json effect gates require the agent-advisory feature; rebuild with --features agent-advisory"
                    )
                }
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
                "Execution mode: {}",
                flow::capabilities::execution_mode(&registry).as_str()
            )?;
            let mut plan = flow::agent::explain::explain_json(
                &program,
                &registry,
                &args.file.to_string_lossy(),
            )?;
            flow::capabilities::annotate_enforcement(&mut plan, &registry);
            if let Some(steps) = plan["steps"].as_array() {
                for step in steps.iter().filter(|step| step["process"] == true) {
                    writeln!(
                        io::stdout(),
                        "  line {}: {}",
                        step["line"],
                        step["enforcement"].as_str().unwrap_or("refused")
                    )?;
                }
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
                cache_directory: args.cache_dir,
                trace: args.trace,
                max_output_bytes: args.max_output_bytes,
                max_loop_iterations: args.max_loop_iterations,
                command_timeout: args.command_timeout,
                total_timeout: args.total_timeout,
                ..RunOptions::new(cwd.clone())
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
                let result = runtime::run(&body, &registry, &options)
                    .map_err(|error| redact_runtime_error(&registry, error))?;
                // Under Actions hardening the final output shares the stdout
                // sanitizer with streamed output, so a forged command split
                // across the stream boundary is still neutralized. The flush
                // releases a trailing line-leading `:` at true end of stream.
                // The sink lock spans filter and write so a concurrent mask
                // cannot interleave between classification and landing.
                flow::github_actions::write_stdout_final(&result.output)
                    .context("could not write workflow output")?;
                if let Some(exit) = result.exit {
                    // Same contract as the finite path: flush output, report the
                    // message, then exit with the program's status.
                    io::stdout()
                        .flush()
                        .context("could not write workflow output")?;
                    if let Some(message) = &exit.message {
                        let message =
                            flow::security::secret_redactor(&registry).redact_text(message);
                        runtime::write_exit_diagnostic(format!("{message}\n").as_bytes());
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
                        program_path: canonical_file.clone(),
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
                    )
                    .map_err(|error| redact_runtime_error(&registry, error))?;
                } else {
                    runtime::run_service_with_schedule_options(
                        &program, &registry, &options, &limits, &schedule, emit,
                    )
                    .map_err(|error| redact_runtime_error(&registry, error))?;
                }
            } else {
                #[cfg(feature = "records")]
                let result = if let Some(directory) = args.record_dir.as_deref() {
                    if checkpoint.is_some() {
                        bail!("--record cannot be combined with checkpoints");
                    }
                    flow::security::register_sensitive_inputs(
                        &registry,
                        &program.inputs,
                        &options.inputs,
                        &std::sync::atomic::AtomicBool::new(false),
                        None,
                    )?;
                    flow::security::register_sensitive_program(
                        &registry,
                        &program,
                        &std::sync::atomic::AtomicBool::new(false),
                        None,
                    )?;
                    record_cli::run_recorded(directory, &args.file, &program, &registry, &options)
                        .map_err(|error| redact_runtime_error(&registry, error))?
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
                    })
                    .map_err(|error| redact_runtime_error(&registry, error))?
                };
                #[cfg(not(feature = "records"))]
                let result = {
                    if args.record_dir.is_some() {
                        bail!(
                            "--record requires the records feature; rebuild with --features records"
                        );
                    }
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
                    })
                    .map_err(|error| redact_runtime_error(&registry, error))?
                };
                // Same end-of-stream sanitization as `--once`: the shared stdout
                // sanitizer keeps line-start state across streamed writes.
                // The sink lock spans filter and write (see above).
                flow::github_actions::write_stdout_final(&result.output)
                    .context("could not write workflow output")?;
                if let Some(exit) = result.exit {
                    // The pipeline output above is already delivered. Flush it
                    // before exiting, since process::exit skips buffer drops.
                    io::stdout()
                        .flush()
                        .context("could not write workflow output")?;
                    if let Some(message) = &exit.message {
                        let message =
                            flow::security::secret_redactor(&registry).redact_text(message);
                        runtime::write_exit_diagnostic(format!("{message}\n").as_bytes());
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
    flow::security::install_panic_hook("live", None);
    let refused = [
        (args.execution_mode.is_some(), "--execution-mode"),
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
        max_loop_iterations: args.max_loop_iterations,
        breakpoints: args.break_at.clone(),
    };
    // The host has already written the program's output and diagnostics. Exit with
    // the program's status, as plain run does; process::exit skips buffer drops.
    let status = flow::live::host_run(&run, default_manifest_dir()?, workflow_path)?;
    if status != 0 {
        std::process::exit(status);
    }
    Ok(())
}

fn applets_cli(arguments: &[String]) -> Result<()> {
    const HELP: &str = "Usage: execute applets list [--names | --signatures] [--manifest-dir PATH | --manifest-catalog FILE]\n       execute applets search QUERY [--limit N] [--json] [--manifest-dir PATH | --manifest-catalog FILE]\n       execute applets inspect NAME_OR_PHRASE [--json=compact] [--verbose] [--manifest-dir PATH | --manifest-catalog FILE]\n       execute applets discover\n       execute applets match --json REQUEST [--manifest-dir PATH | --manifest-catalog FILE]\n\nThe manifest selectors are mutually exclusive. --manifest-dir keeps its directory or single-manifest-file behavior; --manifest-catalog explicitly selects a packed catalog. No catalog is merged or selected automatically. List signatures and search provide compact contract-derived discovery. Inspect emits validated contract metadata without executing applets; --json=compact omits in-process argv templates unless --verbose is supplied. Discover reports installed commands using PATH metadata only. Match ranks declared typed ports without running applets. Versioned contracts expose typed I/O, outcomes, effects, backends, and lifecycle while retaining legacy fields for compatibility.\nUse -- before a name or phrase beginning with a hyphen.";
    if arguments == ["--help"] || arguments == ["-h"] {
        writeln!(io::stdout(), "{HELP}")?;
        return Ok(());
    }
    let command = arguments.first().context(HELP)?;
    if !matches!(
        command.as_str(),
        "list" | "search" | "inspect" | "discover" | "match"
    ) {
        bail!("unknown applets command {command:?}\n{HELP}");
    }
    #[cfg(not(feature = "tooling"))]
    if matches!(command.as_str(), "discover" | "match") {
        bail!("`applets {command}` requires the tooling feature; rebuild with --features tooling");
    }
    let mut manifest_dir = None;
    let mut manifest_catalog = None;
    let mut name = None;
    let mut request = None;
    let mut request_file = None;
    let mut search_words = Vec::new();
    let mut search_limit = 10usize;
    let mut search_json = false;
    let mut list_mode = None;
    let mut compact = false;
    let mut verbose = false;
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
        } else if !literal && argument == "--names" && command == "list" {
            if list_mode.replace("names").is_some() {
                bail!("choose only one of --names and --signatures");
            }
        } else if !literal && argument == "--signatures" && command == "list" {
            if list_mode.replace("signatures").is_some() {
                bail!("choose only one of --names and --signatures");
            }
        } else if !literal && argument == "--json" && command == "search" {
            search_json = true;
        } else if !literal && argument == "--json=compact" && command == "inspect" {
            compact = true;
        } else if !literal && argument == "--verbose" && command == "inspect" {
            verbose = true;
        } else if !literal && argument == "--limit" && command == "search" {
            let raw = take_option_value(arguments, &mut index, "--limit", "a result limit")?;
            search_limit = raw
                .parse()
                .context("--limit must be an integer from 1 to 100")?;
            if !(1..=100).contains(&search_limit) {
                bail!("--limit must be an integer from 1 to 100");
            }
        } else if !literal && argument.starts_with('-') {
            bail!("unknown or unsupported applets option `{argument}`");
        } else if command == "inspect" && name.is_none() {
            name = Some(argument.as_str());
        } else if command == "search" {
            search_words.push(argument.as_str());
        } else {
            bail!("unexpected applets argument {argument:?}\n{HELP}");
        }
        index += 1;
    }
    if command == "inspect" && name.is_none() {
        bail!("applets inspect requires a name or phrase\n{HELP}");
    }
    if command == "search" && search_words.is_empty() {
        bail!("applets search requires a query\n{HELP}");
    }
    if command != "list" && list_mode.is_some() {
        bail!("--names and --signatures apply only to applets list");
    }
    #[cfg(feature = "tooling")]
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
    if manifest_dir.is_none()
        && manifest_catalog.is_none()
        && !default_manifest_candidates()
            .iter()
            .any(|path| path.is_dir())
    {
        bail!("no installed manifest directory found; pass --manifest-dir or --manifest-catalog");
    }
    // Listing and matching read every manifest, so compare the whole directory first.
    let registry = load_registry(manifest_dir, manifest_catalog, ManifestCheck::Full)?;
    #[cfg(feature = "tooling")]
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
    if command == "search" {
        let query = search_words.join(" ");
        let mut manifests = registry.manifests()?;
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        let mut ranked = manifests
            .into_iter()
            .map(|manifest| {
                let contract = manifest.contract();
                let score = applet_search_score(&query, &contract);
                (contract, score)
            })
            .filter(|(_, score)| *score > 0.0)
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right.1.total_cmp(&left.1).then_with(|| {
                left.0["name"]
                    .as_str()
                    .unwrap_or_default()
                    .cmp(right.0["name"].as_str().unwrap_or_default())
            })
        });
        ranked.truncate(search_limit);
        if search_json {
            return write_json(&serde_json::json!({
                "protocol": "execute.applet-search/1",
                "query": query,
                "results": ranked.iter().map(|(contract, score)| serde_json::json!({
                    "name": contract["name"],
                    "score": (score * 100.0).round() / 100.0,
                    "signature": applet_signature_with_description(contract),
                    "description": contract["description"]
                })).collect::<Vec<_>>()
            }));
        }
        for (contract, score) in ranked {
            let line = format!(
                "{score:.2} {}",
                applet_signature_with_description(&contract)
            );
            writeln!(io::stdout(), "{}", escape_applet_text_line(&line))?;
        }
        return Ok(());
    }
    if let Some(name) = name {
        let manifest = match registry.discover(name) {
            Ok(manifest) => manifest,
            Err(error) => {
                let mut candidates = registry
                    .manifests()?
                    .into_iter()
                    .map(|manifest| {
                        let contract = manifest.contract();
                        (manifest.name.clone(), applet_search_score(name, &contract))
                    })
                    .collect::<Vec<_>>();
                candidates.sort_by(|left, right| {
                    right
                        .1
                        .total_cmp(&left.1)
                        .then_with(|| left.0.cmp(&right.0))
                });
                let suggestions = candidates
                    .into_iter()
                    .filter(|(_, score)| *score > 0.0)
                    .take(3)
                    .map(|(name, _)| escape_applet_text_line(&name))
                    .collect::<Vec<_>>();
                if suggestions.is_empty() {
                    return Err(error);
                }
                bail!(
                    "unknown applet {name:?}; did you mean: {}?",
                    suggestions.join(", ")
                );
            }
        };
        let mut contract = manifest.contract();
        if compact && !verbose && contract["execution"]["contract_backend"] == "in_process" {
            contract
                .as_object_mut()
                .expect("contract is an object")
                .remove("argv_template");
        }
        write_json(&serde_json::json!({
            "protocol": "execute.applet/1", "validation": "manifest_static",
            "applet": contract
        }))
    } else {
        let mut manifests = registry.manifests()?;
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        if let Some(mode) = list_mode {
            for manifest in manifests {
                let contract = manifest.contract();
                if mode == "names" {
                    writeln!(io::stdout(), "{}", escape_applet_text_line(&manifest.name))?;
                } else {
                    writeln!(
                        io::stdout(),
                        "{}",
                        escape_applet_text_line(&applet_signature(&contract))
                    )?;
                }
            }
            return Ok(());
        }
        write_json(&serde_json::json!({
            "protocol": "execute.applets/1", "validation": "manifest_static",
            "applets": manifests.iter().map(|manifest| manifest.contract()).collect::<Vec<_>>()
        }))
    }
}

fn applet_signature(contract: &serde_json::Value) -> String {
    const DESCRIPTION_BUDGET_BYTES: usize = 30;
    let mut line = applet_signature_core(contract);
    if let Some(description) = contract["description"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        let escaped = escape_applet_text_line(description);
        let summary = compact_applet_description(&escaped, DESCRIPTION_BUDGET_BYTES);
        if !summary.is_empty() {
            line.push_str("  ");
            line.push_str(&summary);
        }
    }
    line
}

fn escape_applet_text_line(text: &str) -> String {
    flow::terminal_text::escape_controls(text)
        .replace('\n', r"\n")
        .replace('\t', r"\t")
        .replace('\u{2028}', r"\u{2028}")
        .replace('\u{2029}', r"\u{2029}")
}

fn applet_signature_with_description(contract: &serde_json::Value) -> String {
    let mut line = applet_signature_core(contract);
    if let Some(description) = contract["description"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        line.push_str("  ");
        line.push_str(description);
    }
    line
}

fn compact_applet_description(description: &str, max_bytes: usize) -> String {
    const ELLIPSIS: &str = "…";
    let words = description.split_whitespace().collect::<Vec<_>>();
    let mut summary = String::new();
    let mut consumed = 0;
    for (index, word) in words.iter().enumerate() {
        let separator = usize::from(!summary.is_empty());
        let reserve_ellipsis = if index + 1 < words.len() {
            ELLIPSIS.len()
        } else {
            0
        };
        if summary.len() + separator + word.len() + reserve_ellipsis > max_bytes {
            break;
        }
        if separator != 0 {
            summary.push(' ');
        }
        summary.push_str(word);
        consumed = index + 1;
    }
    if consumed < words.len() && !summary.is_empty() {
        summary.push_str(ELLIPSIS);
    }
    summary
}

fn applet_signature_core(contract: &serde_json::Value) -> String {
    let name = contract["name"].as_str().unwrap_or("<unknown>");
    let properties = contract["parameters_schema"]["properties"].as_object();
    let required = contract["parameters_schema"]["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let parameters = properties
        .into_iter()
        .flat_map(|properties| properties.iter())
        .map(|(parameter, schema)| {
            let mut ty = match schema["type"].as_str().unwrap_or("value") {
                "string" => "text".to_owned(),
                "integer" => "integer".to_owned(),
                "number" => "number".to_owned(),
                "boolean" => "boolean".to_owned(),
                other => other.to_owned(),
            };
            if ty == "array" {
                ty = format!(
                    "list<{}>",
                    schema["items"]["type"].as_str().unwrap_or("value")
                );
            } else if ty == "object" {
                ty = "record".to_owned();
            }
            if !required.contains(parameter.as_str()) {
                format!("{parameter}?: {ty}")
            } else {
                format!("{parameter}: {ty}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let output_schema = &contract["io"]["output_schema"];
    let output = match output_schema["type"].as_str().unwrap_or("bytes") {
        "string" => "text",
        "array" => "list",
        "object" => "record",
        output => output,
    };
    format!("{name}({parameters}) -> {output}")
}

fn applet_search_score(query: &str, contract: &serde_json::Value) -> f64 {
    let query_tokens = applet_search_tokens(query);
    if query_tokens.is_empty() {
        return 0.0;
    }
    let fields = vec![
        (
            contract["name"].as_str().unwrap_or_default().to_owned(),
            4.0,
        ),
        (
            contract["phrases"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            3.2,
        ),
        (
            contract["description"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            2.0,
        ),
        (
            contract["parameters_schema"]["properties"]
                .as_object()
                .map(|properties| properties.keys().cloned().collect::<Vec<_>>().join(" "))
                .unwrap_or_default(),
            2.4,
        ),
    ];
    let name = contract["name"].as_str().unwrap_or_default();
    let fields = fields
        .into_iter()
        .map(|(field, weight)| {
            let synonyms = if name == "fs.glob" {
                "file files folder folders directory directories path paths glob list"
            } else if name == "list.sort" {
                "sort sorting order ordered list array records"
            } else if name == "hash.sha256" {
                "sha256 sha 256 checksum digest hash"
            } else if name == "fs.read" {
                "file files read reading content contents"
            } else if name == "fs.write_atomic" {
                "file files write writing atomic"
            } else if name == "fs.ensure_dir" {
                "directory directories folder folders create creation"
            } else if name == "fs.test" {
                "file files path paths check exists existence"
            } else if name == "json.stringify" {
                "format formatted serialize serialization"
            } else {
                ""
            };
            (format!("{field} {synonyms}"), weight)
        })
        .collect::<Vec<_>>();
    let tokens = fields
        .iter()
        .map(|(text, weight)| (applet_search_tokens(text), *weight))
        .collect::<Vec<_>>();
    let mut total = 0.0;
    for query_token in &query_tokens {
        let best = tokens
            .iter()
            .map(|(field_tokens, weight)| {
                let similarity = field_tokens
                    .iter()
                    .map(|candidate| token_similarity(query_token, candidate))
                    .fold(0.0, f64::max);
                similarity * weight
            })
            .fold(0.0, f64::max);
        total += best;
    }
    let coverage = total / (query_tokens.len() as f64 * 4.0);
    let exact_phrase = contract["phrases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .any(|phrase| normalize_search_text(phrase) == normalize_search_text(query));
    let exact_name = contract["name"]
        .as_str()
        .is_some_and(|name| normalize_search_text(name) == normalize_search_text(query));
    (coverage * 100.0)
        + if exact_name {
            100.0
        } else if exact_phrase {
            70.0
        } else {
            0.0
        }
}

fn applet_search_tokens(text: &str) -> Vec<String> {
    normalize_search_text(text)
        .split_whitespace()
        .filter(|token| {
            !matches!(
                *token,
                "a" | "an"
                    | "and"
                    | "by"
                    | "for"
                    | "from"
                    | "in"
                    | "of"
                    | "on"
                    | "or"
                    | "the"
                    | "to"
                    | "with"
            )
        })
        .map(|token| {
            if token.len() > 4 && token.ends_with("ies") {
                format!("{}y", &token[..token.len() - 3])
            } else if token.len() > 3 && token.ends_with('s') {
                token[..token.len() - 1].to_owned()
            } else {
                token.to_owned()
            }
        })
        .collect()
}

fn normalize_search_text(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn token_similarity(left: &str, right: &str) -> f64 {
    if left == right {
        return 1.0;
    }
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    let mut row = (0..=right.len()).collect::<Vec<_>>();
    for (i, left_char) in left.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let old = row[j + 1];
            row[j + 1] = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(previous + usize::from(left_char != right_char));
            previous = old;
        }
    }
    let distance = row[right.len()];
    let longest = left.len().max(right.len());
    if (distance > 1 && longest <= 5) || distance > 2 {
        0.0
    } else {
        1.0 - distance as f64 / longest as f64
    }
}

/// `execute compile --to sh FILE [--out PATH]`: write a checked POSIX sh script.
/// Without `--out` the script goes to standard output; with it, only that path is written.
#[cfg(feature = "tooling")]
fn compile_cli(arguments: &[String]) -> Result<()> {
    const HELP: &str = "Usage: execute compile --to sh FILE [--out PATH] [--manifest-dir DIR | --manifest-catalog FILE]\n\nCompile a program to a POSIX sh script. Statements with no faithful sh equivalent are refused by name and line; nothing is approximated. Output goes to standard output unless --out names a file.";
    if arguments == ["--help"] || arguments == ["-h"] {
        writeln!(io::stdout(), "{HELP}")?;
        return Ok(());
    }
    let mut target = false;
    let mut output = None;
    let mut file = None;
    let mut manifest_dir = None;
    let mut manifest_catalog = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--to" => {
                if target {
                    bail!("--to may be specified only once");
                }
                let value = take_option_value(arguments, &mut index, "--to", "a target")?;
                if value != "sh" {
                    bail!("compile supports only --to sh\n{HELP}");
                }
                target = true;
                index += 1;
            }
            "--out" => {
                if output.is_some() {
                    bail!("--out may be specified only once");
                }
                output = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--out",
                    "an output path",
                )?));
                index += 1;
            }
            "--manifest-dir" => {
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
                    "a manifest directory",
                )?));
                index += 1;
            }
            "--manifest-catalog" => {
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
                    "a manifest catalog",
                )?));
                index += 1;
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
    if !target {
        bail!("compile needs --to sh\n{HELP}");
    }
    let file = file.context(HELP)?;
    let registry = load_registry(manifest_dir, manifest_catalog, ManifestCheck::Lazy)?;
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
        Some(path) => {
            let source_path = fs::canonicalize(&file)
                .with_context(|| format!("resolving source program {}", file.display()))?;
            let destination = if path.exists() {
                fs::canonicalize(&path)
                    .with_context(|| format!("resolving output path {}", path.display()))?
            } else {
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                let filename = path.file_name().context("output path must name a file")?;
                fs::canonicalize(parent)
                    .with_context(|| format!("resolving output directory {}", parent.display()))?
                    .join(filename)
            };
            if source_path == destination {
                bail!("--out must not name the source program");
            }
            write_compiled_script_atomically(&path, script.as_bytes())?;
        }
        None => io::stdout().write_all(script.as_bytes())?,
    }
    Ok(())
}

#[cfg(feature = "tooling")]
fn write_compiled_script_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::ErrorKind;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    path.file_name().context("output path must name a file")?;
    let mut temporary_path = None;
    let mut temporary_file = None;
    for attempt in 0..100u32 {
        let candidate = parent.join(format!(
            ".execute-compile-{}.{}.tmp",
            std::process::id(),
            attempt
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary_path = Some(candidate);
                temporary_file = Some(file);
                break;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("creating temporary output beside {}", path.display())
                });
            }
        }
    }
    let temporary_path =
        temporary_path.context("could not allocate a unique temporary output file")?;
    let write_result = (|| -> Result<()> {
        let mut file = temporary_file.context("temporary output file was not created")?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary_path, path)
            .with_context(|| format!("atomically replacing compiled script {}", path.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    write_result
}

#[cfg(feature = "module-tools")]
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
        .or_else(|| {
            manifest
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .or_else(|| Some(PathBuf::from(".")))
        })
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

#[cfg(feature = "tooling")]
fn service_cli(arguments: &[String]) -> Result<()> {
    const HELP: &str = "Usage: execute service generate <systemd|systemd-timer|launchd> FILE [--name ID] [--persistent] [--manifest-dir DIR | --manifest-catalog FILE] [--working-dir DIR] [--capability-policy FILE] [--data-flow-policy FILE] [--clear-env | --env-allow NAME[,NAME...]] [--input NAME=VALUE ... | --answers FILE] [-- ARG...]\n\nThe manifest selectors are mutually exclusive. --manifest-dir keeps its directory or single-manifest-file behavior; --manifest-catalog explicitly selects a packed catalog. No catalog is merged or selected automatically. --answers requires a private JSON file and forwards only its path. Print a validated per-user service definition. This command never writes, installs, enables, or starts a service.";
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
    let mut answers_file = None;
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
            "--answers" => {
                if answers_file.is_some() {
                    bail!("--answers may be specified once");
                }
                answers_file = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--answers",
                    "a JSON answers file",
                )?));
            }
            value if value.starts_with('-') => bail!("unsupported service option {value}"),
            _ => positional.push(workflow_path(argument)?),
        }
        index += 1;
    }
    if positional.len() != 2 {
        bail!("service generate requires a format and one workflow file\n{HELP}");
    }
    if answers_file.is_some() && !supplied_inputs.is_empty() {
        bail!("--answers cannot be combined with --input in service generation");
    }
    if let Some(path) = answers_file.as_deref() {
        let path = flow::service_deployment::validate_private_answers_file(path)?;
        supplied_inputs = flow::prompt::answers_from_file(&path)?;
        answers_file = Some(path);
    }
    let capability_policy_path = capability_policy.clone();
    let data_flow_policy_path = data_flow_policy.clone();
    let child_environment_allowlist = env::var_os("EXECUTE_CHILD_ENV_ALLOW")
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow!("EXECUTE_CHILD_ENV_ALLOW must be UTF-8"))
        })
        .transpose()?;
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
        ManifestCheck::Lazy,
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
        child_environment_allowlist: child_environment_allowlist.as_deref(),
        answers_file: answers_file.as_deref(),
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
    check: ManifestCheck,
) -> Result<Registry> {
    if explicit_path.is_some() && explicit_catalog.is_some() {
        bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
    }
    match (explicit_path, explicit_catalog) {
        (Some(path), None) => {
            manifest_cache::load(manifest_cache::Source::Directory(cache_key(&path)), || {
                Registry::load(&path)
                    .with_context(|| format!("could not load manifests from {}", path.display()))
            })
        }
        (None, Some(path)) => {
            manifest_cache::load(manifest_cache::Source::Catalog(cache_key(&path)), || {
                Registry::load_catalog(&path).with_context(|| {
                    format!("could not load packed manifest catalog {}", path.display())
                })
            })
        }
        (None, None) => {
            match default_manifest_candidates()
                .into_iter()
                .find(|path| path.is_dir())
            {
                Some(path) => Registry::load_default_directory_only(&path, check),
                None => Ok(Registry::default()),
            }
        }
        (Some(_), Some(_)) => unreachable!("selector conflict checked above"),
    }
}

/// The key a manifest source is cached under: its canonical path, or the path itself when it
/// cannot be canonicalized, in which case the load reports the error.
fn cache_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Runs in the daemon parent before a forwarded run is forked. Loads the run's own manifest
/// directory or catalog into the cache, so the forked run inherits the parsed registry. A
/// failure is left for the run to report.
fn warm_forwarded_manifests(arguments: &[String]) {
    let Ok(Some(args)) = parse_args(arguments) else {
        return;
    };
    let _ = match (args.manifest_dir, args.manifest_catalog) {
        (Some(path), None) => {
            manifest_cache::warm(manifest_cache::Source::Directory(cache_key(&path)), || {
                Registry::load(&path)
                    .with_context(|| format!("could not load manifests from {}", path.display()))
            })
        }
        (None, Some(path)) => {
            manifest_cache::warm(manifest_cache::Source::Catalog(cache_key(&path)), || {
                Registry::load_catalog(&path).with_context(|| {
                    format!("could not load packed manifest catalog {}", path.display())
                })
            })
        }
        _ => Ok(()),
    };
}

fn load_capability_policy(path: &Path) -> Result<CapabilityPolicy> {
    CapabilityPolicy::load(path)
        .with_context(|| format!("loading capability policy {}", path.display()))
}

fn load_data_flow_policy(path: &Path) -> Result<flow::security::DataFlowPolicy> {
    flow::security::DataFlowPolicy::load(path)
        .map_err(|error| anyhow!("loading data-flow policy {}: {error}", path.display()))
}

fn load_workflow_registry(
    explicit_path: Option<PathBuf>,
    explicit_catalog: Option<PathBuf>,
    check: ManifestCheck,
    capability_policy: Option<&CapabilityPolicy>,
    data_flow_policy: Option<&flow::security::DataFlowPolicy>,
) -> Result<Registry> {
    let registry = load_registry(explicit_path, explicit_catalog, check)?;
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

#[cfg(feature = "live")]
fn default_manifest_dir() -> Result<PathBuf> {
    let candidates = default_manifest_candidates();
    candidates
        .iter()
        .find(|path| path.is_dir())
        .cloned()
        .or_else(|| candidates.into_iter().next())
        .context("cannot determine installed manifest directories; pass --manifest-dir")
}

/// The installed manifest directories, followed by the source tree only in debug builds.
fn default_manifest_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(3);
    if let Ok(executable) = env::current_exe() {
        #[cfg(target_os = "macos")]
        let executable = fs::canonicalize(&executable).unwrap_or(executable);
        if let Some((sibling, system)) = manifest_candidates(&executable) {
            candidates.push(sibling);
            candidates.push(system);
        }
    }
    #[cfg(debug_assertions)]
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"));
    candidates
}

fn manifest_candidates(executable: &Path) -> Option<(PathBuf, PathBuf)> {
    let install_root = executable.parent()?.parent()?;
    Some((
        install_root.join("manifests"),
        install_root.join("share").join("execute").join("manifests"),
    ))
}

pub(crate) fn usage() -> &'static str {
    "Usage: execute FILE_OR_FILE_URI [options] [-- ARG...]\n       execute <check|explain|inspect|run> FILE [options]\n       execute check --explain CODE\n       execute resume FILE --checkpoint DIR [options]\n       execute test | run-parts | show-run | replay | repl | repair | compile | checkpoint | help | attach | dap | daemon ...\n       execute applets <list|search|inspect|discover|match> [options]\n       execute modules <inspect|verify|lock> MANIFEST [options]\n       execute service generate <systemd|launchd> FILE [options]\n       execute lsp [--stdio]\n       execute live <serve|read|create|update|delete|patch|control|send|event|wait|schema> ...\n       execute model <classify|judge|score|generate|extract> [options]\n       execute --help, -h\n\nOptions (values are separate arguments):\n  --clear-env             Pass no ambient environment to native children\n  --env-allow NAMES       Pass only these child variables (comma-separated; repeatable)\n  --json                  Structured result for check or explain\n  --explain CODE          Explain a registered check diagnostic code\n  --resolve               Resolve quoted executables on PATH during check\n  --deny-warnings         Treat check warnings as errors\n  --explain-version 1|2   Select explain protocol (default 2); applies to reviews and confirmations\n  --input NAME=VALUE      Supply a declared program input (repeat for different names)\n  -- ARG...               Literal program arguments for declared positional inputs\n  --manifest-dir DIR      Applet manifest directory or legacy JSON file\n  --manifest-catalog FILE Packed manifest catalog (exclusive with --manifest-dir)\n  --execution-mode MODE   trusted or untrusted (default with policy)\n  --capability-policy FILE Strict applet capability policy\n  --data-flow-policy FILE Origin and persistence grants\n  --cache-dir DIR         Cache directory (run/resume)\n  --checkpoint DIR        Durable checkpoint directory (run/resume)\n  --retry-uncertain       Retry an invocation with an uncertain outcome\n  --answers FILE          Test answers\n  --record FILE           Record a run\n  --what-if               Print plan and fingerprint without running\n  --confirm FINGERPRINT   Run only when the reviewed plan matches\n  --allow CAPABILITY      Grant a capability\n  --policy-header         Include the capability policy header in explain output\n  --format FORMAT         Output format: github or json\n  --single-instance FILE  Prevent concurrent runs with a lock\n  --control               Enable the live control socket for a run\n  --break-at LINE         Pause at a source line\n  --daemon                Forward run through daemon\n  --json-errors           Emit structured errors on stderr (before --)\n  --version, -V           Print version\n  --headless              Run a UI once and print JSON state\n  --once                  Run one calendar activation and exit\n  --init                  Supervise worker as PID 1 (Unix)\n  --trace                 Print execution details to stderr\n  --max-output-bytes N    Maximum captured output bytes\n  --max-loop-iterations N  Maximum iterations per loop (default 1000000)\n  --max-runs N            Maximum trigger runs or interactive UI jobs\n  --missed-ticks POLICY   Trigger backlog policy\n  --max-catch-up N        Bound catch-up runs to 1..64\n  --calendar-dedup MODE   Calendar fold policy\n  --command-timeout-ms N  Maximum native pipeline lifetime\n  --total-timeout-ms N    Maximum workflow lifetime (run/resume)\n  --drain-timeout-ms N    Interrupt drain budget for run\n  --max-record-bytes N    Maximum listener/stream record size"
}

fn write_json(value: &serde_json::Value) -> Result<()> {
    writeln!(io::stdout(), "{}", serde_json::to_string_pretty(value)?)?;
    Ok(())
}

#[cfg(feature = "tooling")]
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
    writeln!(io::stdout(), "{}", usage())?;
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

#[cfg(test)]
mod startup_path_tests {
    use super::{
        default_manifest_candidates, manifest_candidates, parse_os_arguments, redact_runtime_error,
        RedactedRuntimeDiagnostic,
    };
    use std::path::{Component, Path};

    #[test]
    fn executable_relative_manifest_candidates_are_lexically_normalized() {
        let (sibling, system) = manifest_candidates(Path::new("/opt/execute/bin/execute"))
            .expect("executable path has an install root");
        assert_eq!(sibling, Path::new("/opt/execute/manifests"));
        assert_eq!(system, Path::new("/opt/execute/share/execute/manifests"));
        assert!(!sibling
            .components()
            .any(|part| part == Component::ParentDir));
        assert!(!system.components().any(|part| part == Component::ParentDir));
    }

    #[test]
    fn source_manifest_fallback_is_only_available_in_debug_builds() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let candidates = default_manifest_candidates();
        assert_eq!(candidates.contains(&source), cfg!(debug_assertions));
    }

    #[test]
    fn argument_conversion_keeps_json_errors_after_separator_literal() {
        let arguments = ["run", "workflow.program", "--", "--json-errors"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect();
        assert_eq!(
            parse_os_arguments(arguments).expect("UTF-8 arguments"),
            ["run", "workflow.program", "--", "--json-errors"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn argument_conversion_keeps_the_non_utf8_error_wording() {
        use std::os::unix::ffi::OsStringExt;

        let error = parse_os_arguments(vec![std::ffi::OsString::from_vec(vec![0xff])])
            .expect_err("non-UTF-8 arguments are refused");
        assert_eq!(
            error.to_string(),
            "argument 1 is not valid UTF-8; use a percent-encoded file:// URI for non-UTF-8 workflow paths"
        );
    }

    #[test]
    fn redacted_runtime_errors_keep_broken_pipe_and_safe_message() {
        let registry = flow::manifest::Registry::default();
        flow::security::secret_redactor(&registry)
            .register(
                b"PIPE-SECRET-CANARY",
                &std::sync::atomic::AtomicBool::new(false),
                None,
            )
            .unwrap();
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "PIPE-SECRET-CANARY",
        ));
        let wrapped = redact_runtime_error(&registry, error);
        let diagnostic = wrapped.downcast_ref::<RedactedRuntimeDiagnostic>().unwrap();
        assert!(diagnostic.closed_stdout);
        assert!(diagnostic.message.contains("[REDACTED]"));
        assert!(!diagnostic.message.contains("PIPE-SECRET-CANARY"));
    }
}
