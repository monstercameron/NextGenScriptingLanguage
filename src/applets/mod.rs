//! Small, reusable operations for `.program` workflows.
//!
//! These functions intentionally contain no application-specific policy. The
//! `.program` workflow owns sequencing, retries, and decisions about which
//! operation to call.

mod aggregation;
mod arithmetic;
mod config;
mod csv;
mod desktop_entry;
mod ensure;
mod env_file;
mod executable_lookup;
mod file_test;
mod filesystem;
mod glob;
mod hash;
mod i18n;
mod ini;
// #357: nothing calls the encoder until the journald socket send (NEEDS) lands.
#[allow(dead_code)]
mod journal;
mod jsonc;
mod list;
mod lua_config;
mod path;
mod redact;
mod shared;
mod state;
mod structured;
mod sys_facts;
mod system_restart;
mod template;
mod text;
mod time;
mod toml;
mod transaction;
mod wait;
mod yaml;

pub(crate) fn process_alive(pid: u32) -> bool {
    transaction::process_alive(pid)
}

pub(crate) fn valid_time_zone(zone: &str) -> bool {
    time::valid_zone(zone)
}

pub(crate) fn validate_json_pointer(pointer: &str) -> Result<(), String> {
    structured::validate_pointer(pointer).map_err(|error| error.message)
}

use serde_json::{Map, Value};
use std::fmt;
use std::path::Path;
use std::time::Duration;

/// The execution budget stopped an in-process applet at a cooperative
/// checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionBudgetExceeded {
    Timeout,
    Cancelled,
}

/// Scoped checkpoint policy for an in-process applet execution.
///
/// The callback is borrowed for one synchronous invocation and is never
/// installed in process-global state. Applets call it periodically while
/// traversing bounded CPU work. It cannot interrupt an operation that is
/// blocked inside an operating-system call or a library routine that does not
/// expose checkpoints.
pub struct ExecutionContext<'a> {
    checkpoint: &'a dyn Fn() -> Result<(), ExecutionBudgetExceeded>,
    fs_timeout: Option<Duration>,
}

impl<'a> ExecutionContext<'a> {
    /// Create a context whose callback checks the caller's deadline or
    /// cancellation state.
    pub fn new(checkpoint: &'a dyn Fn() -> Result<(), ExecutionBudgetExceeded>) -> Self {
        Self {
            checkpoint,
            fs_timeout: None,
        }
    }

    /// Create a context for trusted direct calls that have no runtime budget.
    pub fn unrestricted() -> ExecutionContext<'static> {
        ExecutionContext {
            checkpoint: &always_continue,
            fs_timeout: None,
        }
    }

    /// Bound filesystem opens and readiness waits started under this context
    /// to `timeout` (threat model H7). `None` keeps the shared default.
    pub fn with_fs_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.fs_timeout = timeout;
        self
    }

    fn checkpoint(&self) -> Result<(), AppletError> {
        (self.checkpoint)().map_err(AppletError::budget_exceeded)
    }

    /// The filesystem-operation deadline for this context: the configured
    /// `--fs-timeout-ms` value, else the shared default (always finite).
    pub(crate) fn fs_timeout(&self) -> Duration {
        self.fs_timeout.unwrap_or(crate::fs_io::DEFAULT_FS_TIMEOUT)
    }

    /// The wait checkpoint for bounded filesystem helpers: budget overruns
    /// surface as [`std::io::ErrorKind::TimedOut`] and cancellation as
    /// [`std::io::ErrorKind::Interrupted`], so [`fs_io_error`] maps them onto
    /// the typed timeout and cancellation errors.
    pub(crate) fn fs_wait_checkpoint(&self) -> impl Fn() -> std::io::Result<()> + '_ {
        let checkpoint = self.checkpoint;
        move || {
            checkpoint().map_err(|exceeded| match exceeded {
                ExecutionBudgetExceeded::Timeout => std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "execution deadline exceeded; applet effects may be incomplete",
                ),
                ExecutionBudgetExceeded::Cancelled => std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "workflow cancelled; applet effects may be incomplete",
                ),
            })
        }
    }
}

fn always_continue() -> Result<(), ExecutionBudgetExceeded> {
    Ok(())
}

const COOPERATIVE_CHECK_INTERVAL: usize = 256;

pub(crate) fn checkpoint_work(
    context: &ExecutionContext<'_>,
    work: usize,
) -> Result<(), AppletError> {
    if work.is_multiple_of(COOPERATIVE_CHECK_INTERVAL) {
        context.checkpoint()?;
    }
    Ok(())
}

