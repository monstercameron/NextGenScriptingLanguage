//! Local, opt-in JSON control plane for real program execution state.
pub(crate) mod auth;
mod debug;
pub(crate) mod state;
mod worker;
use crate::ipc::peer::peer_is_owner;
use crate::ipc::transport;
use crate::ipc::{UnixListener, UnixStream};

use self::auth::{AgentAuthPolicy, AgentCredential, RequestAuth};
use self::state::*;
use crate::capabilities::CapabilityPolicy;
use crate::manifest::{ManifestCheck, Registry};
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
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

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
    pub max_loop_iterations: usize,
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
    let socket = control_endpoint_path()?;
    let mut args = vec![
        "serve".to_owned(),
        run.file.to_string_lossy().into_owned(),
        "--socket".to_owned(),
        socket.to_string_lossy().into_owned(),
        "--max-output-bytes".to_owned(),
        run.max_output_bytes.min(MAX_MESSAGE).to_string(),
        "--max-loop-iterations".to_owned(),
        run.max_loop_iterations.to_string(),
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

/// General client slots. One handler thread serves each admitted connection, and
/// a `wait` client may hold its slot for up to 10 s. Priority traffic uses a
/// free general slot first; the reserve below only engages when all of these
/// are held.
const MAX_CLIENTS: usize = 16;
/// Reserve handlers past [`MAX_CLIENTS`] so priority traffic is still served
/// while every general slot holds a long poll. A reserve slot is claimed only
/// after the accept loop has buffered a full priority request and (when a
/// policy is configured) pre-authorized it for that operation, so slow,
/// partial, or unauthenticated senders can never occupy it; anything else
/// fails fast with `busy`.
const PRIORITY_SPARE: usize = 2;
/// Burst-tolerant flood control for non-priority admissions; see
/// [`state::FloodBudget`]. A fresh server admits [`state::FLOOD_BURST`]
/// back-to-back non-priority requests — enough for a legitimate burst such as
/// eight workers driving a thousand CRUD operations — then refills
/// [`state::SUSTAINED_ACCEPTS_PER_SECOND`] admissions per second. Past the
/// burst, a sustained same-user flood sheds with `busy` before costing a
/// thread (Unix) or reaching policy/state work, while the reserve-eligible
/// priority fast path never spends the budget. Slot exhaustion still sheds
/// first: only an admitted general-lane request spends a token.
/// Accept-loop classification window (Unix): the first newline-terminated
/// request line must arrive within these bytes for the reserve lane to consider
/// it. Genuine requests are far smaller; a larger head is general-lane bulk.
const CLASSIFY_CAP: usize = 8192;
/// Bounded pre-admission queue (Unix): accepted connections that have not yet
/// delivered a full request line wait here without a thread and without holding
/// any client slot, so slow or partial same-UID senders cannot starve control.
/// Past this many, the oldest still-incomplete connection is closed to make
/// room. Enforced on Unix; reported in the schema on every platform.
const PENDING_MAX: usize = 64;

#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    crate::platform::unix_ffi::effective_uid()
}

/// The per-user directory that owns `run --control` endpoints. It persists like
/// the daemon's per-user directory; each endpoint inside it is removed on exit.
#[cfg(unix)]
fn control_dir_name() -> String {
    format!("execute-live-{}", euid())
}
/// The per-user directory that owns `run --control` endpoints.
#[cfg(not(unix))]
fn control_dir_name() -> String {
    "execute-live".to_owned()
}

/// A fresh endpoint for one `run --control`: a randomized name inside the
/// private per-user directory, so neither the path nor its parent is a shared
/// foothold. Binding still creates the socket exclusively and never replaces an
/// existing path.
fn control_endpoint_path() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(control_dir_name());
    ensure_private_dir(&dir)?;
    for _ in 0..16 {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).context("randomizing the control socket name")?;
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let socket = dir.join(format!("control-{}-{name}.sock", std::process::id()));
        // Never reuse a planted path: binding creates the socket exclusively, and
        // a link at a candidate name is skipped here rather than followed.
        if fs::symlink_metadata(&socket).is_err() {
            return Ok(socket);
        }
    }
    bail!("could not pick a unique control socket name")
}

/// Create `dir` private to this user when missing, then refuse anything that is
/// not a caller-owned directory with no group/other permission bits.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", dir.display()));
            }
        }
        check_private_dir(dir)
    }
    #[cfg(not(unix))]
    {
        match fs::create_dir(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", dir.display()));
            }
        }
        Ok(())
    }
}

/// Refuse a socket parent that is not a real directory owned by this user and
/// private to it. Links, group/world-readable directories and other users'
/// directories are all refused.
#[cfg(unix)]
fn check_private_dir(dir: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if metadata.uid() != euid() || metadata.mode() & 0o077 != 0 {
        bail!(
            "{} must be owned by you and private (mode 0700)",
            dir.display()
        );
    }
    Ok(())
}

/// Refuse to bind a socket whose parent directory is not private to this user.
/// A shared parent lets another user pre-create the path to make the run fail,
/// so the predictable-name foothold is closed by refusing it outright.
#[cfg(unix)]
fn check_socket_parent(socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .with_context(|| format!("socket path {} has no directory", socket.display()))?;
    // A bare file name binds in the working directory, which gets the same check.
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let metadata = fs::symlink_metadata(parent).with_context(|| {
        format!(
            "refusing to bind {}: its directory {} must be private and owned by you (mode 0700)",
            socket.display(),
            parent.display()
        )
    })?;
    if !metadata.is_dir() {
        bail!(
            "refusing to bind {}: {} is not a directory",
            socket.display(),
            parent.display()
        );
    }
    if metadata.uid() != euid() || metadata.mode() & 0o077 != 0 {
        bail!(
            "refusing to bind {}: {} must be owned by you and private (mode 0700)",
            socket.display(),
            parent.display()
        );
    }
    Ok(())
}

