use super::{failed, invalid, required_string, AppletError};
use serde_json::{Map, Value};
use std::path::Path;

pub(super) fn dirname(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let value = required_string(args, "path")?;
    let path = Path::new(value);
    let parent = path
        .parent()
        .and_then(Path::to_str)
        .filter(|parent| !parent.is_empty())
        .unwrap_or(".");
    Ok(Value::String(parent.to_owned()))
}

pub(super) fn basename(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let value = required_string(args, "path")?;
    let suffixless = match args.get("suffix") {
        None => value,
        Some(Value::String(suffix)) => value.strip_suffix(suffix).unwrap_or(value),
        Some(_) => return Err(invalid("args.suffix must be a string")),
    };
    let name = Path::new(suffixless)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    Ok(Value::String(name.to_owned()))
}

pub(super) fn remove_suffix(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let value = required_string(args, "path")?;
    let suffix = required_string(args, "suffix")?;
    if suffix.is_empty() {
        return Err(failed("invalid_request", "args.suffix cannot be empty"));
    }
    Ok(Value::String(
        value.strip_suffix(suffix).unwrap_or(value).to_owned(),
    ))
}
