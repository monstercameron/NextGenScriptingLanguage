//! Small, reusable operations for `.program` workflows.
//!
//! These functions intentionally contain no application-specific policy. The
//! `.program` workflow owns sequencing, retries, and decisions about which
//! operation to call.

mod aggregation;
mod arithmetic;
mod config;
mod csv;
mod ensure;
mod env_file;
mod file_test;
mod filesystem;
mod glob;
mod hash;
mod ini;
// #357: nothing calls the encoder until the journald socket send (NEEDS) lands.
#[allow(dead_code)]
mod journal;
mod jsonc;
mod list;
mod path;
mod redact;
mod shared;
mod structured;
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
        "fs.ensure_file" => ensure::ensure_file(args, cwd, context),
        "fs.ensure_line" => ensure::ensure_line(args, cwd, context),
        "conf.set" => ensure::conf_set(args, cwd, context),
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

    fn run(operation: &str, args: Value, cwd: &Path) -> Result<Value, super::AppletError> {
        let map: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
        execute_in_cwd(operation, &map, b"", cwd)
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
}
