//! Local, opt-in JSON control plane for real program execution state.
pub(crate) mod auth;
mod debug;
pub(crate) mod state;
pub(crate) mod transport;
#[cfg(windows)]
mod windows_transport;
mod worker;
#[cfg(windows)]
use windows_transport::{Listener as UnixListener, Stream as UnixStream};

use self::auth::{AgentAuthPolicy, AgentCredential, RequestAuth};
use self::state::*;
use crate::capabilities::CapabilityPolicy;
use crate::manifest::Registry;
use crate::runtime::RunOptions;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread;
use std::time::Duration;

pub fn cli(
    arguments: &[String],
    default_manifests: PathBuf,
    path: fn(&str) -> Result<PathBuf>,
) -> Result<()> {
    match dispatch(arguments, default_manifests, path, Mode::Serve) {
        Ok(value) => {
            writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::to_string(&value)?
            )?;
            Ok(())
        }
        Err(error) => {
            writeln!(std::io::stdout().lock(), "{}", error_response(&error))?;
            std::process::exit(1);
        }
    }
}
/// The options a finite `execute run --control` passes to the live host.
/// `inputs` are the named `--NAME=VALUE` bindings, as text.
pub struct HostRun {
    pub file: PathBuf,
    pub inputs: BTreeMap<String, String>,
    pub manifest_dir: Option<PathBuf>,
    pub manifest_catalog: Option<PathBuf>,
    pub capability_policy: Option<PathBuf>,
    pub data_flow_policy: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub command_timeout_ms: Option<u64>,
    pub max_output_bytes: usize,
    /// Source lines to pause before from the start (`--break-at`).
    pub breakpoints: Vec<usize>,
}