/// A stable error envelope for the standalone helper protocol.
#[derive(Debug)]
pub struct AppletError {
    pub code: &'static str,
    pub kind: AppletErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppletErrorKind {
    Absence,
    Permanent,
    Retryable,
    Uncertain,
    Timeout,
    Cancelled,
}

impl AppletError {
    pub fn exit_status(&self) -> i32 {
        match self.kind {
            AppletErrorKind::Absence => 1,
            AppletErrorKind::Permanent => 2,
            AppletErrorKind::Retryable => 75,
            // Keep uncertain effects distinct from retryable conflicts so
            // contracts never imply that replay is safe.
            AppletErrorKind::Uncertain => 76,
            AppletErrorKind::Timeout => 124,
            AppletErrorKind::Cancelled => 130,
        }
    }
}

impl AppletError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            kind: AppletErrorKind::Permanent,
            message: message.into(),
        }
    }

    fn failed(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind: match code {
                "conflict" | "lock_timeout" => AppletErrorKind::Retryable,
                "uncertain_effect" | "manual_recovery" => AppletErrorKind::Uncertain,
                _ => AppletErrorKind::Permanent,
            },
            message: message.into(),
        }
    }

    fn absence(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind: AppletErrorKind::Absence,
            message: message.into(),
        }
    }

    fn timeout(message: impl Into<String>) -> Self {
        Self {
            code: "timeout",
            kind: AppletErrorKind::Timeout,
            message: message.into(),
        }
    }

    fn budget_exceeded(exceeded: ExecutionBudgetExceeded) -> Self {
        match exceeded {
            ExecutionBudgetExceeded::Timeout => Self {
                code: "timeout",
                kind: AppletErrorKind::Timeout,
                message: "execution deadline exceeded; applet effects may be incomplete".into(),
            },
            ExecutionBudgetExceeded::Cancelled => Self {
                code: "cancelled",
                kind: AppletErrorKind::Cancelled,
                message: "workflow cancelled; applet effects may be incomplete".into(),
            },
        }
    }
}

impl fmt::Display for AppletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppletError {}

/// Run one generic operation. Workflow control and operation ordering belong
/// to the calling `.program` or runtime.
pub fn execute(
    operation: &str,
    args: &Map<String, Value>,
    input: &[u8],
) -> Result<Value, AppletError> {
    let cwd = std::env::current_dir()
        .map_err(|error| io_error(error, "getting the process working directory"))?;
    execute_in_cwd(operation, args, input, &cwd)
}

/// Run one generic operation with an explicit base directory for relative
/// filesystem paths. This is safe for parallel runtimes because it never
/// changes process-global current-directory state.
pub fn execute_in_cwd(
    operation: &str,
    args: &Map<String, Value>,
    input: &[u8],
    cwd: &Path,
) -> Result<Value, AppletError> {
    execute_with_context(
        operation,
        args,
        input,
        cwd,
        &ExecutionContext::unrestricted(),
    )
}

