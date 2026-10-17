//! Declared program inputs: one schema for CLI values, runtime calls and live edits.
use crate::ast::{InputDeclaration, InputSource, InputType, Value};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

impl std::fmt::Display for InputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::TextList => "list of text",
            Self::Json => "json",
        })
    }
}

impl InputDeclaration {
    pub fn validate_value(&self, value: &Value) -> Result<()> {
        let valid = match (self.kind, value) {
            (InputType::Text, Value::String(s)) => !s.contains('\0'),
            (InputType::Integer, Value::Integer(_)) | (InputType::Boolean, Value::Bool(_)) => true,
            (InputType::TextList, Value::List(items)) => items
                .iter()
                .all(|v| matches!(v, Value::String(s) if !s.contains('\0'))),
            (InputType::Json, value) => crate::structured::literal(value),
            _ => false,
        };
        if !valid {
            bail!(
                "line {}: input {:?} expects {}{}",
                self.line,
                self.name,
                self.kind,
                if self.kind == InputType::Json {
                    " as a literal value"
                } else {
                    " without NUL bytes"
                }
            );
        }
        Ok(())
    }

    pub fn parse_cli(&self, text: &str) -> Result<Value> {
        let value = match self.kind {
            InputType::Text => Value::String(text.into()),
            InputType::Integer => Value::Integer(
                text.parse()
                    .with_context(|| format!("input {:?} requires an integer", self.name))?,
            ),
            InputType::Boolean => match text {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => bail!("input {:?} requires true or false", self.name),
            },
            InputType::TextList => {
                let items: Vec<String> = serde_json::from_str(text).with_context(|| {
                    format!("input {:?} requires a JSON list of text", self.name)
                })?;
                Value::List(items.into_iter().map(Value::String).collect())
            }
            InputType::Json => crate::structured::parse(text.as_bytes())
                .with_context(|| format!("input {:?} requires JSON", self.name))?,
        };
        self.validate_value(&value)?;
        Ok(value)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn parse_json(&self, value: &serde_json::Value) -> Result<Value> {
        let result = match (self.kind, value) {
            (InputType::Json, value) => crate::structured::from_json(value.clone()),
            (InputType::Text, serde_json::Value::String(s)) => Value::String(s.clone()),
            (InputType::Integer, serde_json::Value::Number(n)) if n.as_i64().is_some() => {
                Value::Integer(n.as_i64().unwrap())
            }
            (InputType::Boolean, serde_json::Value::Bool(b)) => Value::Bool(*b),
            (InputType::TextList, serde_json::Value::Array(items))
                if items.iter().all(serde_json::Value::is_string) =>
            {
                Value::List(
                    items
                        .iter()
                        .map(|v| Value::String(v.as_str().unwrap().into()))
                        .collect(),
                )
            }
            _ => bail!(
                "line {}: input {:?} expects {}",
                self.line,
                self.name,
                self.kind
            ),
        };
        self.validate_value(&result)?;
        Ok(result)
    }
}

pub fn validate(declarations: &[InputDeclaration]) -> Result<()> {
    if declarations.len() <= 1 {
        // A singleton cannot duplicate a name; retain every other schema check.
        return validate_schema(declarations, |_| true);
    }
    let mut names = BTreeSet::new();
    validate_schema(declarations, |input| names.insert(input.name.as_str()))
}

enum ValidatedSpecs<'a> {
    Small(&'a [InputDeclaration]),
    Indexed(BTreeMap<&'a str, &'a InputDeclaration>),
}
impl<'a> ValidatedSpecs<'a> {
    fn get(&self, name: &str) -> Option<&'a InputDeclaration> {
        match self {
            Self::Small(declarations) => declarations.iter().find(|input| input.name == name),
            Self::Indexed(specs) => specs.get(name).copied(),
        }
    }
}

