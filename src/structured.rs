//! Small, explicit JSON boundary for the byte-oriented runtime.
use crate::ast::{InputDeclaration, InputType, Value};
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::{collections::BTreeMap, io::Write};

pub(crate) fn from_json(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(value) => match value.as_i64() {
            Some(integer) => Value::Integer(integer),
            None => Value::Number(value),
        },
        serde_json::Value::String(value) => Value::String(value),
        serde_json::Value::Array(values) => {
            Value::List(values.into_iter().map(from_json).collect())
        }
        serde_json::Value::Object(values) => Value::Record(
            values
                .into_iter()
                .map(|(key, value)| (key, from_json(value)))
                .collect(),
        ),
    }
}

/// Serialize directly from the typed tree; no second JSON tree or intermediate string.
struct Plain<'a>(&'a Value);
impl Serialize for Plain<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self.0 {
            Value::Null => serializer.serialize_none(),
            Value::String(value) => serializer.serialize_str(value),
            Value::Integer(value) => serializer.serialize_i64(*value),
            Value::Number(value) => value.serialize(serializer),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::List(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(&Plain(value))?;
                }
                seq.end()
            }
            Value::Record(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(key, &Plain(value))?;
                }
                map.end()
            }
            Value::Reference(_) => Err(serde::ser::Error::custom(
                "cannot encode an unresolved reference",
            )),
        }
    }
}

pub(crate) fn to_json(value: &Value) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(Plain(value))?)
}

pub(crate) fn literal(value: &Value) -> bool {
    match value {
        Value::Reference(_) => false,
        Value::List(values) => values.iter().all(literal),
        Value::Record(values) => values.values().all(literal),
        _ => true,
    }
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes)
        .map(from_json)
        .context("parsing JSON")
}

pub(crate) fn validate_pointer(pointer: &str) -> Result<()> {
    if !pointer.is_empty() && !pointer.starts_with('/') {
        bail!("JSON pointer must be empty or start with / (for example /items/0/name)");
    }
    let mut chars = pointer.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
            bail!("JSON pointer escape must be ~0 or ~1");
        }
    }
    Ok(())
}

pub(crate) fn take<'a>(
    pointer: &str,
    value: &'a Value,
    kind: InputType,
    line: usize,
) -> Result<&'a Value> {
    validate_pointer(pointer)?;
    let mut selected = value;
    if !pointer.is_empty() {
        for part in pointer[1..].split('/') {
            let key = part.replace("~1", "/").replace("~0", "~");
            selected = match selected {
                Value::Record(values) => values.get(&key),
                Value::List(values) => {
                    if key.is_empty()
                        || (key.len() > 1 && key.starts_with('0'))
                        || !key.bytes().all(|b| b.is_ascii_digit())
                    {
                        None
                    } else {
                        key.parse::<usize>()
                            .ok()
                            .and_then(|index| values.get(index))
                    }
                }
                _ => None,
            }
            .ok_or_else(|| anyhow!("JSON pointer {pointer:?} does not exist"))?;
        }
    }
    InputDeclaration {
        name: format!("JSON pointer {pointer}"),
        kind,
        default: None,
        source: None,
        line,
        description: None,
    }
    .validate_value(selected)?;
    Ok(selected)
}

pub(crate) fn source<'a>(
    value: &'a Value,
    bindings: &'a BTreeMap<String, Value>,
) -> Result<&'a Value> {
    match value {
        Value::Reference(name) => bindings
            .get(name)
            .ok_or_else(|| anyhow!("undefined result {name:?}")),
        value => Ok(value),
    }
}

pub(crate) fn encode(value: &Value, limit: usize) -> Result<Vec<u8>> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other(
                    "encoded JSON exceeds maximum output bytes",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut output, &Plain(value)).context("encoding JSON")?;
    Ok(output.bytes)
}