/// Run one generic operation with an explicit working directory and scoped
/// cooperative execution context.
pub fn execute_with_context(
    operation: &str,
    args: &Map<String, Value>,
    input: &[u8],
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let result = match operation {
        "list.count"
        | "list.sum"
        | "list.min"
        | "list.max"
        | "list.unique"
        | "list.sort"
        | "list.top"
        | "list.group"
        | "text.count_lines_by" => aggregation::run(operation, args, input, context),
        "list.range" | "list.enumerate" | "list.slice" | "list.first" | "list.last"
        | "list.rest" | "list.reverse" | "list.concat" | "list.flatten" | "list.zip"
        | "list.chunk" => list::run(operation, args, context),
        "path.dirname" => path::dirname(args),
        "path.basename" => path::basename(args),
        "path.remove_suffix" => path::remove_suffix(args),
        "text.trim_final_lf" | "text.normalize" => text::trim_final_lf(args, input),
        "text.trim" | "text.length" | "text.substring" | "text.pad" | "text.repeat"
        | "text.starts_with" | "text.ends_with" | "text.contains_any" | "text.truncate" => {
            text::shape(operation, args, input, context)
        }
        "text.concat" => text::concat(args, context),
        "text.matches" => text::matches(args, input, context),
        "text.default_if_empty" => text::default_if_empty(args, input),
        "text.replace" => text::replace(args, input, context),
        "text.strip_prefix" => text::strip_prefix(args, input),
        "text.strip_suffix" => text::strip_suffix(args, input),
        "text.lower" => text::lower(args, input),
        "text.upper" => text::upper(args, input),
        "text.join" => text::concat(args, context),
        "text.render" => template::render(args, cwd, context),
        "text.format" => template::format(args, context),
        "redact.text" => redact::text(args),
        "i18n.t" => i18n::translate(args, cwd, context),
        "desktop.entry_get" => desktop_entry::entry_get(args, context),
        "desktop.entry_set" => desktop_entry::entry_set(args, context),
        "desktop.exec_argv" => desktop_entry::exec_argv(args, context),
        "lua.encode" => lua_config::encode(args, context),
        "lua.decode" => lua_config::decode(args, context),
        "fs.ensure_file" => ensure::ensure_file(args, cwd, context),
        "fs.ensure_line" => ensure::ensure_line(args, cwd, context),
        "conf.set" => ensure::conf_set(args, cwd, context),
        "conf.ensure_block" => ensure::conf_ensure_block(args, cwd, context),
        "conf.remove_block" => ensure::conf_remove_block(args, cwd, context),
        "fs.ensure_symlink" => ensure::ensure_symlink(args, cwd, context),
        "fs.ensure_dir" => ensure::ensure_dir(args, cwd, context),
        "yaml.parse" => yaml::parse(args, input, context),
        "yaml.stringify" => yaml::stringify(args),
        "toml.parse" => toml::parse(args, input, context),
        "toml.stringify" => toml::stringify(args),
        "env.load" => env_file::load(input, args),
        "ini.parse" => ini::parse(args, input, context),
        "ini.stringify" => ini::stringify(args),
        "path.join" => path::join(args),
        "text.lines" => text::lines(args, input, context),
        "text.split" => text::split(args, input, context),
        "fs.test" => file_test::test(args, cwd, context),
        "exec.which" => executable_lookup::which(args, cwd, context),
        "terminal.info" => file_test::terminal_info(args, context),
        "fs.glob" => glob::glob(args, cwd, context),
        "number.add" | "number.subtract" | "number.multiply" | "number.divide"
        | "number.compare" | "number.int_divide" | "number.modulo" | "number.divmod"
        | "number.min" | "number.max" | "number.clamp" | "number.abs" | "number.sign"
        | "number.round" | "number.floor" | "number.ceil" | "number.trunc" | "number.format"
        | "number.format_size" => arithmetic::run(operation, args),
        "number.parse_integer" | "number.parse_integer.result" => {
            arithmetic::parse_integer(args, input)
        }
        "number.to_text" => arithmetic::to_text(args),
        "color.rgb_to_hex" => arithmetic::rgb_to_hex(args),
        "color.parse_rgb" => arithmetic::parse_rgb(args, input),
        "color.parse_rgb.result" => arithmetic::parse_rgb_result(args, input),
        "json.parse" => structured::parse(args, input, context),
        "json.stringify" => structured::stringify(args, context),
        "csv.parse" => csv::parse(args, input, context),
        "csv.stringify" => csv::stringify(args, context),
        "json.get" => structured::get(args, input, context),
        "json.set" => structured::set(args, input, context),
        "json.map" => structured::map(args, input, context),
        "json.filter" => structured::filter(args, input, context),
        "json.keys" => structured::keys(args, input, context),
        "json.entries" => structured::entries(args, input, context),
        "json.values" => structured::values(args, input, context),
        "json.has" => structured::has(args, input, context),
        "json.delete" => structured::delete(args, input, context),
        "json.merge" => structured::merge(args, context),
        "json.length" => structured::length(args, input, context),
        "json.pick" => structured::pick(args, input, context),
        "json.omit" => structured::omit(args, input, context),
        "json.rename_keys" => structured::rename_keys(args, input, context),
        "json.concat" => structured::concat(args, context),
        "json.first" => structured::edge(args, input, context, false),
        "json.last" => structured::edge(args, input, context, true),
        "json.group_by" => structured::group_by(args, input, context),
        "json.unique_by" => structured::unique_by(args, input, context),
        "json.sum" => structured::sum(args, input, context),
        "json.sort_by" => structured::sort_by(args, input, context),
        "hash.sha256" | "hash.sha512" | "encode.base64" | "decode.base64" | "encode.hex"
        | "decode.hex" | "encode.url" | "decode.url" | "encode.form" => {
            hash::run(operation, args, input, context)
        }
        "time.now" | "time.format" | "time.parse" | "time.parts" | "time.add" | "time.diff" => {
            time::run(operation, args, context)
        }
        "time.sleep" | "wait.until" => wait::run(operation, args, cwd, context),
        "jsonc.edit_content" => jsonc::edit_content(args, input, context),
        "fs.transaction" => transaction::apply(args, cwd, context),
        "fs.transaction.recover" => transaction::recover(args, cwd, context),
        "fs.read" | "fs.snapshot" => filesystem::read(args, cwd, context),
        "fs.stat" => filesystem::stat(args, cwd, context),
        "fs.realpath" => filesystem::realpath(args, cwd, context),
        "fs.copy" => filesystem::copy(args, cwd, context),
        "sys.facts" => sys_facts::facts(args, context),
        "system.restart_required" => system_restart::assess(args, context),
        "state.flag_set" | "state.flag_clear" | "state.flag_is_set" | "state.toggle"
        | "state.value_load" | "state.value_store" | "state.debounce" => {
            state::run(operation, args, context)
        }
        "fs.write_atomic" => filesystem::write_atomic(args, input, cwd, context),
        "fs.unlink_checked" => filesystem::unlink_checked(args, cwd, context),
        "jsonc.set" => jsonc::set(args, cwd, context),
        "jsonc.remove" => jsonc::remove(args, cwd, context),
        _ => Err(AppletError::failed(
            "unknown_operation",
            format!("unknown operation {operation:?}"),
        )),
    };
    if result.is_ok() {
        context.checkpoint()?;
    }
    result
}

/// Return the env.load output strings selected for runtime-secret treatment.
/// The runtime uses this before exposing the binding to observers or persistence.
pub(crate) fn secret_values(
    operation: &str,
    args: &Map<String, Value>,
    output: &Value,
) -> Result<Vec<String>, AppletError> {
    match operation {
        "env.load" => env_file::secret_values(args, output),
        _ => Ok(Vec::new()),
    }
}

/// The parameter an in-process applet can read straight from an interpreter value,
/// without converting it to a JSON tree first. The runtime leaves that parameter out of
/// the converted argument map and passes the bound value to [`execute_with_typed_json`].
pub(crate) fn typed_json_parameter(applet: &str) -> Option<&'static str> {
    match applet {
        "json.get" => Some("document"),
        _ => None,
    }
}

/// Run an applet whose [`typed_json_parameter`] is supplied as an interpreter value.
pub(crate) fn execute_with_typed_json(
    applet: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    typed: &crate::ast::Value,
    context: &ExecutionContext<'_>,
) -> Result<serde_json::Value, AppletError> {
    match applet {
        "json.get" => structured::get_typed(args, typed, context),
        _ => Err(invalid(format!(
            "applet {applet:?} takes no typed JSON parameter"
        ))),
    }
}

