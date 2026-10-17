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

const MAX_PATH_BYTES: usize = 16 * 1024 * 1024;
const MAX_PATH_PARTS: usize = 4096;

/// Join path segments with the platform separator, without normalizing them.
/// Empty segments are skipped. As with `Path::join`, an absolute segment
/// replaces everything before it, and no parts give an empty path. Results use
/// the platform separator, so Windows output contains backslashes.
pub(super) fn join(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let parts = args
        .get("parts")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("args.parts must be an array of strings"))?;
    if parts.len() > MAX_PATH_PARTS {
        return Err(invalid(format!(
            "args.parts exceeds the {MAX_PATH_PARTS}-item limit"
        )));
    }
    let mut path = std::path::PathBuf::new();
    for (index, value) in parts.iter().enumerate() {
        let part = value
            .as_str()
            .ok_or_else(|| invalid(format!("args.parts[{index}] must be a string")))?;
        if part.contains('\0') {
            return Err(invalid(format!("args.parts[{index}] must not contain NUL")));
        }
        if part.is_empty() {
            continue;
        }
        path.push(part);
        if path.as_os_str().len() > MAX_PATH_BYTES {
            return Err(invalid(format!(
                "path.join output exceeds the {MAX_PATH_BYTES}-byte limit"
            )));
        }
    }
    let text = path
        .to_str()
        .ok_or_else(|| failed("invalid_request", "path.join produced a non-UTF-8 path"))?;
    Ok(Value::String(text.to_owned()))
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
