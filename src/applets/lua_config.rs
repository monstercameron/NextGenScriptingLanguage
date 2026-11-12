//! `lua.encode` and `lua.decode`: the applet wrappers over the pure Lua literal
//! engine in `crate::lua_config`.
//!
//! Both are text transforms between a JSON value and the Lua literal subset. Nothing
//! is executed and nothing is read or written; the caller reads and writes the text.

use super::{invalid, required_string, AppletError, ExecutionContext};
use serde_json::{Map, Value};

/// Writes `value` as one Lua literal in the subset.
pub(super) fn encode(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let value = args
        .get("value")
        .ok_or_else(|| invalid("args.value is required"))?;
    let text = crate::lua_config::encode(value)
        .map_err(|error| invalid(format!("lua.encode: {error}")))?;
    context.checkpoint()?;
    Ok(Value::String(text))
}

/// Reads one Lua literal in the subset back into a JSON value. Anything outside the
/// subset is refused with its line and column.
pub(super) fn decode(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = required_string(args, "text")?;
    let value =
        crate::lua_config::decode(text).map_err(|error| invalid(format!("lua.decode: {error}")))?;
    context.checkpoint()?;
    Ok(value)
}
