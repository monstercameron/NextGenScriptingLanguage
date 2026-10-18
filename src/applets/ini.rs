//! Strict INI subset converted to and from JSON.
//!
//! Supported: `[section]` headers, `key = value` and `key: value` entries, and
//! comment lines beginning with `;` or `#`. Keys before the first section are
//! top-level members. Values are the raw text after trimming outer whitespace:
//! quotes and inline comments are not interpreted, so every value is a string.
//! Continuation lines (leading whitespace) fail. Duplicate sections, duplicate
//! keys in one section and a section named like a top-level key all fail.
//!
//! Stringify writes top-level scalars, then one section per object member. Nested
//! sections, arrays, nulls, and text that would not survive a trim or contains a
//! line break fail instead of being changed.

use super::config::{count_value, insert_unique, syntax};
use super::text::{check_text_size, text_input};
use super::{checkpoint_work, invalid, AppletError, ExecutionContext};
use serde_json::{Map, Value};

const FORMAT: &str = "ini";

pub(super) fn parse(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    check_text_size(text.len())?;
    let text = std::str::from_utf8(crate::structured::strip_utf8_bom(text.as_bytes()))
        .map_err(|error| invalid(format!("INI input is not UTF-8: {error}")))?;
    let mut root: Map<String, Value> = Map::new();
    let mut sections: Map<String, Value> = Map::new();
    let mut current: Option<String> = None;
    let mut count = 0usize;
    for (index, raw) in text.split('\n').enumerate() {
        checkpoint_work(context, index)?;
        let line_no = index + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(syntax(
                FORMAT,
                line_no,
                "continuation lines are not supported",
            ));
        }
        if let Some(inner) = trimmed.strip_prefix('[') {
            let name = inner
                .strip_suffix(']')
                .ok_or_else(|| syntax(FORMAT, line_no, "section header is not closed"))?
                .trim();
            if name.is_empty() {
                return Err(syntax(FORMAT, line_no, "section name is empty"));
            }
            insert_unique(
                &mut sections,
                name.to_owned(),
                Value::Object(Map::new()),
                FORMAT,
                line_no,
            )?;
            current = Some(name.to_owned());
            continue;
        }
        let separator = trimmed
            .find(['=', ':'])
            .ok_or_else(|| syntax(FORMAT, line_no, "expected key = value"))?;
        let key = trimmed[..separator].trim_end();
        if key.is_empty() {
            return Err(syntax(FORMAT, line_no, "key is empty"));
        }
        let value = Value::String(trimmed[separator + 1..].trim().to_owned());
        count_value(&mut count)?;
        match &current {
            None => insert_unique(&mut root, key.to_owned(), value, FORMAT, line_no)?,
            Some(section) => {
                let Some(Value::Object(members)) = sections.get_mut(section) else {
                    return Err(syntax(FORMAT, line_no, "section is missing"));
                };
                insert_unique(members, key.to_owned(), value, FORMAT, line_no)?;
            }
        }
    }
    for (name, members) in sections {
        if root.contains_key(&name) {
            return Err(invalid(format!(
                "ini section {name:?} has the same name as a top-level key"
            )));
        }
        root.insert(name, members);
    }
    Ok(Value::Object(root))
}

pub(super) fn stringify(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let Some(Value::Object(map)) = args.get("value") else {
        return Err(invalid(
            "ini.stringify requires args.value to be a JSON object",
        ));
    };
    let mut out = String::new();
    for (key, value) in map {
        if !matches!(value, Value::Object(_)) {
            write_entry(&mut out, key, value)?;
        }
    }
    for (name, value) in map {
        let Value::Object(section) = value else {
            continue;
        };
        check_key(name)?;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push('[');
        out.push_str(name);
        out.push_str("]\n");
        for (key, member) in section {
            if matches!(member, Value::Object(_)) {
                return Err(invalid(format!(
                    "ini cannot nest sections below {name:?}; use one level of sections"
                )));
            }
            write_entry(&mut out, key, member)?;
        }
    }
    check_text_size(out.len())?;
    Ok(Value::String(out))
}

fn write_entry(out: &mut String, key: &str, value: &Value) -> Result<(), AppletError> {
    check_key(key)?;
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Null => {
            return Err(invalid(format!(
                "ini cannot represent null for key {key:?}"
            )))
        }
        Value::Array(_) | Value::Object(_) => {
            return Err(invalid(format!(
                "ini supports only scalar values; key {key:?} is a collection"
            )))
        }
    };
    if text.contains(['\n', '\r']) || text != text.trim() {
        return Err(invalid(format!(
            "ini value for key {key:?} would lose line breaks or outer whitespace"
        )));
    }
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(&text);
    out.push('\n');
    Ok(())
}

fn check_key(key: &str) -> Result<(), AppletError> {
    let forbidden = key.contains(['=', ':', '[', ']', '\n', '\r']);
    if key.is_empty()
        || forbidden
        || key != key.trim()
        || key.starts_with(';')
        || key.starts_with('#')
    {
        return Err(invalid(format!(
            "ini cannot write key {key:?}; keys must be nonempty and free of = : [ ] and line breaks"
        )));
    }
    Ok(())
}