/// Priority traffic that a constrained lane still serves: inspection reads and
/// the stop the operator relies on. Every other request fails fast with `busy`
/// instead of starving behind long polls or a connection flood.
fn is_priority_request(op: &str, action: Option<&str>) -> bool {
    op == "read" || (op == "control" && action == Some("stop"))
}

/// Whether raw request bytes carry priority traffic. Best effort: malformed
/// bytes are not priority; the handler still answers them with the usual
/// `invalid_request` when they are admitted through the general lane.
#[cfg(unix)]
fn is_priority_bytes(line: &[u8]) -> bool {
    let value: Json = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => return false,
    };
    let op = value.get("op").and_then(Json::as_str).unwrap_or_default();
    is_priority_request(op, value.get("action").and_then(Json::as_str))
}

/// Whether raw priority bytes may claim a reserve slot. When no agent policy
/// is configured the socket stays same-user trusted, so every well-formed
/// priority claim without an `auth` field stays reserve-eligible and `stop`
/// availability is unchanged; a no-policy claim carrying `auth` is demoted
/// because the handler rejects any `auth` without a policy. When a policy is
/// configured, only a request already authorized for its priority operation
/// (`read`, or `control stop`) is reserve-eligible: an unauthenticated,
/// under-permissioned, or malformed-`auth` fake-priority frame is demoted to
/// the general lane, where it spends the flood budget and sheds with `busy`
/// instead of occupying a reserve slot. Preclassification also mirrors the
/// handler's pre-state field checks (allowed keys, `if_revision` shape, and
/// request types) so an authorized-but-invalid `read`/`stop` the handler
/// would reject is demoted rather than occupying a reserve slot. This bounds
/// unauthenticated reserve pressure without adding a priority rate limit
/// that could shed a genuine `stop`. Admission still never bypasses
/// authentication: the handler re-authorizes every admitted request before
/// touching state, and this predicate never grants access on its own.
#[cfg(unix)]
fn reserve_eligible(line: &[u8], policy: Option<&AgentAuthPolicy>) -> bool {
    let value: Json = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => return false,
    };
    let op = value.get("op").and_then(Json::as_str).unwrap_or_default();
    let action = value.get("action").and_then(Json::as_str);
    if !is_priority_request(op, action) {
        return false;
    }
    if !priority_shape_valid(&value, op, action) {
        return false;
    }
    let Some(policy) = policy else {
        // Mirrors `handle_request`: without a policy any `auth` field is
        // rejected, so it must not claim the reserve.
        return value.get("auth").is_none();
    };
    // Mirrors the handler's strict `auth` parse: malformed `auth`
    // (missing/extra fields, wrong types) fails the request there, so it is
    // demoted here rather than occupying a reserve slot. A missing `auth`
    // stays `None` so `authorize` fails closed below, exactly as in the
    // handler.
    let auth = match value.get("auth").cloned() {
        None => None,
        Some(auth_value) => match serde_json::from_value::<RequestAuth>(auth_value) {
            Ok(auth) => Some(auth),
            Err(_) => return false,
        },
    };
    policy.authorize(auth.as_ref(), op, action).is_ok()
}

/// Pre-state shape check for priority preclassification. Mirrors the
/// `handle_request` checks that run before live state is touched, for the
/// priority operations only: the per-operation allowed-field list, the
/// `if_revision` nonnegative-integer shape, and the `Request` type shapes
/// (e.g. `path` must be a string). The `auth` field is validated separately
/// by the caller and ignored here, matching the handler removing it before
/// its allowed-field check. State-dependent checks (`if_revision` freshness,
/// path existence, revision conflicts) are intentionally not mirrored: a
/// well-formed `read`/`stop` that later fails on state still needed its
/// reserve slot to reach the handler.
#[cfg(unix)]
fn priority_shape_valid(value: &Json, op: &str, action: Option<&str>) -> bool {
    let allowed: &[&str] = match (op, action) {
        ("read", _) => &["op", "path"],
        ("control", Some("stop")) => &["op", "action", "value", "if_revision"],
        _ => return false,
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    for key in object.keys() {
        if key == "auth" {
            continue;
        }
        if !allowed.contains(&key.as_str()) {
            return false;
        }
    }
    if value
        .get("if_revision")
        .is_some_and(|revision| revision.as_u64().is_none())
    {
        return false;
    }
    // Catch the type errors the handler's `Request` parse would reject
    // (for example a numeric `path`), after the allowed-field check above
    // which is stricter than the `Request` struct itself.
    let mut shaped = value.clone();
    if let Some(object) = shaped.as_object_mut() {
        object.remove("auth");
    }
    serde_json::from_value::<Request>(shaped).is_ok()
}

/// Where one fully-buffered connection goes. Pure so the reserve-exhausted
/// `busy` path stays covered by deterministic unit tests: holding two
/// reserve slots with fast `read`/`stop` handlers is inherently racy, so an
/// integration test cannot pin the third-priority `busy` without sleeps.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Admit {
    General,
    Priority,
    Shed,
}

/// Decide the lane from exact slot counts. Priority claims use a free
/// general slot first; the reserve engages only while all general slots are
/// held. `flood_admits` is the already-computed flood-budget verdict for
/// this connection: callers spend a token only for a general-lane,
/// non-priority admission, never for reserve-eligible priority or for a
/// connection that sheds on full slots.
#[cfg(unix)]
fn decide_lane(
    general_used: usize,
    priority_used: usize,
    reserve_eligible: bool,
    flood_admits: bool,
) -> Admit {
    if reserve_eligible {
        if general_used < MAX_CLIENTS {
            Admit::General
        } else if priority_used < PRIORITY_SPARE {
            Admit::Priority
        } else {
            Admit::Shed
        }
    } else if general_used < MAX_CLIENTS && flood_admits {
        Admit::General
    } else {
        Admit::Shed
    }
}

