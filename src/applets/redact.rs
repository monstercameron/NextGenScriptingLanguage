//! `redact.text`: the applet wrapper over the pure engine in `crate::redact`.
//!
//! The engine reads no files, opens no sockets and keeps no state, so this
//! applet declares no effects. Matched values never leave the engine: the
//! reply carries the redacted text and a count per category.

use super::{invalid, AppletError};
use serde_json::{json, Map, Value};

pub(super) fn text(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let input = super::required_string(args, "text")?;
    let owned: Vec<&str> = match args.get("literals") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .ok_or_else(|| invalid("args.literals must be a list of strings"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(invalid("args.literals must be a list of strings")),
    };
    let require_clean = super::optional_bool(args, "require_clean", false)?;
    let redaction = crate::redact::redact_text(input, &owned)
        .map_err(|error| invalid(format!("args.literals: {error}")))?;
    // The second pass runs over the redacted text, so a value that survives
    // masking (for example inside a placeholder) fails the step. The message
    // never repeats the input or the matched value.
    if require_clean && !crate::redact::require_clean(&redaction.text, &owned) {
        return Err(super::failed(
            "not_clean",
            "redacted text still contains a detectable value; the step refuses to pass it on",
        ));
    }
    let counts = redaction.counts;
    Ok(json!({
        "text": redaction.text,
        "counts": {
            "literal": counts.literal,
            "token": counts.token,
            "email": counts.email,
            "ipv4": counts.ipv4,
        },
    }))
}
