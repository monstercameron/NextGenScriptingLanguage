use super::{invalid, required_string, AppletError};
use serde_json::{Map, Value};

const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_MATCHES: usize = 100_000;

fn text_input<'a>(args: &'a Map<String, Value>, input: &'a [u8]) -> Result<&'a str, AppletError> {
    if let Some(value) = args.get("text") {
        return value
            .as_str()
            .ok_or_else(|| invalid("args.text must be a string"));
    }
    std::str::from_utf8(input).map_err(|error| invalid(format!("stdin must be UTF-8: {error}")))
}

pub(super) fn trim_final_lf(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    check_text_size(text.len())?;
    Ok(Value::String(
        text.strip_suffix('\n').unwrap_or(text).to_owned(),
    ))
}

pub(super) fn concat(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let values = args
        .get("parts")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("args.parts must be an array of strings"))?;
    if values.len() > MAX_MATCHES {
        return Err(invalid(format!(
            "args.parts exceeds the {MAX_MATCHES}-item limit"
        )));
    }
    let separator = match args.get("separator") {
        None => "",
        Some(Value::String(value)) => value.as_str(),
        Some(_) => return Err(invalid("args.separator must be a string")),
    };
    let mut result = String::new();
    for (index, value) in values.iter().enumerate() {
        let part = value
            .as_str()
            .ok_or_else(|| invalid(format!("args.parts[{index}] must be a string")))?;
        let separator_bytes = if index > 0 { separator.len() } else { 0 };
        if result
            .len()
            .checked_add(separator_bytes)
            .and_then(|length| length.checked_add(part.len()))
            .is_none_or(|length| length > MAX_TEXT_BYTES)
        {
            return Err(invalid(format!(
                "text.concat output exceeds the {MAX_TEXT_BYTES}-byte limit"
            )));
        }
        if index > 0 {
            result.push_str(separator);
        }
        result.push_str(part);
    }
    Ok(Value::String(result))
}

pub(super) fn matches(args: &Map<String, Value>, input: &[u8]) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    let needle = required_string(args, "needle")?;
    check_text_size(text.len())?;
    check_text_size(needle.len())?;
    let mut offsets = Vec::new();
    for (offset, _) in text.char_indices() {
        if text[offset..].starts_with(needle) {
            if offsets.len() == MAX_MATCHES {
                return Err(invalid(format!(
                    "text.matches output exceeds the {MAX_MATCHES}-match limit"
                )));
            }
            offsets.push(offset);
        }
    }
    if needle.is_empty() {
        if offsets.len() == MAX_MATCHES {
            return Err(invalid(format!(
                "text.matches output exceeds the {MAX_MATCHES}-match limit"
            )));
        }
        offsets.push(text.len());
    }
    let count = offsets.len();
    let offsets: Vec<Value> = offsets.into_iter().map(Value::from).collect();
    Ok(serde_json::json!({
        "matches": count != 0,
        "count": count,
        "byte_offsets": offsets,
    }))
}

fn check_text_size(length: usize) -> Result<(), AppletError> {
    if length > MAX_TEXT_BYTES {
        return Err(invalid(format!(
            "text exceeds the {MAX_TEXT_BYTES}-byte limit"
        )));
    }
    Ok(())
}