/// Admission lane for one handler thread. Unix classifies each connection in
/// the accept loop before spawning, so every spawned Unix handler runs
/// [`Gate::Open`]; the gate stays as a backstop. Windows constrains post-accept
/// like before.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    Open,
    PriorityOnly,
}

/// Spend one flood-budget token for a general-lane admission. Failing closed:
/// a poisoned budget sheds instead of admitting. Reserve-eligible priority
/// never calls this; demoted fake-priority frames do, like any other
/// general-lane traffic. Partial connections that never deliver a request
/// never reach it.
fn admit_non_priority(shared: &Shared) -> bool {
    shared
        .flood
        .lock()
        .map(|mut budget| budget.try_admit(Instant::now()))
        .unwrap_or(false)
}

/// An accepted connection that has not delivered a full request line yet. It
/// holds no thread and no client slot: only a small buffer and a deadline, so
/// a slow or partial same-UID sender costs almost nothing while it stalls.
#[cfg(unix)]
struct PendingConn {
    stream: UnixStream,
    buf: Vec<u8>,
    accepted_at: Instant,
}

/// Spawn one admitted handler and release its lane slot when it exits.
#[cfg(unix)]
fn spawn_slot(
    clients: &mut Vec<thread::JoinHandle<()>>,
    shared: &Arc<Shared>,
    slot: Arc<AtomicUsize>,
    stream: UnixStream,
    request: Vec<u8>,
) {
    slot.fetch_add(1, Ordering::Relaxed);
    let control = shared.clone();
    clients.push(thread::spawn(move || {
        handle(stream, &control, Gate::Open, request);
        slot.fetch_sub(1, Ordering::Relaxed);
    }));
}

/// Best-effort `busy` reply for shed load, written inline in the accept loop so
/// shedding never costs a handler thread. The pending streams are already
/// non-blocking, so this loops `write` until the frame is complete or the
/// peer backpressures (`WouldBlock`), then drops the connection either way:
/// a flowing peer gets the retryable `busy` frame, while a backpressured or
/// stalled peer costs the accept loop no poll budget and no stall. Never
/// block or poll here: a same-UID peer that connects but never reads must
/// not delay admission of a `stop`. Honest edge: if the peer's buffer fills
/// mid-frame, the connection still closes after a truncated prefix, so the
/// client can observe EOF or a JSON parse failure instead of `busy` under
/// saturation; clients should treat those as retryable exactly like `busy`.
#[cfg(unix)]
fn shed_busy(stream: UnixStream) {
    let response = json!({"ok":false,"error":{"code":"busy","message":"the server is saturated; retry, or stop and read remain available"}});
    let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
    bytes.push(b'\n');
    let mut stream = stream;
    // Non-blocking by construction (the caller set non-blocking before
    // buffering). Short frame, so one write usually suffices; loop only
    // while progress is made, never blocking or polling.
    let mut written = 0;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => written += count,
        }
    }
}

/// Take one newline-terminated request out of bytes the accept loop already
/// buffered, or read the tail under the same absolute budget the direct path
/// always had. The protocol is one request per connection, so bytes past the
/// first newline are ignored, exactly like the old read-until-newline path.
fn read_request_bytes(stream: &mut UnixStream, prefix: Vec<u8>) -> Result<Vec<u8>> {
    if let Some(end) = prefix.iter().position(|&b| b == b'\n') {
        return Ok(prefix[..=end].to_vec());
    }
    let mut bytes = prefix;
    let remaining = MAX_MESSAGE.saturating_sub(bytes.len());
    let tail = transport::read_message(stream, remaining, transport::IO_TIMEOUT)?;
    bytes.extend_from_slice(&tail);
    Ok(bytes)
}