/// Host a finite program under the live control endpoint for one run. It uses the
/// same setup and serve loop as `live serve`, with the run-mode rules in `serve`.
/// The endpoint is a fresh path that is never reused. It is removed on exit.
/// `max_output_bytes` is capped at the live message limit (1 MiB), which bounds the
/// captured output of each native command.
/// Returns the process status the run ends with, which the caller exits with.
pub fn host_run(
    run: &HostRun,
    default_manifests: PathBuf,
    path: fn(&str) -> Result<PathBuf>,
) -> Result<i32> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let socket = std::env::temp_dir().join(format!(
        "execute-control-{}-{nanos}.sock",
        std::process::id()
    ));
    let mut args = vec![
        "serve".to_owned(),
        run.file.to_string_lossy().into_owned(),
        "--socket".to_owned(),
        socket.to_string_lossy().into_owned(),
        "--max-output-bytes".to_owned(),
        run.max_output_bytes.min(MAX_MESSAGE).to_string(),
    ];
    if let Some(dir) = &run.cache_dir {
        args.push("--cache-dir".to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
    if !run.inputs.is_empty() {
        args.push("--bindings".to_owned());
        args.push(serde_json::to_string(&run.inputs)?);
    }
    if let Some(dir) = &run.manifest_dir {
        args.push("--manifest-dir".to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
    if let Some(catalog) = &run.manifest_catalog {
        args.push("--manifest-catalog".to_owned());
        args.push(catalog.to_string_lossy().into_owned());
    }
    if let Some(policy) = &run.capability_policy {
        args.push("--capability-policy".to_owned());
        args.push(policy.to_string_lossy().into_owned());
    }
    if let Some(policy) = &run.data_flow_policy {
        args.push("--data-flow-policy".to_owned());
        args.push(policy.to_string_lossy().into_owned());
    }
    if let Some(timeout) = run.command_timeout_ms {
        args.push("--command-timeout-ms".to_owned());
        args.push(timeout.to_string());
    }
    if !run.breakpoints.is_empty() {
        let lines: Vec<String> = run.breakpoints.iter().map(usize::to_string).collect();
        args.push("--breakpoints".to_owned());
        args.push(lines.join(","));
    }
    let finished = dispatch(&args, default_manifests, path, Mode::Run)?;
    Ok(finished["exit_status"]
        .as_i64()
        .and_then(|status| i32::try_from(status).ok())
        .unwrap_or(1))
}

fn error_response(error: &anyhow::Error) -> Json {
    let code = error
        .downcast_ref::<ApiError>()
        .map_or("invalid_request", |error| error.code);
    json!({"ok":false,"error":{"code":code,"message":format!("{error:#}")}})
}
fn schema() -> Json {
    json!({"ok":true,"protocol":"execute.live/1","execute_version":env!("CARGO_PKG_VERSION"),"commands":{
        "serve":"execute live serve FILE --socket PATH [--paused] [--breakpoints LINES] [--headless] [--bindings JSON] [--state-file FILE] [--manifest-dir DIR | --manifest-catalog FILE] [--capability-policy FILE] [--data-flow-policy FILE] [--agent-policy FILE] [--working-dir DIR] [--cache-dir DIR] [--command-timeout-ms N] [--max-output-bytes N]",
        "token-create":"execute live token-create --agent ID --token-file FILE (creates a new private file and displays the token once)",
        "read":"execute live read [JSON_POINTER] --socket PATH [--agent ID --token-file FILE]",
        "create":"execute live create JSON_POINTER --json JSON --socket PATH [--if-revision N] [--agent ID --token-file FILE]",
        "update":"execute live update JSON_POINTER --json JSON --socket PATH [--if-revision N] [--agent ID --token-file FILE]",
        "delete":"execute live delete JSON_POINTER --socket PATH [--if-revision N] [--agent ID --token-file FILE]",
        "patch":"execute live patch --json '[{\"op\":\"replace\",\"path\":\"/bindings/name\",\"value\":\"value\"}]' --socket PATH [--if-revision N] [--agent ID --token-file FILE]",
        "control":"execute live control pause|resume|step|cancel|retry|reset|restart|restart-source|stop --socket PATH [--if-revision N] [--agent ID --token-file FILE]",
        "event":"execute live event --json '{\"type\":\"press\",\"program\":\"chooser\",\"widget\":\"track\"}' --socket PATH [--agent ID --token-file FILE]",
        "send":"execute live send --json '{\"text\":\"event\\n\"}' --socket PATH [--agent ID --token-file FILE]",
        "wait":"execute live wait [JSON_POINTER] --after REVISION --timeout-ms 1000 --socket PATH [--agent ID --token-file FILE]",
        "request":"execute live request --json JSON --socket PATH [--agent ID --token-file FILE]; raw JSON may include top-level auth when a server agent policy is enabled"},
        "value_sources":["--json JSON (use - to read stdin)","--json-file FILE","--text-file FILE (JSON string)"],
        "inspection":"execute inspect FILE [--input NAME=VALUE] returns execute.inspect/1 JSON input types/defaults/missing values or TUI component ports without executing applets",
        "declared_inputs":"top-level accept declarations constrain /bindings: required inputs must exist, defaults materialize at startup, typed values cannot be deleted or changed to another type; inspect FILE discovers the contract",
        "state":{"source":".program text, <=64 KiB; replace source and cursor atomically to change the pending workflow","cwd":"existing absolute working directory","bindings":"real named values; string/bool/integer/list map to native parameters; objects/null/floats become JSON text in argv","input":"next pipeline input: {text:UTF8} or {bytes:[0..255]}","cursor":"zero-based next statement; native pipeline groups, conditionals, reusable calls and whole loops are indivisible","inbox":"bounded listener input queue; listen to stdin uses this queue in managed mode","user":"arbitrary JSON metadata object","ui":"live component inputs, widget state, focus, and queued UI events for a screen program"},
        "read_only":["/runtime","/events"],"runtime_fields":["status","in_flight","active_node","effects_may_be_incomplete","error","runs_completed","nodes_completed","completed_native_launches","cache_hits","last_output","frame","ui_applied_revision","ui_errors"],
        "authorization":{"mode":"same_user_trusted_without_policy","opt_in":"serve --agent-policy FILE requires every request to carry an authorized --agent ID --token-file FILE credential; policy stores token hashes; credentials are not persisted or echoed","boundary":"same-UID clients remain inside the existing OS ownership boundary; bearer-token scopes are cooperative client authorization and do not isolate a hostile same-UID process that can read private token files","permissions":["read","edit","control","input","stop"]},
        "capability_policy_scope":"--capability-policy grants applet capabilities only; use --data-flow-policy to enforce remote-origin and persistence data-flow grants",
        "semantics":{"revision":"monotonic; if_revision protects atomic writes against concurrent edits and execution","create":"requires absent object key; arrays insert at index or -","update":"requires existing path","delete":"requires existing path; required document fields cannot be removed","patch":"atomic RFC 6902 add/replace/remove/test operations; all changes validate before commit","breakpoint":"control breakpoint LINE... pauses before those source lines; with no lines it clears them; a paused run shows /runtime/debug (line and bindings)","continue":"while paused at a breakpoint, run to the next breakpoint; otherwise refused","step":"while paused at a breakpoint, run one statement and pause before the next; otherwise run one native node","pause":"finish the current native group, then pause; streams pause at their next emitted record","cancel":"cancel/reap the active native group; retry explicitly acknowledges possibly incomplete effects","source":"in-memory replacement only; source file is not rewritten; changing the native listener source/cwd restarts its producer and may replay input; handler-only edits preserve it","reset":"reset cursor/cwd/input and local result bindings; preserves external bindings and user metadata; does not undo effects","restart":"reset and run; may repeat effects","completed":"the control server remains alive for inspection and edits until stop","stream":"native argv is fixed for the lifetime of a stream; cancel and wait before editing/restarting its pipeline","ui":"actual renderer state under /ui/programs/NAME/widgets/ID; edit text, items/selection, table rows, bars, chart series and focus; component input changes invoke .program handlers; event dispatch supports press/submit/choose/open; source replacement changes layout and widget identities","durability":"ephemeral live state; separate from durable checkpoints; completed effects are never rolled back"},
        "limits":{"request_bytes":MAX_MESSAGE,"state_bytes":MAX_STATE,"inbox_records":MAX_QUEUE,"event_history":64,"concurrent_clients":16,"wait_timeout_ms":10000,"request_read_timeout_ms":2000,"response_write_timeout_ms":2000,"client_response_timeout_ms":12000},
        "supported":["finite","interval","calendar","stdin listener via send/inbox","native applet listener","persistent native stream","interactive TUI","persistent headless TUI"]})
}

struct Options {
    positional: Vec<String>,
    flags: BTreeMap<String, String>,
    paused: bool,
    headless: bool,
}
fn options(args: &[String]) -> Result<Options> {
    let mut result = Options {
        positional: Vec::new(),
        flags: BTreeMap::new(),
        paused: false,
        headless: false,
    };
    let mut index = 0;
    while index < args.len() {
        let value = &args[index];
        if value == "--headless" {
            if result.headless {
                bail!("duplicate --headless");
            }
            result.headless = true;
        } else if value == "--paused" {
            if result.paused {
                bail!("duplicate --paused");
            }
            result.paused = true;
        } else if value.starts_with("--") {
            let allowed = [
                "--socket",
                "--bindings",
                "--state-file",
                "--manifest-dir",
                "--manifest-catalog",
                "--capability-policy",
                "--data-flow-policy",
                "--agent-policy",
                "--working-dir",
                "--cache-dir",
                "--command-timeout-ms",
                "--max-output-bytes",
                "--breakpoints",
                "--json",
                "--json-file",
                "--text-file",
                "--if-revision",
                "--after",
                "--timeout-ms",
                "--agent",
                "--token-file",
            ];
            if !allowed.contains(&value.as_str()) {
                bail!("unknown live option {value}");
            }
            index += 1;
            let argument = args.get(index).context("option requires a value")?;
            if argument.starts_with("--") {
                bail!("option {value} requires a value");
            }
            if result
                .flags
                .insert(value.clone(), argument.clone())
                .is_some()
            {
                bail!("duplicate option {value}");
            }
        } else if value.starts_with('-') {
            bail!("unknown option {value}");
        } else {
            result.positional.push(value.clone());
        }
        index += 1;
    }
    Ok(result)
}
fn limited_read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("file exceeds byte limit");
    }
    Ok(bytes)
}
fn value(opts: &Options) -> Result<Option<Json>> {
    let names = ["--json", "--json-file", "--text-file"];
    if names
        .iter()
        .filter(|name| opts.flags.contains_key(**name))
        .count()
        > 1
    {
        bail!("choose one JSON value source");
    }
    if let Some(text) = opts.flags.get("--json") {
        if text == "-" {
            let mut bytes = Vec::new();
            std::io::stdin()
                .take((MAX_MESSAGE + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_MESSAGE {
                bail!("stdin exceeds request limit");
            }
            return Ok(Some(serde_json::from_slice(&bytes)?));
        }
        return Ok(Some(serde_json::from_str(text)?));
    }
    if let Some(path) = opts.flags.get("--json-file") {
        return Ok(Some(serde_json::from_slice(&limited_read(
            Path::new(path),
            MAX_MESSAGE,
        )?)?));
    }
    if let Some(path) = opts.flags.get("--text-file") {
        return Ok(Some(json!(String::from_utf8(limited_read(
            Path::new(path),
            MAX_MESSAGE
        )?)?)));
    }
    Ok(None)
}
fn numeric(opts: &Options, name: &str) -> Result<Option<u64>> {
    opts.flags
        .get(name)
        .map(|n| {
            n.parse()
                .with_context(|| format!("{name} requires a nonnegative integer"))
        })
        .transpose()
}
fn only(opts: &Options, names: &[&str], allow_paused: bool) -> Result<()> {
    if opts.headless && !allow_paused {
        bail!("--headless applies only to serve");
    }
    if opts.paused && !allow_paused {
        bail!("--paused applies only to serve");
    }
    for flag in opts.flags.keys() {
        if !names.contains(&flag.as_str()) {
            bail!("unsupported option {flag} for this command");
        }
    }
    Ok(())
}
/// How the server ends. `Serve` stays up until stopped. `Run` belongs to a finite
/// `execute run --control`: it shuts down when the program completes or fails, and
/// it fails when a control stop cancels the run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Serve,
    Run,
}

fn dispatch(
    args: &[String],
    default_manifests: PathBuf,
    path: fn(&str) -> Result<PathBuf>,
    mode: Mode,
) -> Result<Json> {
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(schema());
    };
    if matches!(command, "schema" | "--help" | "-h") {
        if args.len() != 1 {
            bail!("schema takes no options");
        }
        return Ok(schema());
    }
    if command == "token-create" {
        let opts = options(&args[1..])?;
        only(&opts, &["--agent", "--token-file"], false)?;
        if !opts.positional.is_empty() {
            bail!("token-create takes no positional arguments");
        }
        let agent = opts
            .flags
            .get("--agent")
            .context("token-create requires --agent ID")?;
        let token_file = opts
            .flags
            .get("--token-file")
            .context("token-create requires --token-file FILE")?;
        let credential = AgentCredential::create_token_file(agent, Path::new(token_file))
            .context("creating live agent token file")?;
        let auth = credential.request_auth();
        return Ok(json!({
            "ok": true,
            "agent": auth.agent,
            "token_file": token_file,
            "token_sha256": credential.token_sha256(),
            "token": auth.token
        }));
    }
    let opts = options(&args[1..])?;
    let socket = PathBuf::from(
        opts.flags
            .get("--socket")
            .context("--socket PATH is required")?,
    );
    if command == "serve" {
        only(
            &opts,
            &[
                "--socket",
                "--bindings",
                "--state-file",
                "--manifest-dir",
                "--manifest-catalog",
                "--capability-policy",
                "--data-flow-policy",
                "--agent-policy",
                "--working-dir",
                "--cache-dir",
                "--command-timeout-ms",
                "--max-output-bytes",
                "--breakpoints",
            ],
            true,
        )?;
        if opts.flags.contains_key("--manifest-dir")
            && opts.flags.contains_key("--manifest-catalog")
        {
            bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
        }
        if opts.positional.len() != 1 {
            bail!("serve requires one .program file");
        }
        let file = path(&opts.positional[0])?;
        let source = String::from_utf8(limited_read(&file, 65536)?)?;
        let cwd = fs::canonicalize(
            opts.flags
                .get("--working-dir")
                .map(PathBuf::from)
                .unwrap_or(std::env::current_dir()?),
        )?;
        let mut doc = Document {
            source,
            cwd: cwd.clone(),
            bindings: BTreeMap::new(),
            input: Data::default(),
            cursor: 0,
            inbox: VecDeque::new(),
            user: Default::default(),
            ui: Json::Null,
        };
        if let Some(file) = opts.flags.get("--state-file") {
            let initial: Json =
                serde_json::from_slice(&limited_read(Path::new(file), MAX_MESSAGE)?)?;
            let object = initial
                .as_object()
                .context("initial state must be an object")?;
            if object.contains_key("source") {
                bail!("initial source comes from the .program file");
            }
            let mut json = serde_json::to_value(&doc)?;
            for (key, value) in object {
                json.as_object_mut()
                    .unwrap()
                    .insert(key.clone(), value.clone());
            }
            doc = serde_json::from_value(json)?;
        }
        if let Some(bindings) = opts.flags.get("--bindings") {
            doc.bindings =
                serde_json::from_str(bindings).context("--bindings requires a JSON object")?;
        }
        let breakpoints = debug::lines_text(opts.flags.get("--breakpoints").map(String::as_str))?;
        let max_bytes = usize::try_from(numeric(&opts, "--max-output-bytes")?.unwrap_or(262144))?;
        if max_bytes == 0 || max_bytes > MAX_MESSAGE {
            bail!("live max-output-bytes must be 1..1048576");
        }
        let timeout = numeric(&opts, "--command-timeout-ms")?.map(Duration::from_millis);
        if timeout
            .is_some_and(|d| d.is_zero() || std::time::Instant::now().checked_add(d).is_none())
        {
            bail!("invalid command timeout");
        }
        let registry = match (
            opts.flags.get("--manifest-dir"),
            opts.flags.get("--manifest-catalog"),
        ) {
            (Some(path), None) => Registry::load(Path::new(path)),
            (None, Some(path)) => Registry::load_catalog(Path::new(path)),
            (None, None) => Registry::load_optional(&default_manifests),
            (Some(_), Some(_)) => unreachable!("selector conflict checked above"),
        }?;
        let registry = if let Some(path) = opts.flags.get("--capability-policy") {
            let bytes = limited_read(Path::new(path), 1024 * 1024)
                .with_context(|| format!("reading capability policy {path}"))?;
            let source =
                String::from_utf8(bytes).context("capability policy must be UTF-8 JSON")?;
            let policy = CapabilityPolicy::from_json(&source)
                .with_context(|| format!("loading capability policy {path}"))?;
            registry.set_host_state(Some(policy));
            registry
        } else {
            registry
        };
        let registry = if let Some(path) = opts.flags.get("--agent-policy") {
            let policy = AgentAuthPolicy::load(Path::new(path))
                .with_context(|| format!("loading agent authorization policy {path}"))?;
            registry.set_host_state(Some(policy));
            registry
        } else {
            registry
        };
        let registry = if let Some(path) = opts.flags.get("--data-flow-policy") {
            let policy = crate::security::DataFlowPolicy::load(Path::new(path))
                .map_err(|error| anyhow::anyhow!("loading data-flow policy {path}: {error}"))?;
            registry.set_host_state(policy);
            registry
        } else {
            registry
        };
        if opts.flags.contains_key("--capability-policy")
            && !opts.flags.contains_key("--data-flow-policy")
        {
            crate::runtime::write_diagnostic(
                b"warning: --capability-policy grants applet capabilities only; without --data-flow-policy, remote egress and persistence are not protected by data-flow grants\n",
            );
        }
        let initial_plan = Plan::parse(&doc.source)?;
        for input in &initial_plan.program.inputs {
            if let Some(default) = &input.default {
                doc.bindings
                    .entry(input.name.clone())
                    .or_insert_with(|| plain(default.clone()));
            }
        }
        if let Some(screen) = &initial_plan.screen {
            if doc.ui.is_null() {
                doc.ui = crate::ui::initial_live_state(screen)?;
            }
        }
        let plan = validate(&doc, &registry, max_bytes, mode == Mode::Run)?;
        if plan.screen.is_some()
            && !opts.headless
            && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
        {
            bail!("live TUI needs a terminal or serve --headless");
        }
        let initial_cwd = doc.cwd.clone();
        let initial_bindings = doc.bindings.clone();
        let initial_input = doc.input.clone();
        let state = State {
            doc,
            plan,
            revision: 0,
            running: !opts.paused,
            in_flight: false,
            status: if opts.paused { "paused" } else { "ready" }.into(),
            uncertain: false,
            error: None,
            active: None,
            step_budget: None,
            cycle: false,
            runs: 0,
            nodes: 0,
            launches: 0,
            cache_hits: 0,
            last_output: Data::default(),
            exit: None,
            debug: debug::Control {
                breakpoints,
                ..Default::default()
            },
            active_cancel: Arc::new(AtomicBool::new(false)),
            producer_cancel: Arc::new(AtomicBool::new(false)),
            producer_done: false,
            producer_error: None,
            producer_epoch: 0,
            events: VecDeque::new(),
            ui_epoch: 0,
            ui_dirty: true,
            ui_frame: String::new(),
            ui_applied_revision: 0,
            ui_errors: Json::Null,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            wake: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
            registry,
            max_bytes,
            initial_cwd,
            initial_bindings,
            initial_input,
            headless: opts.headless,
            run_mode: mode == Mode::Run,
        });
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: shared.initial_cwd.clone(),
            cache_directory: opts
                .flags
                .get("--cache-dir")
                .map(|p| shared.initial_cwd.join(p)),
            trace: false,
            max_output_bytes: max_bytes,
            command_timeout: timeout,
            total_timeout: None,
        };
        return serve(&socket, shared, options, mode);
    }
    only(
        &opts,
        &[
            "--socket",
            "--json",
            "--json-file",
            "--text-file",
            "--if-revision",
            "--after",
            "--timeout-ms",
            "--agent",
            "--token-file",
        ],
        false,
    )?;
    let supplied = value(&opts)?;
    let mut request = json!({"op":command});
    if let Some(revision) = numeric(&opts, "--if-revision")? {
        request["if_revision"] = json!(revision);
    }
    match command {
        "read" => {
            if opts.positional.len() > 1 {
                bail!("read takes at most one path");
            }
            if let Some(p) = opts.positional.first() {
                request["path"] = json!(p);
            }
        }
        "create" | "update" | "delete" => {
            if opts.positional.len() != 1 {
                bail!("mutation requires one JSON pointer");
            }
            request["path"] = json!(opts.positional[0]);
            if command != "delete" {
                request["value"] = supplied.context("create/update require a JSON value")?;
            }
        }
        "patch" => {
            if !opts.positional.is_empty() {
                bail!("patch takes no positional arguments");
            }
            request["operations"] = supplied.context("patch requires a JSON operations array")?;
        }
        "send" | "event" => {
            if !opts.positional.is_empty() {
                bail!("send takes no positional arguments");
            }
            request["value"] = supplied.context("send requires a text/bytes record")?;
        }
        "control" => {
            let action = opts
                .positional
                .first()
                .context("control requires one action")?;
            if action == "breakpoint" {
                // `control breakpoint 4 9` sets breakpoints on lines 4 and 9; none clears them.
                let lines = opts.positional[1..]
                    .iter()
                    .map(|line| {
                        line.parse::<u64>()
                            .ok()
                            .filter(|line| *line >= 1)
                            .with_context(|| {
                                format!("breakpoint line {line:?} must be a positive integer")
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                request["value"] = json!(lines);
            } else if opts.positional.len() != 1 {
                bail!("control requires one action");
            }
            request["action"] = json!(action);
        }
        "wait" => {
            if opts.positional.len() > 1 {
                bail!("wait takes at most one projection path");
            }
            if let Some(path) = opts.positional.first() {
                request["path"] = json!(path);
            }
            request["after"] = json!(numeric(&opts, "--after")?.context("wait requires --after")?);
            request["timeout_ms"] = json!(numeric(&opts, "--timeout-ms")?.unwrap_or(1000));
        }
        "request" => {
            if !opts.positional.is_empty() {
                bail!("request takes no positional arguments");
            }
            request = supplied.context("request requires JSON")?;
        }
        _ => bail!("unknown live command; use execute live schema"),
    }
    // Refuse silently ignored command-specific flags.
    if matches!(command, "read" | "wait" | "request") && opts.flags.contains_key("--if-revision") {
        bail!("--if-revision applies to mutations; include it inside raw request JSON");
    }
    if opts.flags.contains_key("--after") && command != "wait" {
        bail!("--after applies only to wait");
    }
    if opts.flags.contains_key("--timeout-ms") && command != "wait" {
        bail!("--timeout-ms applies only to wait");
    }
    if matches!(command, "read" | "delete" | "control" | "wait")
        && ["--json", "--json-file", "--text-file"]
            .iter()
            .any(|key| opts.flags.contains_key(*key))
    {
        bail!("this command does not take a JSON value");
    }
    let agent = opts.flags.get("--agent");
    let token_file = opts.flags.get("--token-file");
    if agent.is_some() != token_file.is_some() {
        bail!("--agent and --token-file must be supplied together");
    }
    if let Some(agent) = agent {
        let credential = AgentCredential::from_token_file(agent, Path::new(token_file.unwrap()))
            .context("loading live agent credential")?;
        let object = request
            .as_object_mut()
            .context("request must be a JSON object")?;
        if object.contains_key("auth") {
            bail!("do not include auth in JSON when using --agent and --token-file");
        }
        object.insert(
            "auth".into(),
            serde_json::to_value(credential.request_auth())?,
        );
    }
    let mut encoded = serde_json::to_vec(&request)?;
    if encoded.len() >= MAX_MESSAGE {
        bail!("request exceeds 1 MiB");
    }
    let mut stream = UnixStream::connect(&socket).context("could not connect to live session")?;
    if !peer_is_owner(&stream) {
        bail!("live session peer does not match the current user");
    }
    encoded.push(b'\n');
    transport::write_message(&mut stream, &encoded, transport::IO_TIMEOUT)?;
    let bytes = transport::read_message(&mut stream, MAX_STATE * 2, transport::RESPONSE_TIMEOUT)?;
    if bytes.len() > MAX_STATE * 2 {
        bail!("response exceeds byte limit");
    }
    let response: Json = serde_json::from_slice(&bytes)?;
    if response.get("ok") == Some(&json!(false)) {
        writeln!(std::io::stdout().lock(), "{}", response)?;
        std::process::exit(1);
    }
    Ok(response)
}

#[cfg(unix)]
struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
#[cfg(unix)]
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.dev() == self.dev && m.ino() == self.ino)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
struct SignalGuard(Vec<signal_hook::SigId>);
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for id in &self.0 {
            signal_hook::low_level::unregister(*id);
        }
    }
}
fn serve(socket: &Path, shared: Arc<Shared>, options: RunOptions, mode: Mode) -> Result<Json> {
    let listener = UnixListener::bind(socket)
        .context("could not bind socket; an existing path is never replaced")?;
    #[cfg(unix)]
    let _socket_guard = {
        let metadata = fs::symlink_metadata(socket)?;
        let guard = SocketGuard {
            path: socket.to_owned(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
        guard
    };
    listener.set_nonblocking(true)?;
    let mut signals = SignalGuard(Vec::new());
    signals.0 = crate::platform::signals(shared.stop.clone(), None)?;
    match mode {
        Mode::Serve => {
            writeln!(
                std::io::stdout(),
                "{}",
                json!({"ok":true,"event":"ready","protocol":"execute.live/1","socket":socket,"pid":std::process::id()})
            )?;
            std::io::stdout().flush()?;
        }
        // In run mode, stdout is reserved for the program's output.
        Mode::Run => {
            writeln!(
                std::io::stderr(),
                "execute: control endpoint {}",
                socket.display()
            )?;
        }
    }
    let mut run_end: Option<RunEnd> = None;
    let control = shared.clone();
    let worker = thread::spawn(move || worker::run(control, options));
    let active = Arc::new(AtomicUsize::new(0));
    let mut clients = Vec::new();
    let result = (|| -> Result<()> {
        while !shared.stop.load(Ordering::Relaxed) {
            if mode == Mode::Run && run_end.is_none() {
                if let Some(end) = run_outcome(&shared) {
                    run_end = Some(end);
                    shared.stop.store(true, Ordering::Relaxed);
                    break;
                }
            }
            #[cfg(unix)]
            {
                let mut poll = libc::pollfd {
                    fd: listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut poll, 1, 20) };
                if ready < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(std::io::Error::last_os_error().into());
                }
                if ready == 0 {
                    continue;
                }
            }
            #[cfg(windows)]
            thread::sleep(Duration::from_millis(10));
            let (stream, _) = match listener.accept() {
                Ok(stream) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e.into()),
            };
            if active.load(Ordering::Relaxed) >= 16 {
                drop(stream);
                continue;
            }
            active.fetch_add(1, Ordering::Relaxed);
            let count = active.clone();
            let control = shared.clone();
            clients.push(thread::spawn(move || {
                handle(stream, &control);
                count.fetch_sub(1, Ordering::Relaxed);
            }));
            let mut i = 0;
            while i < clients.len() {
                if clients[i].is_finished() {
                    let _ = clients.swap_remove(i).join();
                } else {
                    i += 1;
                }
            }
        }
        Ok(())
    })();
    shared.stop.store(true, Ordering::Relaxed);
    {
        let state = shared.state.lock().unwrap();
        state.active_cancel.store(true, Ordering::Relaxed);
        state.producer_cancel.store(true, Ordering::Relaxed);
    }
    shared.wake.notify_all();
    let joined = worker.join();
    for client in clients {
        let _ = client.join();
    }
    joined.map_err(|_| anyhow::anyhow!("live worker panicked"))?;
    result?;
    if mode == Mode::Serve {
        return Ok(json!({"ok":true,"event":"stopped"}));
    }
    let Some(end) = run_end else {
        bail!("run stopped before it completed; the run was cancelled by control stop or a signal");
    };
    // Uncaptured stdout has already streamed. Write what plain run writes last: the
    // pipeline output of a completed run, then its diagnostic, then the status.
    if let Some(output) = end.output {
        let mut stdout = std::io::stdout().lock();
        match output {
            Data::Text { text } => stdout.write_all(text.as_bytes())?,
            Data::Bytes { bytes } => stdout.write_all(&bytes)?,
        }
        stdout.flush()?;
    }
    if let Some(diagnostic) = &end.diagnostic {
        crate::runtime::write_diagnostic(diagnostic.as_bytes());
    }
    Ok(json!({"ok":true,"event":"run_finished","exit_status":end.status}))
}