pub(crate) fn required_string<'a>(
    args: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, AppletError> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| AppletError::invalid(format!("args.{name} must be a string")))
}

/// Read an optional string, defaulting only when the field is absent.
pub(crate) fn optional_string<'a>(
    args: &'a Map<String, Value>,
    name: &str,
    default: &'a str,
) -> Result<&'a str, AppletError> {
    match args.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_str()
            .ok_or_else(|| invalid(format!("args.{name} must be a string"))),
    }
}

/// Read an optional boolean, defaulting only when the field is absent.
pub(crate) fn optional_bool(
    args: &Map<String, Value>,
    name: &str,
    default: bool,
) -> Result<bool, AppletError> {
    match args.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid(format!("args.{name} must be a boolean"))),
    }
}

/// Read an optional unsigned integer, defaulting only when the field is absent.
pub(crate) fn optional_u64(
    args: &Map<String, Value>,
    name: &str,
    default: u64,
) -> Result<u64, AppletError> {
    match args.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| invalid(format!("args.{name} must be a nonnegative integer"))),
    }
}

pub(crate) fn io_error(error: std::io::Error, context: impl fmt::Display) -> AppletError {
    if error.kind() == std::io::ErrorKind::NotFound {
        AppletError::absence("not_found", format!("{context}: {error}"))
    } else if matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::StorageFull
    ) {
        AppletError {
            code: "io_error",
            kind: AppletErrorKind::Retryable,
            message: format!("{context}: {error}"),
        }
    } else {
        AppletError::failed("io_error", format!("{context}: {error}"))
    }
}

/// Open `open_path` for reading without ever blocking in `open(2)`, refuse
/// non-regular targets, wait for readiness within this context's
/// filesystem-operation deadline, and return the handle in blocking mode.
/// Errors name `display_path`, which may differ from `open_path` when the
/// caller resolved a symlink first. A FIFO or socket fails with the typed
/// `timeout` (threat model H7); any other non-regular target keeps the
/// long-standing `path is not a regular file` refusal. Nothing is created,
/// truncated, or written: the path is only ever opened read-only.
pub(crate) fn open_regular_input(
    open_path: &Path,
    display_path: &Path,
    no_follow_final: bool,
    context: &ExecutionContext<'_>,
) -> Result<std::fs::File, AppletError> {
    let file = crate::fs_io::open_nonblocking(open_path, no_follow_final)
        .map_err(|error| io_error(error, format!("opening {}", display_path.display())))?;
    if let Err(error) = crate::fs_io::classify_regular_input(&file, display_path) {
        return Err(match error.kind() {
            std::io::ErrorKind::TimedOut => {
                fs_io_error(error, format!("opening {}", display_path.display()))
            }
            std::io::ErrorKind::InvalidInput => AppletError::invalid(format!(
                "path is not a regular file: {}",
                display_path.display()
            )),
            _ => io_error(error, format!("checking {}", display_path.display())),
        });
    }
    let wait_checkpoint = context.fs_wait_checkpoint();
    let deadline = crate::fs_io::fs_deadline(Some(context.fs_timeout()), None);
    if let Err(error) = crate::fs_io::wait_readable(&file, display_path, deadline, &wait_checkpoint)
    {
        return Err(fs_io_error(
            error,
            format!("opening {}", display_path.display()),
        ));
    }
    crate::fs_io::restore_blocking(&file)
        .map_err(|error| io_error(error, format!("opening {}", display_path.display())))?;
    Ok(file)
}

