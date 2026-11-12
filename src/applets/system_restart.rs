//! `system.restart_required`: the applet wrapper over `crate::restart_required::assess`.
//!
//! The library reads marker files, kernel module trees and `/proc` links below
//! `root` and executes nothing. The optional `root` argument (absolute, default
//! `/`) is shared with `sys.facts`.

use super::sys_facts::root_argument;
use super::{failed, AppletError, ExecutionContext};
use serde_json::{Map, Value};

pub(super) fn assess(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let root = root_argument(args)?;
    let assessment = crate::restart_required::assess(&root);
    context.checkpoint()?;
    serde_json::to_value(assessment).map_err(|error| {
        failed(
            "io_error",
            format!("encoding system.restart_required: {error}"),
        )
    })
}
