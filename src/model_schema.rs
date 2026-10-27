//! Shared strict JSON parsing and schema validation for `llm.extract`.
//!
//! Static checking and the model helper use the same parser and validator so
//! literal schemas cannot drift from runtime acceptance.

use anyhow::{anyhow, bail, Result};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::error::Error as StdError;
use std::fmt;

#[derive(Debug)]
pub(crate) struct DuplicateJsonKey;

impl fmt::Display for DuplicateJsonKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("duplicate JSON object key")
    }
}

impl StdError for DuplicateJsonKey {}

struct UniqueJson(Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> Visitor<'de> for UniqueJsonVisitor {
    type Value = UniqueJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        let number = serde_json::Number::from_f64(value)
            .ok_or_else(|| E::custom("non-finite JSON number"))?;
        Ok(UniqueJson(Value::Number(number)))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::String(value)))
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Null))
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(UniqueJson(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(UniqueJson(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        let mut keys = BTreeSet::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let UniqueJson(value) = object.next_value()?;
            values.insert(key, value);
        }
        Ok(UniqueJson(Value::Object(values)))
    }
}

pub(crate) fn parse_unique_json(bytes: &[u8]) -> Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let UniqueJson(value) = match UniqueJson::deserialize(&mut deserializer) {
        Ok(value) => value,
        Err(error) if error.to_string().starts_with("duplicate JSON object key") => {
            return Err(DuplicateJsonKey.into());
        }
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = deserializer.end() {
        if error.to_string().starts_with("duplicate JSON object key") {
            return Err(DuplicateJsonKey.into());
        }
        return Err(error.into());
    }
    Ok(value)
}

pub(crate) fn validate_schema_definition(schema: &Value) -> Result<()> {
    if !schema.is_object() {
        bail!("schema must be a JSON object");
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") || schema.get("anyOf").is_some()
    {
        bail!("strict extraction schemas need an object root without anyOf");
    }
    validate_schema_keywords(schema)
}

fn validate_schema_keywords(schema: &Value) -> Result<()> {
    const SUPPORTED: &[&str] = &[
        "$schema",
        "$id",
        "$comment",
        "title",
        "description",
        "examples",
        "default",
        "deprecated",
        "readOnly",
        "writeOnly",
        "type",
        "enum",
        "const",
        "required",
        "properties",
        "additionalProperties",
        "items",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minProperties",
        "maxProperties",
        "uniqueItems",
        "anyOf",
    ];
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow!("schema node must be an object"))?;
    if object.keys().any(|key| !SUPPORTED.contains(&key.as_str())) {
        bail!("schema contains an unsupported keyword");
    }
    for key in ["$schema", "$id", "$comment", "title", "description"] {
        if object.get(key).is_some_and(|value| !value.is_string()) {
            bail!("schema text keyword has an invalid shape");
        }
    }
    for key in ["deprecated", "readOnly", "writeOnly", "uniqueItems"] {
        if object.get(key).is_some_and(|value| !value.is_boolean()) {
            bail!("schema boolean keyword has an invalid shape");
        }
    }
    for key in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        if object
            .get(key)
            .is_some_and(|value| value.as_u64().is_none())
        {
            bail!("schema count keyword has an invalid shape");
        }
    }
    for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if object
            .get(key)
            .is_some_and(|value| value.as_f64().is_none())
        {
            bail!("schema numeric keyword has an invalid shape");
        }
    }
    if object.get("multipleOf").is_some_and(|value| {
        !value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number > 0.0)
    }) {
        bail!("schema multipleOf must be a positive finite number");
    }
    let types = match object.get("type") {
        Some(Value::String(kind)) => {
            if !is_supported_schema_type(kind) {
                bail!("schema type is unsupported");
            }
            Some(BTreeSet::from([kind.as_str()]))
        }
        Some(Value::Array(kinds)) => {
            if kinds.is_empty() {
                bail!("schema type union cannot be empty");
            }
            let mut types = BTreeSet::new();
            for kind in kinds {
                let kind = kind
                    .as_str()
                    .filter(|kind| is_supported_schema_type(kind))
                    .ok_or_else(|| anyhow!("schema type is unsupported"))?;
                if !types.insert(kind) {
                    bail!("schema type union entries must be distinct");
                }
            }
            Some(types)
        }
        Some(_) => bail!("schema type must be text or a text array"),
        None => None,
    };
    let has_object_keywords = ["properties", "required", "additionalProperties"]
        .iter()
        .any(|key| object.contains_key(*key));
    let includes_object = types.as_ref().is_some_and(|types| types.contains("object"));
    let includes_array = types.as_ref().is_some_and(|types| types.contains("array"));
    if has_object_keywords && types.is_some() && !includes_object {
        bail!("object keywords require type object");
    }
    if object.contains_key("items") && types.is_some() && !includes_array {
        bail!("items requires type array");
    }
    if let Some(enumeration) = object.get("enum") {
        if enumeration
            .as_array()
            .is_none_or(|values| values.is_empty())
        {
            bail!("schema enum must be a nonempty array");
        }
    }
    let object_schema = includes_object || has_object_keywords;
    if object_schema {
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("object schema must declare properties"))?;
        let required = object
            .get("required")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("object schema must declare required properties"))?;
        let mut required_names = BTreeSet::new();
        for name in required {
            let name = name
                .as_str()
                .ok_or_else(|| anyhow!("schema required entries must be text"))?;
            if !required_names.insert(name) {
                bail!("schema required entries must be unique");
            }
        }
        if required_names
            != properties
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>()
        {
            bail!("strict object schemas must require every declared property");
        }
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            bail!("strict object schemas must set additionalProperties to false");
        }
        for property in properties.values() {
            validate_schema_keywords(property)?;
        }
    }
    if let Some(items) = object.get("items") {
        if !items.is_object() {
            bail!("schema items must be an object schema");
        }
        validate_schema_keywords(items)?;
    }
    if object
        .get("additionalProperties")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        bail!("additionalProperties must be false");
    }
    if let Some(value) = object.get("anyOf") {
        let alternatives = value
            .as_array()
            .ok_or_else(|| anyhow!("schema alternatives must be an array"))?;
        if alternatives.is_empty() {
            bail!("schema alternatives cannot be empty");
        }
        for alternative in alternatives {
            validate_schema_keywords(alternative)?;
        }
    }
    Ok(())
}

fn is_supported_schema_type(kind: &str) -> bool {
    matches!(
        kind,
        "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
    )
}
