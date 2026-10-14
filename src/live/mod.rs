//! Local, opt-in JSON control plane for real program execution state.
pub(crate) mod state;
#[cfg(windows)]
mod windows_transport;
mod worker;
#[cfg(windows)]
use windows_transport::{Listener as UnixListener, Stream as UnixStream};

use self::state::*;
use crate::manifest::Registry;
use crate::runtime::RunOptions;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
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
    match dispatch(arguments, default_manifests, path) {
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
fn error_response(error: &anyhow::Error) -> Json {
    let code = error
        .downcast_ref::<ApiError>()
        .map_or("invalid_request", |error| error.code);
    json!({"ok":false,"error":{"code":code,"message":format!("{error:#}")}})
}
fn schema() -> Json {
    json!({"ok":true,"protocol":"execute.live/1","execute_version":env!("CARGO_PKG_VERSION"),"commands":{
        "serve":"execute live serve FILE --socket PATH [--paused] [--headless] [--bindings JSON] [--state-file FILE] [--manifest-dir DIR] [--working-dir DIR] [--cache-dir DIR] [--command-timeout-ms N] [--max-output-bytes N]",
        "read":"execute live read [JSON_POINTER] --socket PATH",
        "create":"execute live create JSON_POINTER --json JSON --socket PATH [--if-revision N]",
        "update":"execute live update JSON_POINTER --json JSON --socket PATH [--if-revision N]",
        "delete":"execute live delete JSON_POINTER --socket PATH [--if-revision N]",
        "patch":"execute live patch --json '[{\"op\":\"replace\",\"path\":\"/bindings/name\",\"value\":\"value\"}]' --socket PATH [--if-revision N]",
        "control":"execute live control pause|resume|step|cancel|retry|reset|restart|restart-source|stop --socket PATH [--if-revision N]",
        "event":"execute live event --json '{\"type\":\"press\",\"program\":\"chooser\",\"widget\":\"track\"}' --socket PATH",
        "send":"execute live send --json '{\"text\":\"event\\n\"}' --socket PATH",
        "wait":"execute live wait [JSON_POINTER] --after REVISION --timeout-ms 1000 --socket PATH",
        "request":"execute live request --json JSON --socket PATH"},
        "value_sources":["--json JSON (use - to read stdin)","--json-file FILE","--text-file FILE (JSON string)"],
        "inspection":"execute inspect FILE [--input NAME=VALUE] returns execute.inspect/1 JSON input types/defaults/missing values or TUI component ports without executing applets",
        "declared_inputs":"top-level accept declarations constrain /bindings: required inputs must exist, defaults materialize at startup, typed values cannot be deleted or changed to another type; inspect FILE discovers the contract",
        "state":{"source":".program text, <=64 KiB; replace source and cursor atomically to change the pending workflow","cwd":"existing absolute working directory","bindings":"real named values; string/bool/integer/list map to native parameters; objects/null/floats become JSON text in argv","input":"next pipeline input: {text:UTF8} or {bytes:[0..255]}","cursor":"zero-based next statement; native pipeline groups are indivisible","inbox":"bounded listener input queue; listen to stdin uses this queue in managed mode","user":"arbitrary JSON metadata object","ui":"live component inputs, widget state, focus, and queued UI events for a screen program"},
        "read_only":["/runtime","/events"],"runtime_fields":["status","in_flight","active_node","effects_may_be_incomplete","error","runs_completed","nodes_completed","completed_native_launches","cache_hits","last_output","frame","ui_applied_revision","ui_errors"],
        "semantics":{"revision":"monotonic; if_revision protects atomic writes against concurrent edits and execution","create":"requires absent object key; arrays insert at index or -","update":"requires existing path","delete":"requires existing path; required document fields cannot be removed","patch":"atomic RFC 6902 add/replace/remove/test operations; all changes validate before commit","pause":"finish the current native group, then pause; streams pause at their next emitted record","cancel":"cancel/reap the active native group; retry explicitly acknowledges possibly incomplete effects","source":"in-memory replacement only; source file is not rewritten; changing the native listener source/cwd restarts its producer and may replay input; handler-only edits preserve it","reset":"reset cursor/cwd/input and local result bindings; preserves external bindings and user metadata; does not undo effects","restart":"reset and run; may repeat effects","completed":"the control server remains alive for inspection and edits until stop","stream":"native argv is fixed for the lifetime of a stream; cancel and wait before editing/restarting its pipeline","ui":"actual renderer state under /ui/programs/NAME/widgets/ID; edit text, items/selection, table rows, bars, chart series and focus; component input changes invoke .program handlers; event dispatch supports press/submit/choose/open; source replacement changes layout and widget identities","durability":"ephemeral live state; separate from durable checkpoints; completed effects are never rolled back"},
        "limits":{"request_bytes":MAX_MESSAGE,"state_bytes":MAX_STATE,"inbox_records":MAX_QUEUE,"event_history":64,"concurrent_clients":16,"wait_timeout_ms":10000},
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
                "--working-dir",
                "--cache-dir",
                "--command-timeout-ms",
                "--max-output-bytes",
                "--json",
                "--json-file",
                "--text-file",
                "--if-revision",
                "--after",
                "--timeout-ms",
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
fn dispatch(
    args: &[String],
    default_manifests: PathBuf,
    path: fn(&str) -> Result<PathBuf>,
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
                "--working-dir",
                "--cache-dir",
                "--command-timeout-ms",
                "--max-output-bytes",
            ],
            true,
        )?;
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
        let registry = match opts.flags.get("--manifest-dir") {
            Some(path) => Registry::load(Path::new(path)),
            None => Registry::load_optional(&default_manifests),
        }?;
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
        let plan = validate(&doc, &registry, max_bytes)?;
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
        };
        return serve(&socket, shared, options);
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
            if opts.positional.len() != 1 {
                bail!("control requires one action");
            }
            request["action"] = json!(opts.positional[0]);
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
    let encoded = serde_json::to_vec(&request)?;
    if encoded.len() >= MAX_MESSAGE {
        bail!("request exceeds 1 MiB");
    }
    let mut stream = UnixStream::connect(&socket).context("could not connect to live session")?;
    if !peer_is_owner(&stream) {
        bail!("live session peer does not match the current user");
    }
    stream.set_read_timeout(Some(Duration::from_secs(12)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(&encoded)?;
    stream.write_all(b"\n")?;
    let mut bytes = Vec::new();
    BufReader::new(stream)
        .take((MAX_STATE * 2 + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
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
fn serve(socket: &Path, shared: Arc<Shared>, options: RunOptions) -> Result<Json> {
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
    signals.0 = crate::platform::signals(shared.stop.clone())?;
    writeln!(
        std::io::stdout(),
        "{}",
        json!({"ok":true,"event":"ready","protocol":"execute.live/1","socket":socket,"pid":std::process::id()})
    )?;
    std::io::stdout().flush()?;
    let control = shared.clone();
    let worker = thread::spawn(move || worker::run(control, options));
    let active = Arc::new(AtomicUsize::new(0));
    let mut clients = Vec::new();
    let result = (|| -> Result<()> {
        while !shared.stop.load(Ordering::Relaxed) {
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
    Ok(json!({"ok":true,"event":"stopped"}))
}
fn peer_is_owner(stream: &UnixStream) -> bool {
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
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut bytes = Vec::new();
        BufReader::new(&stream)
            .take((MAX_MESSAGE + 1) as u64)
            .read_until(b'\n', &mut bytes)?;
        if bytes.len() > MAX_MESSAGE {
            bail!("request exceeds 1 MiB");
        }
        let value: Json = serde_json::from_slice(&bytes)?;
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
            .context("request needs op")?;
        let allowed: &[&str] = match op {
            "read" => &["op", "path"],
            "create" | "update" => &["op", "path", "value", "if_revision"],
            "delete" => &["op", "path", "if_revision"],
            "patch" => &["op", "operations", "if_revision"],
            "control" => &["op", "action", "if_revision"],
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
        let _ = stream.write_all(&bytes);
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
