//! `desktop.entry_get`, `desktop.entry_set` and `desktop.exec_argv`: the applet
//! wrappers over the pure Desktop Entry engine in `crate::desktop_entry`.
//!
//! The engine reads and writes text only. These applets take the file content as
//! `text` and never touch the filesystem: the caller reads with `fs.read` and writes
//! the result with `fs.write_atomic`. The library reads and writes only the
//! `Desktop Entry` group, so `group` may name nothing else.

use super::{invalid, required_string, AppletError, ExecutionContext};
use crate::desktop_entry::{self, FieldContext, DESKTOP_ENTRY_GROUP};
use serde_json::{json, Map, Value};

/// Reads one key, unescaped, or returns null when the key is absent. With `locale`,
/// the localized forms `Key[lang_COUNTRY@MODIFIER]`, `Key[lang_COUNTRY]`,
/// `Key[lang@MODIFIER]` and `Key[lang]` are tried first, in that order, before the
/// plain key, as the Desktop Entry specification requires.
pub(super) fn entry_get(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = required_string(args, "text")?;
    let key = required_key(args)?;
    check_group(args)?;
    let localized = match args.get("locale") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(locale)) => localized_keys(key, locale)?,
        Some(_) => return Err(invalid("args.locale must be a string")),
    };
    for candidate in &localized {
        if let Some(value) = desktop_entry::get_string(text, candidate).map_err(entry_error)? {
            return Ok(Value::String(value));
        }
    }
    let value = desktop_entry::get_string(text, key).map_err(entry_error)?;
    context.checkpoint()?;
    Ok(value.map_or(Value::Null, Value::String))
}

/// Sets one key to `value` in the Desktop Entry group. The reply holds the new text,
/// or the original text unchanged when the key already had that value.
pub(super) fn entry_set(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let text = required_string(args, "text")?;
    let key = required_key(args)?;
    let value = required_string(args, "value")?;
    let reply = match desktop_entry::set_string(text, key, value).map_err(entry_error)? {
        Some(updated) => json!({"changed": true, "text": updated}),
        None => json!({"changed": false, "text": text}),
    };
    context.checkpoint()?;
    Ok(reply)
}

/// Parses an unescaped Exec value (what `desktop.entry_get` returns) and expands its
/// field codes from the `files` and `urls` lists. `%i`, `%c` and `%k` expand to
/// nothing because this applet takes no icon, name or desktop-file path.
pub(super) fn exec_argv(
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let exec = required_string(args, "text")?;
    let files = string_list(args, "files")?;
    let urls = string_list(args, "urls")?;
    let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
    let url_refs: Vec<&str> = urls.iter().map(String::as_str).collect();
    let field_context = FieldContext {
        files: &file_refs,
        urls: &url_refs,
        ..FieldContext::default()
    };
    let parsed = desktop_entry::parse_exec(exec).map_err(entry_error)?;
    let argv = desktop_entry::expand_exec(&parsed, &field_context).map_err(entry_error)?;
    context.checkpoint()?;
    Ok(json!(argv))
}

fn entry_error(error: desktop_entry::DesktopError) -> AppletError {
    invalid(format!("desktop entry: {error}"))
}

fn required_key(args: &Map<String, Value>) -> Result<&str, AppletError> {
    let key = required_string(args, "key")?;
    if desktop_entry::is_valid_key(key) {
        Ok(key)
    } else {
        Err(invalid(format!("args.key {key:?} is not a valid key name")))
    }
}

/// Only the `Desktop Entry` group is read or written, so any other group is refused
/// rather than silently read from the wrong place.
fn check_group(args: &Map<String, Value>) -> Result<(), AppletError> {
    match args.get("group") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(group)) if group == DESKTOP_ENTRY_GROUP => Ok(()),
        Some(_) => Err(invalid(format!(
            "args.group must be {DESKTOP_ENTRY_GROUP:?}; no other group is read yet"
        ))),
    }
}

/// The localized keys to try for `locale`, most specific first. The encoding part
/// (after `.`) is dropped, as the specification says it must be.
fn localized_keys(key: &str, locale: &str) -> Result<Vec<String>, AppletError> {
    if key.contains('[') {
        return Err(invalid(
            "args.key must be a plain key when args.locale is given",
        ));
    }
    let (without_modifier, modifier) = match locale.split_once('@') {
        Some((base, modifier)) => (base, Some(modifier)),
        None => (locale, None),
    };
    let base = without_modifier.split('.').next().unwrap_or_default();
    let (language, country) = match base.split_once('_') {
        Some((language, country)) => (language, Some(country)),
        None => (base, None),
    };
    let mut suffixes: Vec<String> = Vec::new();
    if let (Some(country), Some(modifier)) = (country, modifier) {
        suffixes.push(format!("{language}_{country}@{modifier}"));
    }
    if let Some(country) = country {
        suffixes.push(format!("{language}_{country}"));
    }
    if let Some(modifier) = modifier {
        suffixes.push(format!("{language}@{modifier}"));
    }
    suffixes.push(language.to_string());
    suffixes
        .into_iter()
        .filter(|suffix| !suffix.is_empty())
        .map(|suffix| {
            let candidate = format!("{key}[{suffix}]");
            if desktop_entry::is_valid_key(&candidate) {
                Ok(candidate)
            } else {
                Err(invalid(format!(
                    "args.locale {locale:?} is not a valid locale"
                )))
            }
        })
        .collect()
}

/// An optional list of strings. Absent or null is an empty list.
fn string_list(args: &Map<String, Value>, name: &str) -> Result<Vec<String>, AppletError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| invalid(format!("args.{name} must be a list of strings")))
            })
            .collect(),
        Some(_) => Err(invalid(format!("args.{name} must be a list of strings"))),
    }
}