/// Map a bounded-filesystem-helper failure onto a typed applet error: a
/// [`std::io::ErrorKind::TimedOut`] wait becomes the typed `timeout` error
/// (threat model H7) instead of the retryable `io_error` that [`io_error`]
/// reports for that kind; everything else keeps the usual mapping,
/// including absence for missing paths.
pub(crate) fn fs_io_error(error: std::io::Error, context: impl fmt::Display) -> AppletError {
    // Within the bounded filesystem helpers, `Interrupted` only escapes via
    // the caller's wait checkpoint (raw EINTR from `open`/`poll` is retried
    // inside), so it reports cancellation rather than a retryable I/O error.
    // `TimedOut` covers both the filesystem-operation deadline and an
    // overrun run budget (the checkpoint reports the latter as `TimedOut`
    // too); the message names which one fired.
    match error.kind() {
        std::io::ErrorKind::TimedOut => AppletError::timeout(format!("{context}: {error}")),
        std::io::ErrorKind::Interrupted => {
            AppletError::budget_exceeded(ExecutionBudgetExceeded::Cancelled)
        }
        _ => io_error(error, context),
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> AppletError {
    AppletError::invalid(message)
}

pub(crate) fn failed(code: &'static str, message: impl Into<String>) -> AppletError {
    AppletError::failed(code, message)
}

pub(crate) fn absence(code: &'static str, message: impl Into<String>) -> AppletError {
    AppletError::absence(code, message)
}

#[cfg(test)]
mod tests {
    use super::execute_in_cwd;
    use serde_json::{json, Map, Value};
    use std::fs;
    use std::path::Path;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn run(operation: &str, args: Value, cwd: &Path) -> Result<Value, super::AppletError> {
        let map: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
        execute_in_cwd(operation, &map, b"", cwd)
    }

    #[test]
    fn exec_which_reports_absent_names_and_refuses_separators() {
        let dir = tempfile::tempdir().unwrap();
        let absent = run(
            "exec.which",
            json!({"name": "execute-no-such-tool-318", "path": dir.path()}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(absent, json!({"found": false, "path": null, "all": []}));
        assert!(run(
            "exec.which",
            json!({"name": "bin/tool", "path": ""}),
            dir.path()
        )
        .is_err());
    }

    #[test]
    fn sys_facts_reads_only_under_the_given_root() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("sys/class/dmi/id");
        fs::create_dir_all(&vendor).unwrap();
        fs::write(vendor.join("sys_vendor"), "Fixture Corp\n").unwrap();
        let output = run("sys.facts", json!({"root": dir.path()}), dir.path()).unwrap();
        assert_eq!(output["dmi"]["sys_vendor"], "Fixture Corp");
        assert_eq!(output["pci_display"], json!([]));
        assert!(run("sys.facts", json!({"root": "relative/root"}), dir.path()).is_err());
    }

    #[test]
    fn system_restart_required_reads_the_reboot_marker_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let quiet = run(
            "system.restart_required",
            json!({"root": dir.path()}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(quiet["reboot_required"], json!(false));
        assert_eq!(quiet["reasons"], json!([]));
        let marker_dir = dir.path().join("var/run");
        fs::create_dir_all(&marker_dir).unwrap();
        fs::write(
            marker_dir.join("reboot-required"),
            "*** System restart required ***\n",
        )
        .unwrap();
        let flagged = run(
            "system.restart_required",
            json!({"root": dir.path()}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(flagged["reboot_required"], json!(true));
    }

    #[test]
    fn state_flags_toggle_and_values_round_trip_in_a_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path();
        let call = |operation: &str, arguments: Value| {
            super::state::apply(operation, &args(arguments), state).unwrap()
        };
        assert_eq!(
            call("state.flag_set", json!({"name": "maint/window"})),
            json!({"set": true})
        );
        assert_eq!(
            call("state.flag_is_set", json!({"name": "maint/window"})),
            json!(true)
        );
        assert_eq!(
            call("state.toggle", json!({"name": "maint/window"})),
            json!(false)
        );
        assert_eq!(
            call("state.toggle", json!({"name": "maint/window"})),
            json!(true)
        );
        assert_eq!(
            call("state.flag_clear", json!({"name": "maint/window"})),
            json!({"set": false})
        );
        assert_eq!(
            call("state.flag_is_set", json!({"name": "maint/window"})),
            json!(false)
        );
        assert_eq!(
            call("state.value_load", json!({"name": "last/build"})),
            json!({"found": false, "value": null})
        );
        assert_eq!(
            call(
                "state.value_store",
                json!({"name": "last/build", "value": "v1.2"})
            ),
            json!({"written": true})
        );
        assert_eq!(
            call("state.value_load", json!({"name": "last/build"})),
            json!({"found": true, "value": "v1.2"})
        );
    }

    #[test]
    fn state_debounce_allows_one_event_inside_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let arguments = args(json!({"name": "notify/disk", "window_ms": 60_000}));
        assert_eq!(
            super::state::apply("state.debounce", &arguments, dir.path()).unwrap(),
            json!(true)
        );
        assert_eq!(
            super::state::apply("state.debounce", &arguments, dir.path()).unwrap(),
            json!(false)
        );
    }

    #[test]
    fn state_refuses_unsafe_names_and_missing_arguments() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run("state.flag_set", json!({"name": "../escape"}), dir.path()).is_err());
        assert!(run("state.flag_set", json!({"name": 7}), dir.path()).is_err());
        assert!(run("state.value_store", json!({"name": "ok"}), dir.path()).is_err());
        assert!(run("state.debounce", json!({"name": "ok"}), dir.path()).is_err());
    }

    #[test]
    fn redact_text_replaces_literals_tokens_emails_and_ipv4() {
        let dir = tempfile::tempdir().unwrap();
        let output = run(
            "redact.text",
            json!({
                "text": "mail ops@example.com from 10.0.0.7 key sk_live_abcdefghijklmnop1234 and secret-x",
                "literals": ["secret-x"],
            }),
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            output["text"],
            "mail <email-1> from <ipv4-1> key <token-1> and <literal-1>"
        );
        assert_eq!(
            output["counts"],
            json!({"literal": 1, "token": 1, "email": 1, "ipv4": 1})
        );
    }

    #[test]
    fn redact_text_without_literals_passes_plain_text_through() {
        let dir = tempfile::tempdir().unwrap();
        let output = run("redact.text", json!({"text": "nothing here"}), dir.path()).unwrap();
        assert_eq!(output["text"], "nothing here");
        assert_eq!(
            output["counts"],
            json!({"literal": 0, "token": 0, "email": 0, "ipv4": 0})
        );
    }

    #[test]
    fn redact_text_refuses_non_string_literals() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run(
            "redact.text",
            json!({"text": "x", "literals": [1]}),
            dir.path()
        )
        .is_err());
    }

    #[test]
    fn fs_stat_answers_absent_for_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let output = run("fs.stat", json!({"path": "missing.txt"}), dir.path()).unwrap();
        assert_eq!(
            output,
            json!({"type": "absent", "size": null, "mode": null, "read_only": null, "mtime_ns": null})
        );
    }

    #[test]
    fn fs_stat_reports_file_size_and_directory_type() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"abc").unwrap();
        let file = run("fs.stat", json!({"path": "a.txt"}), dir.path()).unwrap();
        assert_eq!(file["type"], "file");
        assert_eq!(file["size"], 3);
        let folder = run("fs.stat", json!({"path": "."}), dir.path()).unwrap();
        assert_eq!(folder["type"], "directory");
    }

    #[cfg(unix)]
    #[test]
    fn fs_stat_describes_a_link_itself_unless_follow_is_set() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("target.txt"), b"abc").unwrap();
        std::os::unix::fs::symlink("target.txt", dir.path().join("link")).unwrap();
        let link = run("fs.stat", json!({"path": "link"}), dir.path()).unwrap();
        assert_eq!(link["type"], "symlink");
        let followed = run(
            "fs.stat",
            json!({"path": "link", "follow": true}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(followed["type"], "file");
        assert_eq!(followed["size"], 3);
    }

    #[test]
    fn fs_realpath_returns_the_canonical_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"abc").unwrap();
        let output = run("fs.realpath", json!({"path": "a.txt"}), dir.path()).unwrap();
        let resolved = output["realpath"].as_str().unwrap();
        assert!(Path::new(resolved).is_absolute());
        assert!(resolved.ends_with("a.txt"));
        assert!(Path::new(resolved).is_file());
    }

    #[test]
    fn fs_realpath_refuses_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run("fs.realpath", json!({"path": "missing.txt"}), dir.path()).is_err());
    }

    #[test]
    fn conf_set_edits_only_the_key_line_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let original = "# top comment\nname = demo\nport=80\n";
        fs::write(dir.path().join("app.conf"), original).unwrap();
        let args = json!({"path": "app.conf", "key": "port", "value": "8080"});
        let first = run("conf.set", args.clone(), dir.path()).unwrap();
        assert_eq!(first, json!({"changed": true, "path": "app.conf"}));
        assert_eq!(
            fs::read_to_string(dir.path().join("app.conf")).unwrap(),
            "# top comment\nname = demo\nport=8080\n"
        );
        let second = run("conf.set", args, dir.path()).unwrap();
        assert_eq!(second["changed"], false);
    }

    #[test]
    fn conf_set_creates_a_missing_file_with_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let output = run(
            "conf.set",
            json!({"path": "new.conf", "key": "port", "value": "8080", "separator": ":", "spaced": true}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(output["changed"], true);
        assert_eq!(
            fs::read_to_string(dir.path().join("new.conf")).unwrap(),
            "port: 8080\n"
        );
    }

    #[test]
    fn conf_set_refuses_unknown_style_values() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run(
            "conf.set",
            json!({"path": "x.conf", "key": "k", "value": "v", "separator": "=="}),
            dir.path(),
        )
        .is_err());
        assert!(run(
            "conf.set",
            json!({"path": "x.conf", "key": "k", "value": "v", "comment": "%"}),
            dir.path(),
        )
        .is_err());
        assert!(!dir.path().join("x.conf").exists());
    }

    fn write_catalog(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn i18n_t_uses_the_requested_language_then_english_then_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let catalogs = dir.path().join("catalogs");
        fs::create_dir(&catalogs).unwrap();
        write_catalog(
            &catalogs,
            "da.json",
            r#"{"language":"da","messages":{"":{"Saved":"Gemt","Hello {name}":"Hej {name}","Blank":""},"menu":{"Saved":"Gem"}}}"#,
        );
        write_catalog(
            &catalogs,
            "en.json",
            r#"{"language":"en","messages":{"":{"Saved":"Saved","Only english":"Only English"}}}"#,
        );
        let lookup = |arguments: Value| run("i18n.t", arguments, dir.path()).unwrap();
        let base = json!({"catalog_dir": "catalogs", "language": "da"});
        let with = |extra: Value| {
            let mut map = base.as_object().cloned().unwrap();
            map.extend(extra.as_object().cloned().unwrap());
            Value::Object(map)
        };
        assert_eq!(lookup(with(json!({"key": "Saved"}))), json!("Gemt"));
        assert_eq!(
            lookup(with(json!({"key": "Saved", "context": "menu"}))),
            json!("Gem")
        );
        assert_eq!(
            lookup(with(json!({"key": "Only english"}))),
            json!("Only English")
        );
        assert_eq!(
            lookup(with(json!({"key": "Missing key"}))),
            json!("Missing key")
        );
        assert_eq!(lookup(with(json!({"key": "Blank"}))), json!("Blank"));
        assert_eq!(
            lookup(with(
                json!({"key": "Hello {name}", "values": {"name": "Ada"}})
            )),
            json!("Hej Ada")
        );
        assert_eq!(
            lookup(json!({"catalog_dir": "catalogs", "key": "Saved", "language": "da_DK.UTF-8"})),
            json!("Gemt")
        );
        assert_eq!(
            lookup(json!({"catalog_dir": "catalogs", "key": "Saved"})),
            json!("Saved")
        );
    }

    #[test]
    fn i18n_t_fills_scalar_values_verbatim_and_refuses_structured_ones() {
        let dir = tempfile::tempdir().unwrap();
        let catalogs = dir.path().join("catalogs");
        fs::create_dir(&catalogs).unwrap();
        write_catalog(
            &catalogs,
            "da.json",
            r#"{"messages":{"":{"Level {n} {on}":"Niveau {n} {on}"}}}"#,
        );
        let text = run(
            "i18n.t",
            json!({"catalog_dir": "catalogs", "key": "Level {n} {on}", "language": "da",
                   "values": {"n": 13.1, "on": true}}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(text, json!("Niveau 13.1 true"));
        assert!(run(
            "i18n.t",
            json!({"catalog_dir": "catalogs", "key": "x", "values": {"n": {"a": 1}}}),
            dir.path(),
        )
        .is_err());
    }

    #[test]
    fn i18n_t_selects_plural_forms_by_the_language_rule() {
        let dir = tempfile::tempdir().unwrap();
        let catalogs = dir.path().join("catalogs");
        fs::create_dir(&catalogs).unwrap();
        write_catalog(
            &catalogs,
            "en.json",
            r#"{"messages":{"":{"{count} file":["{count} file","{count} files"]}}}"#,
        );
        write_catalog(
            &catalogs,
            "ru.json",
            r#"{"language":"ru","plural_forms":"nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);","messages":{"":{"{count} file":["{count} файл","{count} файла","{count} файлов"]}}}"#,
        );
        let plural = |language: &str, count: u64| {
            run(
                "i18n.t",
                json!({"catalog_dir": "catalogs", "key": "{count} file",
                       "plural_key": "{count} files", "count": count,
                       "language": language, "values": {"count": count}}),
                dir.path(),
            )
            .unwrap()
        };
        assert_eq!(plural("en", 1), json!("1 file"));
        assert_eq!(plural("en", 5), json!("5 files"));
        assert_eq!(plural("ru", 21), json!("21 файл"));
        assert_eq!(plural("ru", 22), json!("22 файла"));
        assert_eq!(plural("ru", 11), json!("11 файлов"));
        // No catalog translates the plural key, so the English source is used.
        assert_eq!(
            run(
                "i18n.t",
                json!({"catalog_dir": "catalogs", "key": "{count} box",
                       "plural_key": "{count} boxes", "count": 3, "values": {"count": 3}}),
                dir.path(),
            )
            .unwrap(),
            json!("3 boxes")
        );
        // A plural entry needs a count.
        assert!(run(
            "i18n.t",
            json!({"catalog_dir": "catalogs", "key": "{count} file", "language": "ru",
                   "values": {"count": 2}}),
            dir.path(),
        )
        .is_err());
    }

    #[test]
    fn i18n_t_reads_only_locale_named_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let catalogs = dir.path().join("catalogs");
        fs::create_dir(&catalogs).unwrap();
        write_catalog(
            &catalogs,
            "da.json",
            r#"{"messages":{"":{"Saved":"Gemt"}}}"#,
        );
        // Not catalogs: a broken JSON file whose name is not a locale tag, a text
        // file, and a directory named like a catalog.
        write_catalog(&catalogs, "broken.json", "{ not json");
        write_catalog(&catalogs, "notes.txt", "{ not json");
        fs::create_dir(catalogs.join("de.json")).unwrap();
        let text = run(
            "i18n.t",
            json!({"catalog_dir": "catalogs", "key": "Saved", "language": "de"}),
            dir.path(),
        )
        .unwrap();
        assert_eq!(text, json!("Saved"));
        assert_eq!(
            run(
                "i18n.t",
                json!({"catalog_dir": "catalogs", "key": "Saved", "language": "da"}),
                dir.path(),
            )
            .unwrap(),
            json!("Gemt")
        );
    }

    #[test]
    fn i18n_t_reports_bad_catalogs_and_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let catalogs = dir.path().join("catalogs");
        fs::create_dir(&catalogs).unwrap();
        write_catalog(&catalogs, "da.json", "{ not json");
        let broken = run(
            "i18n.t",
            json!({"catalog_dir": "catalogs", "key": "Saved", "language": "da"}),
            dir.path(),
        )
        .unwrap_err();
        assert_eq!(broken.code, "invalid_catalog");
        assert!(broken.message.contains("da.json"), "{broken}");
        assert!(run(
            "i18n.t",
            json!({"catalog_dir": "missing", "key": "Saved"}),
            dir.path(),
        )
        .is_err());
        assert!(run(
            "i18n.t",
            json!({"catalog_dir": "catalogs", "key": "Saved", "plural_key": "Saved"}),
            dir.path(),
        )
        .is_err());
        assert!(run(
            "i18n.t",
            json!({"catalog_dir": "catalogs", "key": "Saved", "count": 2}),
            dir.path(),
        )
        .is_err());
        assert!(run(
            "i18n.t",
            json!({"catalog_dir": "catalogs", "key": "Saved", "count": -1, "plural_key": "x"}),
            dir.path(),
        )
        .is_err());
    }

    const DESKTOP: &str = "[Desktop Entry]\nName=Editor\nName[da]=Redigering\nName[de_DE]=Editor DE\nExec=\"/opt/My Apps/edit\" --line=%f %U\n";

    #[test]
    fn desktop_entry_get_prefers_the_localized_key_then_the_plain_one() {
        let dir = tempfile::tempdir().unwrap();
        let get = |arguments: Value| run("desktop.entry_get", arguments, dir.path()).unwrap();
        assert_eq!(
            get(json!({"text": DESKTOP, "key": "Name"})),
            json!("Editor")
        );
        assert_eq!(
            get(json!({"text": DESKTOP, "key": "Name", "locale": "da_DK.UTF-8"})),
            json!("Redigering")
        );
        assert_eq!(
            get(json!({"text": DESKTOP, "key": "Name", "locale": "de_DE"})),
            json!("Editor DE")
        );
        assert_eq!(
            get(json!({"text": DESKTOP, "key": "Name", "locale": "fr_FR"})),
            json!("Editor")
        );
        assert_eq!(get(json!({"text": DESKTOP, "key": "Comment"})), json!(null));
        assert_eq!(
            get(json!({"text": DESKTOP, "key": "Exec", "group": "Desktop Entry"})),
            json!("\"/opt/My Apps/edit\" --line=%f %U")
        );
    }

    #[test]
    fn desktop_entry_get_refuses_other_groups_and_malformed_requests() {
        let dir = tempfile::tempdir().unwrap();
        let refused = |arguments: Value| run("desktop.entry_get", arguments, dir.path()).is_err();
        assert!(refused(
            json!({"text": DESKTOP, "key": "Name", "group": "Desktop Action x"})
        ));
        assert!(refused(
            json!({"text": DESKTOP, "key": "Name[de]", "locale": "da"})
        ));
        assert!(refused(json!({"text": DESKTOP, "key": "bad key"})));
        assert!(refused(json!({"text": "Name=x\n", "key": "Name"})));
    }

    #[test]
    fn desktop_entry_set_changes_only_the_named_line() {
        let dir = tempfile::tempdir().unwrap();
        let original = "# keep me\n[Desktop Entry]\nName=Editor\nIcon=edit\n";
        let set = |text: &str, key: &str, value: &str| {
            run(
                "desktop.entry_set",
                json!({"text": text, "key": key, "value": value}),
                dir.path(),
            )
            .unwrap()
        };
        assert_eq!(
            set(original, "Name", "Writer"),
            json!({"changed": true, "text": "# keep me\n[Desktop Entry]\nName=Writer\nIcon=edit\n"})
        );
        assert_eq!(
            set(original, "Name", "Editor"),
            json!({"changed": false, "text": original})
        );
        assert_eq!(
            set(original, "Comment", "Edit files"),
            json!({"changed": true, "text": "# keep me\n[Desktop Entry]\nName=Editor\nIcon=edit\nComment=Edit files\n"})
        );
        assert!(run(
            "desktop.entry_set",
            json!({"text": original, "key": "bad key", "value": "x"}),
            dir.path(),
        )
        .is_err());
    }

    #[test]
    fn desktop_exec_argv_expands_field_codes_one_argument_each() {
        let dir = tempfile::tempdir().unwrap();
        let argv = run(
            "desktop.exec_argv",
            json!({
                "text": "\"/opt/My Apps/edit\" --line=%f %U",
                "files": ["a b.txt"],
                "urls": ["https://example.com/x"],
            }),
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            argv,
            json!([
                "/opt/My Apps/edit",
                "--line=a b.txt",
                "https://example.com/x"
            ])
        );
        assert_eq!(
            run(
                "desktop.exec_argv",
                json!({"text": "tool 100%%"}),
                dir.path()
            )
            .unwrap(),
            json!(["tool", "100%"])
        );
        assert!(run(
            "desktop.exec_argv",
            json!({"text": "tool %f", "files": ["a", "b"]}),
            dir.path(),
        )
        .is_err());
        assert!(run(
            "desktop.exec_argv",
            json!({"text": "tool \"unterminated"}),
            dir.path(),
        )
        .is_err());
    }

    #[test]
    fn lua_encode_and_decode_round_trip_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let value = json!({
            "name": "demo",
            "port": 8080,
            "enabled": true,
            "ratio": 1.5,
            "hosts": ["a", "b\n\"c\""],
        });
        let text = run("lua.encode", json!({"value": value.clone()}), dir.path()).unwrap();
        let source = text.as_str().unwrap().to_string();
        assert_eq!(
            run("lua.decode", json!({"text": source}), dir.path()).unwrap(),
            value
        );
    }

    #[test]
    fn lua_decode_reads_the_subset_and_refuses_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run(
                "lua.decode",
                json!({"text": "{ port = 8080, [\"mode\"] = \"tile\" }"}),
                dir.path(),
            )
            .unwrap(),
            json!({"port": 8080, "mode": "tile"})
        );
        assert!(run("lua.decode", json!({"text": "os.execute('x')"}), dir.path()).is_err());
        assert!(run("lua.decode", json!({"text": "{ 1, x = 2 }"}), dir.path()).is_err());
        assert!(run("lua.decode", json!({"text": "[[long]]"}), dir.path()).is_err());
    }

    #[test]
    fn lua_encode_refuses_a_missing_value_and_excessive_depth() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run("lua.encode", json!({}), dir.path()).is_err());
        let mut deep = json!(1);
        for _ in 0..200 {
            deep = json!([deep]);
        }
        assert!(run("lua.encode", json!({"value": deep}), dir.path()).is_err());
    }
}