fn validated_specs(declarations: &[InputDeclaration]) -> Result<ValidatedSpecs<'_>> {
    // Small interfaces do not amortize the wider lookup-index nodes.
    if declarations.len() <= 8 {
        validate(declarations)?;
        return Ok(ValidatedSpecs::Small(declarations));
    }
    let mut specs = BTreeMap::new();
    validate_schema(declarations, |input| {
        specs.insert(input.name.as_str(), input).is_none()
    })?;
    Ok(ValidatedSpecs::Indexed(specs))
}

// Collect the lookup index during the existing duplicate-name check. Keep all
// declaration checks in their original order, before inspecting supplied keys.
fn validate_schema<'a>(
    declarations: &'a [InputDeclaration],
    mut insert: impl FnMut(&'a InputDeclaration) -> bool,
) -> Result<()> {
    for input in declarations {
        if !crate::parser::is_identifier(&input.name)
            || matches!(input.name.as_str(), "true" | "false")
        {
            bail!("line {}: invalid input name {:?}", input.line, input.name);
        }
        if !insert(input) {
            bail!("line {}: duplicate input {:?}", input.line, input.name);
        }
        if let Some(default) = &input.default {
            input.validate_value(default)?;
        }
        match &input.source {
            Some(InputSource::Environment(name)) => {
                let mut bytes = name.bytes();
                if !bytes
                    .next()
                    .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                    || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    bail!(
                        "line {}: environment name must be an ASCII identifier",
                        input.line
                    );
                }
            }
            Some(InputSource::Argument(position)) if !(1..=1_000_000).contains(position) => {
                bail!(
                    "line {}: argument position must be between 1 and 1000000",
                    input.line
                );
            }
            Some(InputSource::Arguments) if input.kind != InputType::TextList => {
                bail!(
                    "line {}: `from arguments` requires list of text",
                    input.line
                );
            }
            Some(InputSource::Arguments) if input.default.is_some() => {
                bail!(
                    "line {}: `from arguments` always supplies a list; omit its default",
                    input.line
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// Return resolved declared values. `allow_extra` permits host-provided local
/// bindings; the returned map still contains declared inputs only.
pub fn resolve(
    declarations: &[InputDeclaration],
    supplied: &BTreeMap<String, Value>,
    allow_extra: bool,
) -> Result<BTreeMap<String, Value>> {
    if allow_extra {
        validate(declarations)?;
    } else {
        let specs = validated_specs(declarations)?;
        for name in supplied.keys() {
            if specs.get(name.as_str()).is_none() {
                bail!("unknown program input {name:?}");
            }
        }
    }
    declarations
        .iter()
        .map(|input| {
            let environment = if supplied.contains_key(&input.name) {
                None
            } else if let Some(InputSource::Environment(name)) = &input.source {
                match std::env::var_os(name) {
                    Some(value) => {
                        let text = value.into_string().map_err(|_| {
                            anyhow!(
                                "line {}: environment input {:?} is not valid UTF-8",
                                input.line,
                                input.name
                            )
                        })?;
                        Some(input.parse_cli(&text).with_context(|| {
                            format!(
                                "line {}: invalid environment input {:?}",
                                input.line, input.name
                            )
                        })?)
                    }
                    None => None,
                }
            } else {
                matches!(input.source, Some(InputSource::Arguments))
                    .then(|| Value::List(Vec::new()))
            };
            let value = supplied
                .get(&input.name)
                .or(environment.as_ref())
                .or(input.default.as_ref())
                .ok_or_else(|| {
                    anyhow!(
                        "line {}: missing required input {:?}; supply --input {}=VALUE",
                        input.line,
                        input.name,
                        input.name
                    )
                })?;
            input.validate_value(value)?;
            Ok((input.name.clone(), value.clone()))
        })
        .collect()
}

/// CLI checks/explanations validate provided values but may leave required inputs
/// absent, allowing agents to inspect a program before choosing its arguments.
pub fn parse_cli(
    declarations: &[InputDeclaration],
    supplied: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Value>> {
    let specs = validated_specs(declarations)?;
    supplied
        .iter()
        .map(|(name, text)| {
            let spec = specs
                .get(name.as_str())
                .ok_or_else(|| anyhow!("unknown program input {name:?}"))?;
            Ok((name.clone(), spec.parse_cli(text)?))
        })
        .collect()
}

/// Bind the explicit Unix argv tail once at the process boundary. Named CLI
/// values take precedence, and runtime/checkpoint APIs receive ordinary inputs.
pub fn parse_cli_arguments(
    declarations: &[InputDeclaration],
    supplied: &BTreeMap<String, String>,
    arguments: &[String],
) -> Result<BTreeMap<String, Value>> {
    let mut values = parse_cli(declarations, supplied)?;
    let variadic = declarations
        .iter()
        .any(|d| d.source == Some(InputSource::Arguments));
    let maximum = declarations
        .iter()
        .filter_map(|d| match d.source {
            Some(InputSource::Argument(position)) => Some(position),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    if !variadic && arguments.len() > maximum {
        bail!(
            "program accepts at most {maximum} positional arguments; received {}",
            arguments.len()
        );
    }
    for input in declarations {
        if values.contains_key(&input.name) {
            continue;
        }
        let value = match input.source {
            Some(InputSource::Argument(position)) => arguments
                .get(position - 1)
                .map(|value| input.parse_cli(value))
                .transpose()?,
            Some(InputSource::Arguments) => Some(Value::List(
                arguments
                    .iter()
                    .map(|value| Value::String(value.clone()))
                    .collect(),
            )),
            _ => None,
        };
        if let Some(value) = value {
            input.validate_value(&value)?;
            values.insert(input.name.clone(), value);
        }
    }
    Ok(values)
}

/// Describe a partial input interface without requiring values or running effects.
/// Types use language spellings; defaults and effective values use native JSON.
pub fn describe(
    declarations: &[InputDeclaration],
    supplied: &BTreeMap<String, Value>,
) -> Result<serde_json::Value> {
    let known = known_values(declarations, supplied)?;
    let mut inputs = Vec::with_capacity(declarations.len());
    let mut missing = Vec::new();
    for input in declarations {
        let mut spec = serde_json::json!({
            "name": input.name, "type": input.kind.to_string(),
            "line": input.line, "required": input.default.is_none()
        });
        if let Some(default) = &input.default {
            spec["default"] = literal_json(default)?;
        }
        match &input.source {
            Some(InputSource::Environment(name)) => {
                spec["source"] = serde_json::json!({"kind":"environment", "name":name});
            }
            Some(InputSource::Argument(position)) => {
                spec["source"] = serde_json::json!({"kind":"argument", "position":position});
            }
            Some(InputSource::Arguments) => {
                spec["source"] = serde_json::json!({"kind":"arguments"});
            }
            None => {}
        }
        if let Some(text) = &input.description {
            spec["description"] = text.clone().into();
        }
        if let Some(value) = known.get(&input.name) {
            spec["value"] = literal_json(value)?;
        } else {
            missing.push(&input.name);
        }
        inputs.push(spec);
    }
    Ok(serde_json::json!({
        "protocol": "execute.inspect/1", "validation": "static",
        "inputs": inputs, "inputs_complete": missing.is_empty(), "missing_inputs": missing
    }))
}

fn literal_json(value: &Value) -> Result<serde_json::Value> {
    crate::structured::to_json(value).context("input inspection requires literal values")
}

pub(crate) fn known_values(
    declarations: &[InputDeclaration],
    supplied: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>> {
    let specs = validated_specs(declarations)?;
    let mut values = declarations
        .iter()
        .filter_map(|d| {
            d.default
                .as_ref()
                .map(|value| (d.name.clone(), value.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    for (name, value) in supplied {
        let input = specs
            .get(name.as_str())
            .ok_or_else(|| anyhow!("unknown program input {name:?}"))?;
        input.validate_value(value)?;
        values.insert(name.clone(), value.clone());
    }
    Ok(values)
}