fn error_response(error: &anyhow::Error) -> Json {
    let code = error
        .downcast_ref::<ApiError>()
        .map_or("invalid_request", |error| error.code);
    json!({"ok":false,"error":{"code":code,"message":format!("{error:#}")}})
}
/// `concurrent_clients` is a deprecated alias for `general_clients` (both are
/// [`MAX_CLIENTS`]): it stays so strict schema readers written before the
/// reserve/flood keys still validate. New readers should use
/// `general_clients` + `priority_spare`.
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
        "limits":{"request_bytes":MAX_MESSAGE,"state_bytes":MAX_STATE,"inbox_records":MAX_QUEUE,"event_history":64,"concurrent_clients":MAX_CLIENTS,"general_clients":MAX_CLIENTS,"priority_spare":PRIORITY_SPARE,"pending_connections":PENDING_MAX,"priority_classify_bytes":CLASSIFY_CAP,"pending_timeout_ms":transport::IO_TIMEOUT.as_millis(),"burst_accepts":FLOOD_BURST,"sustained_accepts_per_second":SUSTAINED_ACCEPTS_PER_SECOND,"priority_requests":["read","control stop"],"wait_timeout_ms":10000,"request_read_timeout_ms":2000,"response_write_timeout_ms":2000,"client_response_timeout_ms":12000},
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
                "--max-loop-iterations",
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
                "--max-loop-iterations",
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
        // A withheld marker is what a read shows for a secret answer. It is not an answer,
        // so a state file or bindings that carry one cannot start a run.
        if doc.bindings.values().any(is_withheld_marker) {
            bail!("the state carries a withheld secret answer; supply the answer with --bindings");
        }
        let breakpoints = debug::lines_text(opts.flags.get("--breakpoints").map(String::as_str))?;
        let max_bytes = usize::try_from(numeric(&opts, "--max-output-bytes")?.unwrap_or(262144))?;
        if max_bytes == 0 || max_bytes > MAX_MESSAGE {
            bail!("live max-output-bytes must be 1..1048576");
        }
        let max_loop_iterations = usize::try_from(
            numeric(&opts, "--max-loop-iterations")?
                .unwrap_or(crate::runtime::DEFAULT_MAX_LOOP_ITERATIONS as u64),
        )?;
        if max_loop_iterations == 0 {
            bail!("live max-loop-iterations must be positive");
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
            (None, None) if !default_manifests.is_dir() => Ok(Registry::default()),
            (None, None) => {
                Registry::load_default_directory_only(&default_manifests, ManifestCheck::Lazy)
            }
            (Some(_), Some(_)) => unreachable!("selector conflict checked above"),
        }?;
        let registry = if let Some(path) = opts.flags.get("--capability-policy") {
            let policy = CapabilityPolicy::load(Path::new(path))
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
        for input in plan
            .program
            .inputs
            .iter()
            .filter(|input| crate::ast::is_sensitive(input))
        {
            if let Some(value) = doc.bindings.get(&input.name) {
                crate::security::register_sensitive_value(&registry, &state::argument(value))?;
            }
        }
        crate::security::register_sensitive_program(&registry, &plan.program)?;
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
            job_session: Arc::new(crate::runtime::live::JobSession::new()),
            redactor: crate::security::secret_redactor(&registry),
            registry: registry.clone(),
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
            flood: Mutex::new(FloodBudget::new()),
        });
        let options = RunOptions {
            cache_directory: opts
                .flags
                .get("--cache-dir")
                .map(|p| shared.initial_cwd.join(p)),
            max_output_bytes: max_bytes,
            max_loop_iterations,
            command_timeout: timeout,
            ..RunOptions::new(shared.initial_cwd.clone())
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
            .with_context(|| format!("loading live agent credential {}", token_file.unwrap()))?;
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
struct SignalGuard {
    ids: Vec<signal_hook::SigId>,
    // Retained so the registered handlers keep working for the guard's whole
    // lifetime: the Windows backend only holds weak references. Each serve
    // call owns a fresh slot, so one run can neither inherit nor erase
    // another's recorded interrupt.
    _cleanup: Arc<AtomicBool>,
    _signal_code: Arc<AtomicI32>,
}
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for id in &self.ids {
            signal_hook::low_level::unregister(*id);
        }
    }
}
fn serve(socket: &Path, shared: Arc<Shared>, options: RunOptions, mode: Mode) -> Result<Json> {
    #[cfg(unix)]
    check_socket_parent(socket)?;
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
    let cleanup = Arc::new(AtomicBool::new(false));
    let signal_code = Arc::new(AtomicI32::new(0));
    let mut signals = SignalGuard {
        ids: Vec::new(),
        _cleanup: cleanup.clone(),
        _signal_code: signal_code.clone(),
    };
    signals.ids = crate::platform::signals(shared.stop.clone(), None, None, cleanup, signal_code)?;
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
    #[cfg(windows)]
    let active = Arc::new(AtomicUsize::new(0));
    // Unix lane accounting is exact because only this loop admits: general
    // slots serve anything, reserve slots serve fully-read priority requests.
    #[cfg(unix)]
    let general = Arc::new(AtomicUsize::new(0));
    #[cfg(unix)]
    let priority = Arc::new(AtomicUsize::new(0));
    #[cfg(unix)]
    let mut pending: VecDeque<PendingConn> = VecDeque::new();
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
                // `listener` is bound earlier in `serve` and borrowed by this loop, so its fd is
                // open for the wait. The 20 ms timeout is a non-negative c_int.
                let ready = match crate::platform::unix_ffi::poll_one(
                    listener.as_raw_fd(),
                    libc::POLLIN,
                    20,
                ) {
                    Ok((ready, _)) => ready,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                };
                // Only the accept drain depends on listener readiness: it runs
                // here, where `ready` is in scope, while the pending sweep in
                // the block below runs on every tick, including poll timeouts,
                // so bytes that arrive after the accept drain are still picked
                // up promptly.
                if ready != 0 {
                    loop {
                        let (stream, _) = match listener.accept() {
                            Ok(stream) => stream,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) => return Err(e.into()),
                        };
                        // Owner check and non-blocking mode before any buffering: a
                        // foreign peer is dropped without costing a slot or a
                        // flood token, and the sweep below relies on WouldBlock
                        // reads. Accepts are ungated here: the sweep spends a
                        // budget token only once a full non-priority request is
                        // buffered, so partial senders never spend it.
                        if stream.set_nonblocking(true).is_err() || !peer_is_owner(&stream) {
                            continue;
                        }
                        pending.push_back(PendingConn {
                            stream,
                            buf: Vec::new(),
                            accepted_at: Instant::now(),
                        });
                    }
                }
            }
            #[cfg(windows)]
            thread::sleep(Duration::from_millis(10));
            #[cfg(windows)]
            {
                let (stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(e.into()),
                };
                // Past the hard cap the connection is closed without a `busy`
                // frame. This is a deliberate Windows best-effort limit, not
                // parity with Unix: named pipes have no non-blocking shed
                // path, so a reply here would either block this accept loop
                // on the write or spawn an unbounded number of short shed
                // threads under flood. Within the cap, priority and flood
                // sheds still answer `busy` from the handler (see below);
                // a dropped over-cap client observes EOF and should retry.
                // See docs/live-control.md for the Windows contract.
                if active.load(Ordering::Relaxed) >= MAX_CLIENTS + PRIORITY_SPARE {
                    drop(stream);
                    continue;
                }
                // Past the general slots a handler answers only priority
                // traffic (read, stop) so held long polls cannot starve
                // control. The flood budget is checked in the handler after
                // parsing (before policy/state work): Unix already gated
                // pre-admission, and Windows has no pre-admission classifier.
                let constrained = active.fetch_add(1, Ordering::Relaxed) + 1 > MAX_CLIENTS;
                let count = active.clone();
                let control = shared.clone();
                clients.push(thread::spawn(move || {
                    handle(
                        stream,
                        &control,
                        if constrained {
                            Gate::PriorityOnly
                        } else {
                            Gate::Open
                        },
                        Vec::new(),
                    );
                    count.fetch_sub(1, Ordering::Relaxed);
                }));
            }
            #[cfg(unix)]
            {
                // This sweep runs on every tick, including poll timeouts: no
                // thread spawns and no slot is claimed before a full request
                // line is buffered and classified, so slow or partial same-UID
                // senders cannot hold the reserve lane, and bytes that arrive
                // after the accept drain are still picked up promptly.
                // Drain every pending connection until it would block.
                // Connections that delivered a full request line are decided
                // immediately, so bounding the queue below only ever evicts
                // still-incomplete (likely slow-loris) holders, never a ready
                // stop or read. Draining (instead of one chunk per tick) keeps
                // legitimate large requests near the direct-path wire speed;
                // growth past CLASSIFY_CAP is routed as bulk in this same
                // pass, so one fast sender cannot inflate pending memory.
                // Admission below is slot-based first, then burst-tolerant:
                // reserve-eligible priority never spends the flood budget,
                // partial senders never reach it, and a benign burst within
                // the budget is admitted while slots are free. Only a
                // sustained flood past the burst sheds with `busy` — inline,
                // without a thread. The policy is fetched once per sweep; it
                // is immutable after startup, and the per-connection check is
                // a hash comparison, so the accept loop never touches state
                // or policy I/O.
                let agent_policy = shared.registry.host_state::<Option<AgentAuthPolicy>>();
                let policy = agent_policy.as_ref().as_ref();
                let mut index = 0;
                while index < pending.len() {
                    let closed = {
                        let conn = &mut pending[index];
                        let mut closed = false;
                        let mut stalled = false;
                        while !closed
                            && !stalled
                            && conn.buf.len() <= CLASSIFY_CAP.saturating_add(2048)
                        {
                            let mut chunk = [0u8; 2048];
                            match conn.stream.read(&mut chunk) {
                                Ok(0) => closed = true,
                                Ok(count) => {
                                    conn.buf.extend_from_slice(&chunk[..count]);
                                    if conn.buf.contains(&b'\n') {
                                        break;
                                    }
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                    continue;
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                    stalled = true;
                                }
                                Err(_) => closed = true,
                            }
                        }
                        closed
                    };
                    if closed {
                        pending.remove(index);
                        continue;
                    }
                    let line_end = pending[index].buf.iter().position(|&b| b == b'\n');
                    let oversize = line_end.is_none() && pending[index].buf.len() > CLASSIFY_CAP;
                    let expired = line_end.is_none()
                        && !oversize
                        && pending[index].accepted_at.elapsed() > transport::IO_TIMEOUT;
                    if line_end.is_none() && !oversize && !expired {
                        index += 1;
                        continue;
                    }
                    let Some(mut conn) = pending.remove(index) else {
                        continue;
                    };
                    if expired {
                        // A partial older than the request read budget: silent
                        // close. It never held a slot, spawned a thread, or
                        // touched policy state.
                        continue;
                    }
                    let line_end = conn.buf.iter().position(|&b| b == b'\n');
                    // Priority window plus authorization gate: only a
                    // newline-terminated line within CLASSIFY_CAP whose
                    // priority operation is already authorized (or any
                    // well-formed priority claim when no policy is
                    // configured) is considered for the reserve lane. An
                    // oversize-without-newline head joins the general lane,
                    // where the handler applies the full 1 MiB limit to the
                    // remainder. A large-but-valid `read`/`stop` whose first
                    // newline falls past 8 KiB therefore loses reserve
                    // priority but is still served whenever a general slot
                    // is free; document this window, do not widen the
                    // accept-loop buffer to chase it. Demoted
                    // fake-priority frames spend the flood budget like any
                    // other general-lane traffic, so unauthenticated senders
                    // cannot occupy reserve slots however fast they connect.
                    let eligible = line_end.is_some_and(|end| {
                        let line = &conn.buf[..=end];
                        is_priority_bytes(line) && reserve_eligible(line, policy)
                    });
                    let request = match line_end {
                        Some(end) => conn.buf[..=end].to_vec(),
                        None => std::mem::take(&mut conn.buf),
                    };
                    let general_used = general.load(Ordering::Relaxed);
                    let priority_used = priority.load(Ordering::Relaxed);
                    // Spend a flood token only for a general-lane admission
                    // decision on demoted/non-priority traffic; reserve
                    // decisions and full-lane sheds never touch the budget.
                    let admit = if eligible {
                        decide_lane(general_used, priority_used, true, false)
                    } else if general_used < MAX_CLIENTS {
                        decide_lane(
                            general_used,
                            priority_used,
                            false,
                            admit_non_priority(&shared),
                        )
                    } else {
                        Admit::Shed
                    };
                    match admit {
                        Admit::General => {
                            spawn_slot(
                                &mut clients,
                                &shared,
                                general.clone(),
                                conn.stream,
                                request,
                            );
                        }
                        Admit::Priority => {
                            spawn_slot(
                                &mut clients,
                                &shared,
                                priority.clone(),
                                conn.stream,
                                request,
                            );
                        }
                        Admit::Shed => {
                            // Reserve exhausted while every general slot is
                            // held, or the general lane is full/past-burst:
                            // answer `busy` inline (non-blocking writes,
                            // never a thread) so the client can retry
                            // instead of seeing EOF. Only concurrent
                            // authorized priority handlers can hold the
                            // reserve; partial senders never hold a slot
                            // because buffering and authorization precede
                            // admission, and demoted fake-priority frames
                            // shed here without ever reaching the reserve.
                            shed_busy(conn.stream);
                        }
                    }
                }
                // Bound pre-admission memory: the oldest still-incomplete
                // connection is the likeliest slow-loris holder, so it yields
                // to the newcomer.
                while pending.len() > PENDING_MAX {
                    pending.pop_front();
                }
            }
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
                .map(|message| format!("{}\n", state.redactor.redact_text(&message))),
        }),
        "failed" => {
            let error = state
                .error
                .clone()
                .unwrap_or_else(|| "run failed".to_owned());
            Some(RunEnd {
                status: record.map_or(1, |exit| exit.status),
                output: None,
                diagnostic: Some(format!("execute: {}\n", state.redactor.redact_text(&error))),
            })
        }
        _ => None,
    }
}
fn handle(mut stream: UnixStream, shared: &Shared, gate: Gate, prefix: Vec<u8>) {
    if stream.set_nonblocking(false).is_err() || !peer_is_owner(&stream) {
        return;
    }
    let result = (|| -> Result<Json> {
        let bytes = read_request_bytes(&mut stream, prefix)?;
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
            .context("request needs op")?
            .to_owned();
        let action = value
            .get("action")
            .and_then(Json::as_str)
            .map(str::to_owned);
        // A constrained lane (past the general client slots) answers only
        // priority traffic. The shed happens before policy work so held long
        // polls cannot spend server time or starve control. On Unix the
        // accept loop already admits only priority traffic to a constrained
        // lane, so this stays as a backstop.
        if gate == Gate::PriorityOnly && !is_priority_request(&op, action.as_deref()) {
            return Err(ApiError {
                code: "busy",
                message: "the server is saturated; retry, or stop and read remain available".into(),
            }
            .into());
        }
        // Windows has no pre-admission classifier, so the flood budget Unix
        // enforces before spawning is enforced here instead: after parsing but
        // before authentication and state work. Priority-shaped traffic never
        // spends the budget, so read and stop stay on the fast path however
        // fast peers connect. Windows has no pre-admission authorization
        // gate, so this shape check (unlike the Unix reserve gate) still
        // admits fake-priority frames to a handler thread; the handler
        // re-authorizes them before touching state.
        #[cfg(windows)]
        if !is_priority_request(&op, action.as_deref()) && !admit_non_priority(shared) {
            return Err(ApiError {
                code: "busy",
                message: "the server is saturated; retry, or stop and read remain available".into(),
            }
            .into());
        }
        let auth = value
            .get("auth")
            .cloned()
            .map(serde_json::from_value::<RequestAuth>)
            .transpose()
            .context("auth must contain an agent and token")?;
        handle_request(op, action, auth, value, shared)
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

/// Authorize and apply one parsed request. Split from `handle` so every lane
/// shares the same policy and validation: lane admission never bypasses
/// authentication, it only decides scheduling.
fn handle_request(
    op: String,
    action: Option<String>,
    auth: Option<RequestAuth>,
    mut value: Json,
    shared: &Shared,
) -> Result<Json> {
    let agent_policy = shared.registry.host_state::<Option<AgentAuthPolicy>>();
    if let Some(policy) = agent_policy.as_ref() {
        let required = auth::required_permission(&op, action.as_deref()).ok();
        let authorization = policy.authorize(auth.as_ref(), &op, action.as_deref());
        if let Some(required) = required {
            let (permission, risk) = match required {
                auth::AgentPermission::Read => ("read", crate::security::PolicyRisk::Low),
                auth::AgentPermission::Edit => ("edit", crate::security::PolicyRisk::High),
                auth::AgentPermission::Control => ("control", crate::security::PolicyRisk::High),
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
        authorization.map_err(|error| anyhow::anyhow!("live request is unauthorized: {error}"))?;
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
}

#[cfg(all(test, unix))]
mod admission_tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn token_hash(token: &str) -> String {
        let digest = Sha256::digest(token.as_bytes());
        let mut output = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
        }
        output
    }

    fn policy(reader_token: &str, operator_token: &str) -> AgentAuthPolicy {
        let document = serde_json::json!({
            "version": 1,
            "agents": {
                "reader": {
                    "token_sha256": token_hash(reader_token),
                    "permissions": ["read"],
                },
                "operator": {
                    "token_sha256": token_hash(operator_token),
                    "permissions": ["stop"],
                },
            },
        });
        AgentAuthPolicy::from_json(document.to_string().as_bytes()).expect("test policy parses")
    }

    fn line(value: serde_json::Value) -> Vec<u8> {
        value.to_string().into_bytes()
    }

    #[test]
    fn reserve_stays_open_without_policy_so_stop_availability_is_unchanged() {
        assert!(reserve_eligible(
            &line(serde_json::json!({"op": "read"})),
            None
        ));
        assert!(reserve_eligible(
            &line(serde_json::json!({"op": "control", "action": "stop"})),
            None,
        ));
        assert!(!reserve_eligible(
            &line(serde_json::json!({"op": "wait"})),
            None
        ));
        assert!(!reserve_eligible(b"not json", None));
    }

    #[test]
    fn reserve_demotes_no_policy_claims_carrying_auth() {
        // Mirrors `handle_request`: without a policy any `auth` field is
        // rejected, so it must not claim a reserve slot either.
        for auth in [
            serde_json::json!({"agent": "any", "token": "t".repeat(32)}),
            serde_json::json!({"agent": "any"}),
            serde_json::json!({"agent": "any", "token": "t".repeat(32), "extra": 1}),
            serde_json::json!(null),
            serde_json::json!("bearer"),
        ] {
            assert!(
                !reserve_eligible(
                    &line(serde_json::json!({"op": "read", "auth": auth.clone()})),
                    None
                ),
                "no-policy read with auth must not claim reserve: {auth}",
            );
            assert!(
                !reserve_eligible(
                    &line(serde_json::json!({
                        "op": "control",
                        "action": "stop",
                        "auth": auth.clone(),
                    })),
                    None
                ),
                "no-policy stop with auth must not claim reserve: {auth}",
            );
        }
        // Without `auth` the trusted same-user fast path is unchanged.
        assert!(reserve_eligible(
            &line(serde_json::json!({"op": "read", "path": "/bindings"})),
            None
        ));
        assert!(reserve_eligible(
            &line(serde_json::json!({
                "op": "control",
                "action": "stop",
                "if_revision": 7,
            })),
            None
        ));
    }

    #[test]
    fn reserve_demotes_unauthenticated_and_under_permissioned_priority() {
        let reader_token = "r".repeat(32);
        let operator_token = "o".repeat(32);
        let policy = policy(&reader_token, &operator_token);
        let some = Some(&policy);
        // No credential at all: demoted even though the shape claims priority.
        assert!(!reserve_eligible(
            &line(serde_json::json!({"op": "read"})),
            some
        ));
        assert!(!reserve_eligible(
            &line(serde_json::json!({"op": "control", "action": "stop"})),
            some,
        ));
        // Correctly authorized priority: still reserve-eligible.
        assert!(reserve_eligible(
            &line(serde_json::json!({
                "op": "read",
                "auth": {"agent": "reader", "token": reader_token},
            })),
            some,
        ));
        assert!(reserve_eligible(
            &line(serde_json::json!({
                "op": "control",
                "action": "stop",
                "auth": {"agent": "operator", "token": operator_token},
            })),
            some,
        ));
        // Wrong permission for the operation: demoted, never reserve.
        assert!(!reserve_eligible(
            &line(serde_json::json!({
                "op": "control",
                "action": "stop",
                "auth": {"agent": "reader", "token": reader_token},
            })),
            some,
        ));
        assert!(!reserve_eligible(
            &line(serde_json::json!({
                "op": "read",
                "auth": {"agent": "operator", "token": operator_token},
            })),
            some,
        ));
        // Wrong token and malformed auth: demoted.
        assert!(!reserve_eligible(
            &line(serde_json::json!({
                "op": "read",
                "auth": {"agent": "reader", "token": "x".repeat(32)},
            })),
            some,
        ));
        assert!(!reserve_eligible(
            &line(serde_json::json!({"op": "read", "auth": {"agent": "reader"}})),
            some,
        ));
        // Non-priority stays non-eligible even with a valid credential.
        assert!(!reserve_eligible(
            &line(serde_json::json!({
                "op": "wait",
                "auth": {"agent": "reader", "token": reader_token},
            })),
            some,
        ));
    }

    #[test]
    fn reserve_demotes_malformed_auth_shapes() {
        let reader_token = "r".repeat(32);
        let operator_token = "o".repeat(32);
        let policy = policy(&reader_token, &operator_token);
        let some = Some(&policy);
        // Every malformed `auth` below fails the handler's strict parse with
        // `invalid_request`, so none may claim a reserve slot. Valid shapes
        // with wrong credentials stay demoted via `authorize` instead.
        for auth in [
            serde_json::json!({"agent": "reader"}),
            serde_json::json!({"token": reader_token.clone()}),
            serde_json::json!({}),
            serde_json::json!({"agent": "reader", "token": reader_token.clone(), "extra": 1}),
            serde_json::json!({"agent": "reader", "token": 7}),
            serde_json::json!({"agent": 7, "token": reader_token.clone()}),
            serde_json::json!({"agent": "", "token": reader_token.clone()}),
            serde_json::json!(null),
            serde_json::json!("bearer"),
            serde_json::json!(["reader", reader_token.clone()]),
        ] {
            assert!(
                !reserve_eligible(
                    &line(serde_json::json!({"op": "read", "auth": auth.clone()})),
                    some
                ),
                "malformed read auth must not claim reserve: {auth}",
            );
            assert!(
                !reserve_eligible(
                    &line(serde_json::json!({
                        "op": "control",
                        "action": "stop",
                        "auth": auth.clone(),
                    })),
                    some
                ),
                "malformed stop auth must not claim reserve: {auth}",
            );
        }
        // Wrong Bearer [REDACTED] well-formed auth is demoted by authorization, not parsing.
        assert!(!reserve_eligible(
            &line(serde_json::json!({
                "op": "control",
                "action": "stop",
                "auth": {"agent": "operator", "token": "x".repeat(32)},
            })),
            some,
        ));
        assert!(!reserve_eligible(
            &line(serde_json::json!({
                "op": "read",
                "auth": {"agent": "unknown", "token": reader_token},
            })),
            some,
        ));
    }

    #[test]
    fn reserve_mirrors_handler_field_validation_for_priority() {
        let reader_token = "r".repeat(32);
        let operator_token = "o".repeat(32);
        let policy = policy(&reader_token, &operator_token);
        let some = Some(&policy);
        let reader = serde_json::json!({"agent": "reader", "token": reader_token});
        let operator = serde_json::json!({"agent": "operator", "token": operator_token});
        // Extra fields the handler rejects with `unexpected field` are
        // demoted even when authorized, so they cannot occupy the reserve.
        for message in [
            serde_json::json!({"op": "read", "auth": reader.clone(), "value": 1}),
            serde_json::json!({"op": "read", "auth": reader.clone(), "if_revision": 1}),
            serde_json::json!({"op": "read", "auth": reader.clone(), "after": 1}),
            serde_json::json!({"op": "read", "auth": reader.clone(), "timeout_ms": 1}),
            serde_json::json!({"op": "read", "auth": reader.clone(), "operations": []}),
            serde_json::json!({"op": "read", "auth": reader.clone(), "action": "stop"}),
            serde_json::json!({"op": "read", "auth": reader.clone(), "foo": 1}),
            serde_json::json!({"op": "control", "action": "stop", "auth": operator.clone(), "path": "/x"}),
            serde_json::json!({"op": "control", "action": "stop", "auth": operator.clone(), "after": 1}),
            serde_json::json!({"op": "control", "action": "stop", "auth": operator.clone(), "foo": 1}),
        ] {
            assert!(
                !reserve_eligible(&line(message.clone()), some),
                "authorized-but-invalid priority must not claim reserve: {message}",
            );
        }
        // `if_revision` must be a nonnegative integer, exactly as the handler
        // requires; a well-formed revision still keeps reserve eligibility
        // because freshness is state-dependent.
        for bad in [
            serde_json::json!("7"),
            serde_json::json!(7.5),
            serde_json::json!(-1),
            serde_json::json!(null),
        ] {
            assert!(
                !reserve_eligible(
                    &line(serde_json::json!({
                        "op": "control",
                        "action": "stop",
                        "auth": operator.clone(),
                        "if_revision": bad.clone(),
                    })),
                    some
                ),
                "stop with non-u64 if_revision must not claim reserve: {bad}",
            );
        }
        assert!(reserve_eligible(
            &line(serde_json::json!({
                "op": "control",
                "action": "stop",
                "auth": operator.clone(),
                "if_revision": 7,
            })),
            some,
        ));
        // Type errors the handler's `Request` parse rejects are demoted too.
        // An explicit null `path` deserializes like an absent one, so it stays
        // eligible exactly as the handler accepts it.
        assert!(!reserve_eligible(
            &line(serde_json::json!({"op": "read", "auth": reader.clone(), "path": 7})),
            some,
        ));
        assert!(reserve_eligible(
            &line(serde_json::json!({"op": "read", "auth": reader.clone(), "path": null})),
            some,
        ));
        // Well-formed priority with allowed fields stays eligible.
        assert!(reserve_eligible(
            &line(serde_json::json!({"op": "read", "auth": reader.clone(), "path": "/bindings"})),
            some,
        ));
        assert!(reserve_eligible(
            &line(serde_json::json!({
                "op": "control",
                "action": "stop",
                "auth": operator.clone(),
                "value": {"note": 1},
            })),
            some,
        ));
    }

    #[test]
    fn decide_lane_prefers_general_then_reserve_then_sheds() {
        // Priority claims use a free general slot first, with no flood cost.
        assert_eq!(decide_lane(0, 0, true, false), Admit::General);
        assert_eq!(decide_lane(MAX_CLIENTS - 1, 0, true, false), Admit::General);
        // Every general slot held: the two-slot reserve engages, still with
        // no flood cost, so a genuine `stop` never sheds on the flood budget.
        assert_eq!(decide_lane(MAX_CLIENTS, 0, true, false), Admit::Priority);
        assert_eq!(
            decide_lane(MAX_CLIENTS, PRIORITY_SPARE - 1, true, false),
            Admit::Priority
        );
        // Reserve exhausted while every general slot is held: the deterministic
        // gate for the inline `busy` reply (previously covered only by the
        // general-lane shed tests).
        assert_eq!(
            decide_lane(MAX_CLIENTS, PRIORITY_SPARE, true, false),
            Admit::Shed
        );
        assert_eq!(
            decide_lane(MAX_CLIENTS, PRIORITY_SPARE, true, true),
            Admit::Shed
        );
    }

    #[test]
    fn decide_lane_never_admits_demoted_priority_to_the_reserve() {
        // Demoted fake-priority follows the general lane: admitted within the
        // flood budget while a slot is free, shed past the burst or when full.
        assert_eq!(decide_lane(0, 0, false, true), Admit::General);
        assert_eq!(decide_lane(0, 0, false, false), Admit::Shed);
        assert_eq!(decide_lane(MAX_CLIENTS, 0, false, true), Admit::Shed);
        assert_eq!(
            decide_lane(MAX_CLIENTS, PRIORITY_SPARE, false, true),
            Admit::Shed
        );
    }
}

#[cfg(all(test, unix))]
mod peer_tests {
    use super::*;
    use crate::ipc::peer::peer_has_uid;

    #[test]
    fn real_socket_credentials_are_checked_in_both_directions() {
        let (left, right) = UnixStream::pair().unwrap();
        // SAFETY: geteuid takes no arguments, cannot fail, and reads no memory, so the
        // call has no pointer, lifetime or alignment requirement to uphold.
        let owner = crate::platform::unix_ffi::effective_uid();
        for peer in [&left, &right] {
            assert!(peer_has_uid(peer, owner));
            assert!(!peer_has_uid(peer, owner.wrapping_add(1)));
            assert!(peer_is_owner(peer));
        }
    }
}
