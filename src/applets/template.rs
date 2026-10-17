//! `text.render`: placeholder substitution for configuration templates.
//!
//! The only syntax is `{{ name }}` (spaces or tabs inside the braces are
//! optional), where `name` is one to 128 ASCII letters, digits, `_`, `-` or `.`.
//! Each name is looked up as an exact key in the `values` JSON object. A missing
//! key, an invalid or unterminated placeholder, or a value that is not text,
//! number or boolean fails. Substituted text is inserted verbatim and is never
//! scanned again. A literal `{{` is written `{{{{`. A `}}` outside a placeholder
//! is literal text. There are no conditions, loops or expressions, and the
//! output keeps every byte of the template that is not part of a placeholder.

use super::{checkpoint_work, invalid, io_error, AppletError, ExecutionContext};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::io::Read;
use std::path::Path;

const MAX_TEMPLATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_NAME_BYTES: usize = 128;

pub(super) fn render(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let template: Cow<'_, str> = match (args.get("template"), args.get("template_path")) {
        (Some(Value::String(text)), None) => Cow::Borrowed(text.as_str()),
        (None, Some(Value::String(path))) => Cow::Owned(read_template(path, cwd)?),
        _ => {
            return Err(invalid(
                "text.render requires exactly one of args.template or args.template_path",
            ))
        }
    };
    let values = args
        .get("values")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("args.values must be a JSON object"))?;

    let mut output = String::new();
    let mut rest: &str = &template;
    let mut work = 0usize;
    while let Some(start) = rest.find("{{") {
        checkpoint_work(context, work)?;
        work += 1;
        let (before, from_brace) = rest.split_at(start);
        output.push_str(before);
        if let Some(tail) = from_brace.strip_prefix("{{{{") {
            output.push_str("{{");
            rest = tail;
        } else {
            let body = &from_brace[2..];
            let close = body
                .find("}}")
                .ok_or_else(|| invalid("text.render has an unterminated {{ placeholder"))?;
            let name = body[..close].trim_matches([' ', '\t']);
            if !valid_name(name) {
                return Err(invalid(format!(
                    "text.render placeholder name {name:?} must be 1 to {MAX_NAME_BYTES} ASCII letters, digits, '_', '-' or '.'"
                )));
            }
            let value = values.get(name).ok_or_else(|| {
                invalid(format!("text.render has no value for placeholder {name:?}"))
            })?;
            push_value(&mut output, name, value)?;
            rest = &body[close + 2..];
        }
        check_size(output.len() + rest.len())?;
    }
    output.push_str(rest);
    check_size(output.len())?;
    Ok(Value::String(output))
}

fn read_template(path: &str, cwd: &Path) -> Result<String, AppletError> {
    if path.is_empty() || path.contains('\0') {
        return Err(invalid(
            "args.template_path must be nonempty and contain no NUL",
        ));
    }
    let resolved = super::filesystem::resolve_path(path, cwd);
    let file = std::fs::File::open(&resolved)
        .map_err(|error| io_error(error, format!("opening {}", resolved.display())))?;
    let mut bytes = Vec::new();
    file.take(MAX_TEMPLATE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(error, format!("reading {}", resolved.display())))?;
    if bytes.len() > MAX_TEMPLATE_BYTES {
        return Err(invalid(format!(
            "template exceeds the {MAX_TEMPLATE_BYTES}-byte limit"
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| invalid(format!("template is not UTF-8 text: {error}")))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn push_value(output: &mut String, name: &str, value: &Value) -> Result<(), AppletError> {
    match value {
        Value::String(text) => output.push_str(text),
        Value::Bool(flag) => output.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => output.push_str(&number.to_string()),
        _ => {
            return Err(invalid(format!(
                "text.render placeholder {name:?} must be text, a number or a boolean"
            )))
        }
    }
    check_size(output.len())
}

fn check_size(length: usize) -> Result<(), AppletError> {
    if length > MAX_TEMPLATE_BYTES {
        return Err(invalid(format!(
            "text.render output exceeds the {MAX_TEMPLATE_BYTES}-byte limit"
        )));
    }
    Ok(())
}
