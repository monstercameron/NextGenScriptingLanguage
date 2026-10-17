//! Shared limits and helpers for the strict YAML, TOML and INI subsets.
//!
//! These parsers never guess. Every unsupported construct fails with the format
//! name and a line number, duplicate keys fail, and the input and nesting are
//! bounded. No crate is involved: the subsets are small enough to implement
//! directly, which keeps the interpreter's dependency set unchanged.

use super::{invalid, AppletError};
use serde_json::{Map, Value};
use std::fmt::Display;

pub(super) const MAX_CONFIG_DEPTH: usize = 64;
pub(super) const MAX_CONFIG_VALUES: usize = 1_000_000;

/// A syntax error for one configuration format, located by one-based line.
pub(super) fn syntax(format: &str, line: usize, message: impl Display) -> AppletError {
    invalid(format!("{format} line {line}: {message}"))
}

/// Insert a key once. A repeated key in one mapping is an error, even when the
/// two values are equal.
pub(super) fn insert_unique(
    map: &mut Map<String, Value>,
    key: String,
    value: Value,
    format: &str,
    line: usize,
) -> Result<(), AppletError> {
    if map.contains_key(&key) {
        return Err(syntax(format, line, format!("duplicate key {key:?}")));
    }
    map.insert(key, value);
    Ok(())
}

pub(super) fn count_value(count: &mut usize) -> Result<(), AppletError> {
    *count += 1;
    if *count > MAX_CONFIG_VALUES {
        return Err(invalid(format!(
            "configuration exceeds the {MAX_CONFIG_VALUES}-value limit"
        )));
    }
    Ok(())
}

pub(super) fn check_depth(depth: usize, format: &str, line: usize) -> Result<(), AppletError> {
    if depth > MAX_CONFIG_DEPTH {
        return Err(syntax(
            format,
            line,
            format!("nesting exceeds {MAX_CONFIG_DEPTH} levels"),
        ));
    }
    Ok(())
}

/// Reject a value that cannot be represented in the target format's scalar
/// space, such as a non-finite float or an integer outside 64-bit range.
pub(super) fn integer_in_range(value: i128, format: &str, line: usize) -> Result<i64, AppletError> {
    i64::try_from(value)
        .map_err(|_| syntax(format, line, "integer is outside the signed 64-bit range"))
}
