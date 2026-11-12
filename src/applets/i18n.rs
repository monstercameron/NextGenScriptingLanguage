//! `i18n.t`: the applet wrapper over the catalog engine in `crate::i18n`.
//!
//! The applet reads `<locale>.json` catalogs from `catalog_dir`. It reads only
//! regular files directly inside that directory whose names are a locale tag plus
//! `.json`, and only the catalogs on the resolved fallback chain (requested
//! language, its base language, then `en`). Each read is bounded by the library's
//! catalog size limit. Nothing is executed and nothing is written.

use super::{failed, invalid, io_error, required_string, AppletError, ExecutionContext};
use crate::i18n::{self, Catalog};
use serde_json::{Map, Value};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// The fallback language every lookup ends with, after the requested chain.
const DEFAULT_LANGUAGE: &str = "en";

pub(super) fn translate(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let directory = super::filesystem::resolve_path(required_string(args, "catalog_dir")?, cwd);
    let key = required_string(args, "key")?;
    let language = match args.get("language") {
        None | Some(Value::Null) => "",
        Some(Value::String(tag)) => tag.as_str(),
        Some(_) => return Err(invalid("args.language must be a locale tag string")),
    };
    let context_name = match args.get("context") {
        None | Some(Value::Null) => "",
        Some(Value::String(name)) => name.as_str(),
        Some(_) => return Err(invalid("args.context must be a string")),
    };
    let plural = plural_arguments(args)?;
    let values = scalar_values(args)?;

    let available = locale_catalogs(&directory)?;
    let tags: Vec<&str> = available.iter().map(|(tag, _)| tag.as_str()).collect();
    let chain_indexes = i18n::resolve_catalogs(language, &tags, DEFAULT_LANGUAGE);
    let mut catalogs: Vec<Catalog> = Vec::with_capacity(chain_indexes.len());
    for index in chain_indexes {
        context.checkpoint()?;
        let Some((_, file_name)) = available.get(index) else {
            continue;
        };
        catalogs.push(load_catalog(&directory.join(file_name), file_name)?);
    }
    context.checkpoint()?;

    let chain: Vec<&Catalog> = catalogs.iter().collect();
    let plural_view = plural.as_ref().map(|(text, count)| (text.as_str(), *count));
    let value_pairs: Vec<(&str, &str)> = values
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let text = i18n::message(&chain, context_name, key, plural_view, &value_pairs)
        .map_err(|error| invalid(format!("i18n.t: {error}")))?;
    context.checkpoint()?;
    Ok(Value::String(text))
}

/// The optional `plural_key` and `count` pair. Either both are present or neither.
fn plural_arguments(args: &Map<String, Value>) -> Result<Option<(String, u64)>, AppletError> {
    match (args.get("plural_key"), args.get("count")) {
        (None | Some(Value::Null), None | Some(Value::Null)) => Ok(None),
        (Some(Value::String(plural_key)), Some(count)) => {
            let count = count
                .as_u64()
                .ok_or_else(|| invalid("args.count must be a nonnegative integer"))?;
            Ok(Some((plural_key.clone(), count)))
        }
        _ => Err(invalid(
            "args.plural_key and args.count must be given together, plural_key as a string",
        )),
    }
}

/// The optional `values` record. Each value is a string, number or boolean.
fn scalar_values(args: &Map<String, Value>) -> Result<Vec<(String, String)>, AppletError> {
    let object = match args.get("values") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(object)) => object,
        Some(_) => return Err(invalid("args.values must be a record of scalar values")),
    };
    object
        .iter()
        .map(|(name, value)| match value {
            Value::String(text) => Ok((name.clone(), text.clone())),
            Value::Number(number) => Ok((name.clone(), number.to_string())),
            Value::Bool(flag) => Ok((name.clone(), flag.to_string())),
            _ => Err(invalid(format!(
                "args.values.{name} must be a string, number or boolean"
            ))),
        })
        .collect()
}

/// Lists the catalogs in `directory` as `(locale tag, file name)` pairs, sorted by
/// file name. Only regular files named `<locale tag>.json` qualify; anything else
/// in the directory is ignored and never opened.
fn locale_catalogs(directory: &Path) -> Result<Vec<(String, String)>, AppletError> {
    let entries = std::fs::read_dir(directory)
        .map_err(|error| io_error(error, format!("reading {}", directory.display())))?;
    let mut found = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| io_error(error, format!("reading {}", directory.display())))?;
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(tag) = file_name.strip_suffix(".json") else {
            continue;
        };
        if !is_locale_stem(tag) {
            continue;
        }
        let is_regular = entry.file_type().is_ok_and(|kind| kind.is_file());
        if is_regular {
            found.push((tag.to_string(), file_name));
        }
    }
    found.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(found)
}

/// A file-name stem that is a locale tag: ASCII letters, digits, `_` and `-`, and a
/// tag the library can normalise. A path separator, a dot or a modifier never passes.
fn is_locale_stem(stem: &str) -> bool {
    !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && i18n::normalise_locale(stem).is_some()
}

/// Reads and parses one catalog. The read stops one byte past the library limit, so
/// an oversized file fails without being loaded whole.
fn load_catalog(path: &Path, file_name: &str) -> Result<Catalog, AppletError> {
    let file = File::open(path).map_err(|error| io_error(error, format!("opening {file_name}")))?;
    let limit = i18n::MAX_CATALOG_BYTES;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(error, format!("reading {file_name}")))?;
    if bytes.len() > limit {
        return Err(failed(
            "invalid_catalog",
            format!("{file_name}: catalog is larger than {limit} bytes"),
        ));
    }
    let source = String::from_utf8(bytes)
        .map_err(|error| failed("invalid_catalog", format!("{file_name}: {error}")))?;
    i18n::parse_catalog(&source)
        .map_err(|error| failed("invalid_catalog", format!("{file_name}: {error}")))
}
