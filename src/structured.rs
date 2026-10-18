//! Small, explicit JSON boundary for the byte-oriented runtime.
use crate::ast::{InputDeclaration, InputType, Value};
use anyhow::{anyhow, bail, Context, Result};
use serde::{
    de::{MapAccess, SeqAccess, Visitor},
    Deserialize, Serialize,
};
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

#[cfg(test)]
fn old_parse(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes)
        .map(from_json)
        .context("parsing JSON")
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
    serde_json::from_slice::<JsonDocumentValue>(strip_utf8_bom(bytes))
        .map(|value| value.0)
        .context("parsing JSON")
}

/// Remove one UTF-8 byte-order mark from the beginning of a text document.
pub(crate) fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
}

/// Parse applet JSON directly into `serde_json::Value`, rejecting duplicate
/// object keys without an intermediate interpreter value.
pub(crate) fn parse_json_value(bytes: &[u8]) -> Result<serde_json::Value> {
    serde_json::from_slice::<JsonValueDocument>(strip_utf8_bom(bytes))
        .map(|document| document.0)
        .context("parsing JSON")
}

struct JsonValueDocument(serde_json::Value);

impl<'de> Deserialize<'de> for JsonValueDocument {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = JsonValueDocument;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(serde_json::Value::Null))
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_unit()
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(serde_json::Value::Number)
                    .map(JsonValueDocument)
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonValueDocument(value.into()))
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(value) = sequence.next_element::<JsonValueDocument>()? {
                    values.push(value.0);
                }
                Ok(JsonValueDocument(values.into()))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, JsonValueDocument>()? {
                    match values.entry(key) {
                        serde_json::map::Entry::Vacant(entry) => {
                            entry.insert(value.0);
                        }
                        serde_json::map::Entry::Occupied(entry) => {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate key {:?}",
                                entry.key()
                            )));
                        }
                    }
                }
                Ok(JsonValueDocument(values.into()))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

struct JsonDocumentValue(Value);

impl<'de> Deserialize<'de> for JsonDocumentValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = JsonDocumentValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::Null))
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::Null))
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::Integer(value)))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(i64::try_from(value).map_or_else(
                    |_| Value::Number(value.into()),
                    Value::Integer,
                )))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .map(JsonDocumentValue)
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsonDocumentValue(Value::String(value)))
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(value) = sequence.next_element::<JsonDocumentValue>()? {
                    values.push(value.0);
                }
                Ok(JsonDocumentValue(Value::List(values)))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, JsonDocumentValue>()? {
                    match values.entry(key) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(value.0);
                        }
                        std::collections::btree_map::Entry::Occupied(entry) => {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate key {:?}",
                                entry.key()
                            )));
                        }
                    }
                }
                Ok(JsonDocumentValue(Value::Record(values)))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
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
        choices: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_parser_matches_serde_reference() {
        let documents = [
            r#"null"#,
            r#"{"unicode":"雪 café","escaped":"line\nquote\"slash\\","nested":[{},[],{"x":true}]}"#,
            r#"[-9223372036854775808,9223372036854775807,9223372036854775808,18446744073709551615,-1,0,1,1.0,-2.5,1e20]"#,
            r#"{"same":1,"empty_object":{},"empty_array":[]}"#,
        ];
        for document in documents {
            let bytes = document.as_bytes();
            assert_eq!(
                parse(bytes).unwrap(),
                old_parse(bytes).unwrap(),
                "{document}"
            );
        }

        for bytes in [&b""[..], b"null trailing", b"{"] {
            assert_eq!(
                format!("{:#}", parse(bytes).unwrap_err()),
                format!("{:#}", old_parse(bytes).unwrap_err()),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }

        assert!(to_json(&Value::Reference("missing".into()))
            .unwrap_err()
            .to_string()
            .contains("cannot encode an unresolved reference"));
    }

    #[test]
    fn duplicate_json_object_keys_are_rejected_with_source_position() {
        let error = parse(br#"{"a":1,"a":2}"#).expect_err("duplicate key must fail");
        let message = format!("{error:#}");
        assert!(message.contains("duplicate key \"a\""), "{message}");
        assert!(message.contains("line 1 column"), "{message}");
    }
}
