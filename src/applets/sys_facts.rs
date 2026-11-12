//! `sys.facts`: the applet wrapper over `crate::sysfacts::facts`.
//!
//! The library reads system files only below `root` and executes nothing. The
//! optional `root` argument (absolute, default `/`) lets tests and chroots stand
//! in for the live system.

use super::{failed, invalid, AppletError, ExecutionContext};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The optional `root` argument: an absolute directory, `/` when absent.
pub(super) fn root_argument(args: &Map<String, Value>) -> Result<PathBuf, AppletError> {
    match args.get("root") {
        None => Ok(PathBuf::from("/")),
        Some(Value::String(root)) if Path::new(root).is_absolute() => Ok(PathBuf::from(root)),
        Some(_) => Err(invalid("args.root must be an absolute path")),
    }
}

pub(super) fn facts(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let root = root_argument(args)?;
    let facts = crate::sysfacts::facts(&root);
    context.checkpoint()?;
    serde_json::to_value(facts)
        .map_err(|error| failed("io_error", format!("encoding sys.facts: {error}")))
}