/// A finite run's terminal result, read once it reaches a terminal status.
struct RunEnd {
    /// The process status: a program's `exit with`/`fail with` status, a failed
    /// command's status, or 0 for a run that completes without one.
    status: i32,
    /// The pipeline output of a completed run. A failed run has none.
    output: Option<Data>,
    /// The complete stderr text, with its newline: a `fail with` message, or the
    /// error of a failed run.
    diagnostic: Option<String>,
}

/// The outcome of a finite run once it reaches a terminal status.
fn run_outcome(shared: &Shared) -> Option<RunEnd> {
    let state = shared.state.lock().unwrap();
    let record = state.exit.clone();
    match state.status.as_str() {
        "completed" => Some(RunEnd {
            status: record.as_ref().map_or(0, |exit| exit.status),
            output: Some(state.last_output.clone()),
            diagnostic: record
                .and_then(|exit| exit.message)
                .map(|message| format!("{message}\n")),
        }),
        "failed" => {
            let error = state
                .error
                .clone()
                .unwrap_or_else(|| "run failed".to_owned());
            Some(RunEnd {
                status: record.map_or(1, |exit| exit.status),
                output: None,
                diagnostic: Some(format!("execute: {error}\n")),
            })
        }
        _ => None,
    }
}
pub(crate) fn peer_is_owner(stream: &UnixStream) -> bool {
    #[cfg(unix)]
    {
        peer_has_uid(stream, unsafe { libc::geteuid() })
    }
    #[cfg(windows)]
    {
        stream.peer_is_owner()
    }
}
#[cfg(unix)]
fn peer_has_uid(stream: &UnixStream, owner: libc::uid_t) -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut uid = 0;
        let mut gid = 0;
        libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == owner
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut credentials: libc::ucred = std::mem::zeroed();
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        ) == 0
            && credentials.uid == owner
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (stream, owner);
        false
    }
}
fn handle(mut stream: UnixStream, shared: &Shared) {
    if stream.set_nonblocking(false).is_err() || !peer_is_owner(&stream) {
        return;
    }
    let result = (|| -> Result<Json> {
        let bytes = transport::read_message(&mut stream, MAX_MESSAGE, transport::IO_TIMEOUT)?;
        if bytes.len() > MAX_MESSAGE {
            bail!("request exceeds 1 MiB");
        }
        let mut value: Json = serde_json::from_slice(&bytes)?;
        if matches!(
            value.get("op").and_then(Json::as_str),
            Some("create" | "update" | "send" | "event")
        ) && value.get("value").is_none()
        {
            bail!("operation requires value");
        }
        let op = value
            .get("op")
            .and_then(Json::as_str)
            .context("request needs op")?
            .to_owned();
        let action = value
            .get("action")
            .and_then(Json::as_str)
            .map(str::to_owned);
        let auth = value
            .get("auth")
            .cloned()
            .map(serde_json::from_value::<RequestAuth>)
            .transpose()
            .context("auth must contain an agent and token")?;
        let agent_policy = shared.registry.host_state::<Option<AgentAuthPolicy>>();
        if let Some(policy) = agent_policy.as_ref() {
            let required = auth::required_permission(&op, action.as_deref()).ok();
            let authorization = policy.authorize(auth.as_ref(), &op, action.as_deref());
            if let Some(required) = required {
                let (permission, risk) = match required {
                    auth::AgentPermission::Read => ("read", crate::security::PolicyRisk::Low),
                    auth::AgentPermission::Edit => ("edit", crate::security::PolicyRisk::High),
                    auth::AgentPermission::Control => {
                        ("control", crate::security::PolicyRisk::High)
                    }
                    auth::AgentPermission::Input => ("input", crate::security::PolicyRisk::High),
                    auth::AgentPermission::Stop => ("stop", crate::security::PolicyRisk::High),
                };
                let (outcome, reason) = if authorization.is_ok() {
                    (
                        crate::security::PolicyDecisionOutcome::Approved,
                        crate::security::PolicyDecisionReason::PolicyMatched,
                    )
                } else {
                    (
                        crate::security::PolicyDecisionOutcome::Denied,
                        crate::security::PolicyDecisionReason::MissingGrant,
                    )
                };
                let subject = auth
                    .as_ref()
                    .map_or("unknown", |credential| credential.agent.as_str());
                crate::security::record_policy_decision(
                    &shared.registry,
                    subject,
                    &format!("live.{permission}"),
                    None,
                    risk,
                    outcome,
                    reason,
                );
            }
            authorization
                .map_err(|error| anyhow::anyhow!("live request is unauthorized: {error}"))?;
        } else if auth.is_some() {
            bail!("live agent credentials require serve --agent-policy");
        }
        value
            .as_object_mut()
            .context("request must be an object")?
            .remove("auth");
        let allowed: &[&str] = match op.as_str() {
            "read" => &["op", "path"],
            "create" | "update" => &["op", "path", "value", "if_revision"],
            "delete" => &["op", "path", "if_revision"],
            "patch" => &["op", "operations", "if_revision"],
            "control" => &["op", "action", "value", "if_revision"],
            "send" | "event" => &["op", "value", "if_revision"],
            "wait" => &["op", "path", "after", "timeout_ms"],
            _ => bail!("unknown operation"),
        };
        for key in value
            .as_object()
            .context("request must be an object")?
            .keys()
        {
            if !allowed.contains(&key.as_str()) {
                bail!("unexpected field {key} for {op}");
            }
        }
        if value
            .get("if_revision")
            .is_some_and(|revision| revision.as_u64().is_none())
        {
            bail!("if_revision must be a nonnegative integer");
        }
        let request: Request = serde_json::from_value(value)?;
        state::request(shared, request)
    })();
    let response = result.unwrap_or_else(|error| {
        let mut response = error_response(&error);
        if let Ok(state) = shared.state.lock() {
            response["revision"] = json!(state.revision);
        }
        response
    });
    if let Ok(mut bytes) = serde_json::to_vec(&response) {
        bytes.push(b'\n');
        let _ = transport::write_message(&mut stream, &bytes, transport::IO_TIMEOUT);
    }
}

#[cfg(all(test, unix))]
mod peer_tests {
    use super::*;

    #[test]
    fn real_socket_credentials_are_checked_in_both_directions() {
        let (left, right) = UnixStream::pair().unwrap();
        let owner = unsafe { libc::geteuid() };
        for peer in [&left, &right] {
            assert!(peer_has_uid(peer, owner));
            assert!(!peer_has_uid(peer, owner.wrapping_add(1)));
            assert!(peer_is_owner(peer));
        }
    }
}
