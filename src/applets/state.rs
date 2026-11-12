//! The `state.*` applets: flags, toggles, small values and debounce, over the
//! named-state library in `crate::named_state`.
//!
//! The state directory is the execute state location, the same one `doctor` and
//! the what-if key use: `%LOCALAPPDATA%\execute` on Windows, otherwise
//! `$XDG_STATE_HOME/execute` (or `$HOME/.local/state/execute` when that is unset
//! or relative). The library keeps its flags, values and debounce records in
//! subdirectories of that directory, so nothing else is namespaced here.

use super::{failed, invalid, io_error, required_string, AppletError, ExecutionContext};
use crate::named_state;
use serde_json::{json, Map, Value};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) fn run(
    operation: &str,
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let state_dir = state_dir()?;
    context.checkpoint()?;
    apply(operation, args, &state_dir)
}

/// Runs one state operation against an explicit state directory. The dispatch
/// resolves that directory; tests pass a temporary one.
pub(super) fn apply(
    operation: &str,
    args: &Map<String, Value>,
    state_dir: &Path,
) -> Result<Value, AppletError> {
    let io = |result: io::Result<Value>| result.map_err(|error| state_error(error, operation));
    let name = required_string(args, "name")?;
    match operation {
        "state.flag_set" => {
            io(named_state::flag_set(state_dir, name).map(|()| json!({"set": true})))
        }
        "state.flag_clear" => {
            io(named_state::flag_clear(state_dir, name).map(|()| json!({"set": false})))
        }
        "state.flag_is_set" => io(named_state::flag_is_set(state_dir, name).map(Value::Bool)),
        "state.toggle" => io(named_state::flag_toggle(state_dir, name).map(Value::Bool)),
        "state.value_load" => io(named_state::value_read(state_dir, name).map(
            |value| match value {
                Some(text) => json!({"found": true, "value": text}),
                None => json!({"found": false, "value": null}),
            },
        )),
        "state.value_store" => {
            let text = required_string(args, "value")?;
            io(named_state::value_write(state_dir, name, text).map(|()| json!({"written": true})))
        }
        "state.debounce" => {
            let window_ms = args
                .get("window_ms")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("args.window_ms must be a nonnegative integer"))?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| failed("io_error", "the system clock is before the Unix epoch"))?;
            io(
                named_state::debounce(state_dir, name, Duration::from_millis(window_ms), now)
                    .map(Value::Bool),
            )
        }
        other => Err(invalid(format!("unknown state operation {other}"))),
    }
}

fn state_error(error: io::Error, operation: &str) -> AppletError {
    if error.kind() == io::ErrorKind::InvalidInput {
        invalid(format!("{operation}: {error}"))
    } else {
        io_error(error, operation)
    }
}

#[cfg(windows)]
fn state_dir() -> Result<PathBuf, AppletError> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            failed(
                "state_unavailable",
                "LOCALAPPDATA is not set, so the state directory has no location",
            )
        })?;
    Ok(PathBuf::from(base).join("execute"))
}

#[cfg(not(windows))]
fn state_dir() -> Result<PathBuf, AppletError> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(path) if Path::new(&path).is_absolute() => PathBuf::from(path),
        _ => {
            let home = std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    failed(
                        "state_unavailable",
                        "HOME is not set, so the state directory has no location",
                    )
                })?;
            PathBuf::from(home).join(".local").join("state")
        }
    };
    Ok(base.join("execute"))
}
