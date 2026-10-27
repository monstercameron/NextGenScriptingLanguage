use super::{failed, invalid, required_string, AppletError};
use serde_json::{Map, Value};

pub(super) fn dirname(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let value = required_string(args, "path")?;
    Ok(Value::String(posix_dirname(value).to_owned()))
}

fn posix_dirname(value: &str) -> &str {
    if value.is_empty() {
        return ".";
    }
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    match trimmed.rfind('/') {
        None => ".",
        Some(0) => "/",
        Some(index) => trimmed[..index].trim_end_matches('/'),
    }
}

pub(super) fn basename(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let value = required_string(args, "path")?;
    let suffix = match args.get("suffix") {
        None => None,
        Some(Value::String(suffix)) => Some(suffix.as_str()),
        Some(_) => return Err(invalid("args.suffix must be a string")),
    };
    Ok(Value::String(posix_basename(value, suffix).to_owned()))
}

fn posix_basename<'a>(value: &'a str, suffix: Option<&str>) -> &'a str {
    if value.is_empty() {
        return value;
    }
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    match suffix
        .filter(|suffix| !suffix.is_empty())
        .and_then(|suffix| name.strip_suffix(suffix))
    {
        Some(remaining) if !remaining.is_empty() => remaining,
        _ => name,
    }
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
