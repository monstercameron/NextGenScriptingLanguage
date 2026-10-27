//! Declared program inputs: one schema for CLI values, runtime calls and live edits.
use crate::ast::{InputDeclaration, InputSource, InputType, Value};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

impl std::fmt::Display for InputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Integer => "integer",
            Self::UnsignedInteger => "unsigned integer",
            Self::Usize => "usize",
            Self::Boolean => "boolean",
            Self::TextList => "list of text",
            Self::IntegerList => "list of integer",
            Self::UnsignedIntegerList => "list of unsigned integer",
            Self::UsizeList => "list of usize",
            Self::JsonList => "list of json",
            Self::Json => "json",
            Self::Never => "uninhabited empty-list item",
            Self::Bytes => "bytes",
        })
    }
}

fn input_value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Integer(_) | Value::Number(_) => "number",
        Value::String(_) => "text",
        Value::List(_) => "list",
        Value::Record(_) => "object",
        Value::Reference(_) => "binding reference",
    }
}

impl InputDeclaration {
    pub fn validate_value(&self, value: &Value) -> Result<()> {
        let valid = match (self.kind, value) {
            (InputType::Text, Value::String(s)) => !s.contains('\0'),
            (InputType::Integer, Value::Integer(_)) | (InputType::Boolean, Value::Bool(_)) => true,
            (InputType::UnsignedInteger, Value::Integer(value)) => *value >= 0,
            (InputType::UnsignedInteger, Value::Number(value)) => value.as_u64().is_some(),
            (InputType::Usize, Value::Integer(value)) => {
                *value >= 0 && *value as u64 <= usize::MAX as u64
            }
            (InputType::Usize, Value::Number(value)) => value
                .as_u64()
                .is_some_and(|value| value <= usize::MAX as u64),
            (InputType::TextList, Value::List(items)) => items
                .iter()
                .all(|v| matches!(v, Value::String(s) if !s.contains('\0'))),
            (InputType::IntegerList, Value::List(items)) => {
                items.iter().all(|v| matches!(v, Value::Integer(_)))
            }
            (InputType::UnsignedIntegerList, Value::List(items)) => items.iter().all(|item| {
                matches!(item, Value::Integer(value) if *value >= 0)
                    || matches!(item, Value::Number(value) if value.as_u64().is_some())
            }),
            (InputType::UsizeList, Value::List(items)) => items.iter().all(|item| {
                matches!(item, Value::Integer(value) if *value >= 0 && *value as u64 <= usize::MAX as u64)
                    || matches!(item, Value::Number(value) if value.as_u64().is_some_and(|value| value <= usize::MAX as u64))
            }),
            (InputType::JsonList, Value::List(items)) => {
                items.iter().all(crate::structured::literal)
            }
            (InputType::Json, value) => crate::structured::literal(value),
            _ => false,
        };
        if !valid {
            match (self.kind, value) {
                (InputType::Text, Value::String(_)) => {
                    bail!(
                        "line {}: input {:?} contains a NUL byte",
                        self.line,
                        self.name
                    )
                }
                (InputType::TextList, Value::List(items))
                    if items.iter().all(|item| matches!(item, Value::String(_))) =>
                {
                    bail!(
                        "line {}: input {:?} contains a NUL byte",
                        self.line,
                        self.name
                    )
                }
                (
                    InputType::IntegerList
                    | InputType::UnsignedIntegerList
                    | InputType::UsizeList
                    | InputType::JsonList,
                    Value::List(_),
                ) => bail!(
                    "line {}: input {:?} contains an item of the wrong type",
                    self.line,
                    self.name
                ),
                (InputType::Json, _) => bail!(
                    "line {}: input {:?} expects a literal JSON value",
                    self.line,
                    self.name
                ),
                (InputType::Integer, Value::Number(number)) if number.as_i64().is_none() => {
                    bail!(
                        "line {}: input {:?} does not fit in a 64-bit integer",
                        self.line,
                        self.name
                    )
                }
                (_, value) => bail!(
                    "line {}: input {:?} expects {}, got {}",
                    self.line,
                    self.name,
                    self.kind,
                    input_value_type(value)
                ),
            }
        }
        // Checked for every value path (CLI, runtime and live edits), before any effect.
        if let (Some(choices), Value::String(text)) = (&self.choices, value) {
            if !choices.contains(text) {
                if crate::ast::is_sensitive(self) {
                    bail!(
                        "line {}: input {:?} must be one of {}; the supplied value is not shown",
                        self.line,
                        self.name,
                        crate::parser::quoted_list(choices)
                    );
                }
                bail!(
                    "line {}: input {:?} must be one of {}; got {text:?}",
                    self.line,
                    self.name,
                    crate::parser::quoted_list(choices)
                );
            }
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
            InputType::UnsignedInteger => Value::Number(
                text.parse::<u64>()
                    .with_context(|| {
                        format!("input {:?} requires a nonnegative integer", self.name)
                    })?
                    .into(),
            ),
            InputType::Usize => Value::Number(
                (usize::try_from(
                    text.parse::<u64>()
                        .with_context(|| format!("input {:?} requires an integer", self.name))?,
                )
                .context("input does not fit in usize")? as u64)
                    .into(),
            ),
            InputType::Boolean => match text {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => bail!("input {:?} requires true or false", self.name),
            },
            InputType::Bytes => bail!(
                "input {:?} cannot be a byte capture; byte captures come from `then capture bytes`",
                self.name
            ),
            InputType::Never => bail!(
                "input {:?} cannot use the internal empty-loop item type",
                self.name
            ),
            InputType::TextList => {
                let items: Vec<String> = serde_json::from_str(text).with_context(|| {
                    format!("input {:?} requires a JSON list of text", self.name)
                })?;
                Value::List(items.into_iter().map(Value::String).collect())
            }
            InputType::IntegerList
            | InputType::UnsignedIntegerList
            | InputType::UsizeList
            | InputType::JsonList => {
                let value = crate::structured::parse(text.as_bytes())
                    .with_context(|| format!("input {:?} requires a JSON list", self.name))?;
                let Value::List(_) = value else {
                    bail!("input {:?} requires a JSON list", self.name);
                };
                value
            }
            InputType::Json => crate::structured::parse(text.as_bytes())
                .with_context(|| format!("input {:?} requires JSON", self.name))?,
        };
        self.validate_value(&value)?;
        Ok(value)
    }

    #[cfg(all(feature = "live", not(target_arch = "wasm32")))]
    pub(crate) fn parse_json(&self, value: &serde_json::Value) -> Result<Value> {
        let result = match (self.kind, value) {
            (InputType::Json, value) => crate::structured::from_json(value.clone()),
            (InputType::Text, serde_json::Value::String(s)) => Value::String(s.clone()),
            (InputType::Integer, serde_json::Value::Number(n)) if n.as_i64().is_some() => {
                Value::Integer(n.as_i64().unwrap())
            }
            (InputType::UnsignedInteger, serde_json::Value::Number(n)) if n.as_u64().is_some() => {
                Value::Number(n.clone())
            }
            (InputType::Usize, serde_json::Value::Number(n))
                if n.as_u64().is_some_and(|value| value <= usize::MAX as u64) =>
            {
                Value::Number(n.clone())
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
            (InputType::IntegerList, serde_json::Value::Array(items))
                if items.iter().all(|item| item.as_i64().is_some()) =>
            {
                Value::List(
                    items
                        .iter()
                        .map(|item| Value::Integer(item.as_i64().unwrap()))
                        .collect(),
                )
            }
            (InputType::UnsignedIntegerList, serde_json::Value::Array(items))
                if items.iter().all(|item| item.as_u64().is_some()) =>
            {
                Value::List(
                    items
                        .iter()
                        .map(|item| {
                            Value::Number(
                                item.as_number()
                                    .expect("validated unsigned integer array")
                                    .clone(),
                            )
                        })
                        .collect(),
                )
            }
            (InputType::UsizeList, serde_json::Value::Array(items))
                if items.iter().all(|item| {
                    item.as_u64()
                        .is_some_and(|value| value <= usize::MAX as u64)
                }) =>
            {
                Value::List(
                    items
                        .iter()
                        .map(|item| {
                            Value::Number(item.as_number().expect("validated usize array").clone())
                        })
                        .collect(),
                )
            }
            (InputType::JsonList, serde_json::Value::Array(items)) => Value::List(
                items
                    .iter()
                    .cloned()
                    .map(crate::structured::from_json)
                    .collect(),
            ),
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
    let mut spellings: BTreeSet<&str> = BTreeSet::new();
    for input in declarations {
        if input.kind == InputType::Never {
            bail!(
                "line {}: the internal empty-loop item type cannot be declared as a program input",
                input.line
            );
        }
        if !crate::parser::is_identifier(&input.name)
            || matches!(input.name.as_str(), "true" | "false" | "null")
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
            Some(InputSource::Flag { .. } | InputSource::Option { .. }) => {
                validate_flag_spelling(input, &mut spellings)?;
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

/// The values with every sensitive input removed, for display.
pub(crate) fn without_secret_answers(
    declarations: &[InputDeclaration],
    mut values: BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    for declaration in declarations {
        if crate::ast::is_sensitive(declaration) {
            values.remove(&declaration.name);
        }
    }
    values
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
                .ok_or_else(|| match &input.source {
                    Some(InputSource::Prompt { question, .. }) => anyhow!(
                        "line {}: input {:?} needs an answer to the prompt {question:?}; answer it on a terminal or supply --input {}=VALUE",
                        input.line,
                        input.name,
                        input.name
                    ),
                    _ => anyhow!(
                        "line {}: missing required input {:?}; supply --input {}=VALUE",
                        input.line,
                        input.name,
                        input.name
                    ),
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
    let SplitArguments {
        flagged,
        positional: arguments,
    } = split_flags(declarations, arguments)?;
    for (name, value) in flagged {
        if values.insert(name.clone(), value).is_some() {
            bail!("input {name:?} is given by --input and by a flag or option");
        }
    }
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
    // An absent flag is false unless the program gives it a default.
    for input in declarations {
        if matches!(input.source, Some(InputSource::Flag { .. }))
            && input.default.is_none()
            && !values.contains_key(&input.name)
        {
            values.insert(input.name.clone(), Value::Bool(false));
        }
    }
    Ok(values)
}

/// A flag or option spelling: `--` then a lowercase letter or digit, then lowercase
/// letters, digits or hyphens.
fn is_long_spelling(spelling: &str) -> bool {
    spelling.strip_prefix("--").is_some_and(|rest| {
        rest.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

/// An alias: one dash and one letter or digit.
fn is_short_spelling(spelling: &str) -> bool {
    matches!(spelling.as_bytes(), [b'-', letter] if letter.is_ascii_alphanumeric())
}

/// Checks a flag or option declaration: its type, its spellings, and that no spelling
/// is declared twice. `--help` and `-h` belong to the generated help.
fn validate_flag_spelling<'a>(
    input: &'a InputDeclaration,
    spellings: &mut BTreeSet<&'a str>,
) -> Result<()> {
    let (name, alias) = match &input.source {
        Some(InputSource::Flag { name, alias }) | Some(InputSource::Option { name, alias }) => {
            (name.as_str(), alias.as_deref())
        }
        _ => return Ok(()),
    };
    match &input.source {
        Some(InputSource::Flag { .. }) if input.kind != InputType::Boolean => {
            bail!("line {}: flag {name:?} must be a boolean input", input.line)
        }
        Some(InputSource::Option { .. })
            if !matches!(
                input.kind,
                InputType::Text
                    | InputType::Integer
                    | InputType::UnsignedInteger
                    | InputType::Usize
            ) =>
        {
            bail!(
                "line {}: option {name:?} must be a text or integer input",
                input.line
            )
        }
        _ => {}
    }
    if !is_long_spelling(name) {
        bail!(
            "line {}: flag or option name {name:?} must start with -- and use lowercase letters, digits or hyphens",
            input.line
        );
    }
    if name == "--help" || alias == Some("-h") {
        bail!(
            "line {}: --help and -h are reserved for the generated help",
            input.line
        );
    }
    if !spellings.insert(name) {
        bail!(
            "line {}: flag or option {name:?} is declared more than once",
            input.line
        );
    }
    if let Some(alias) = alias {
        if !is_short_spelling(alias) {
            bail!(
                "line {}: alias {alias:?} must be one dash and one letter or digit",
                input.line
            );
        }
        if !spellings.insert(alias) {
            bail!(
                "line {}: alias {alias:?} is declared more than once",
                input.line
            );
        }
    }
    Ok(())
}

/// The flag and option spellings a program declares, each mapped to its input.
struct FlagTable<'a> {
    by_spelling: BTreeMap<&'a str, &'a InputDeclaration>,
}

impl<'a> FlagTable<'a> {
    fn new(declarations: &'a [InputDeclaration]) -> Self {
        let mut by_spelling = BTreeMap::new();
        for input in declarations {
            if let Some(InputSource::Flag { name, alias })
            | Some(InputSource::Option { name, alias }) = &input.source
            {
                by_spelling.insert(name.as_str(), input);
                if let Some(alias) = alias {
                    by_spelling.insert(alias.as_str(), input);
                }
            }
        }
        Self { by_spelling }
    }

    fn is_empty(&self) -> bool {
        self.by_spelling.is_empty()
    }

    fn find(&self, spelling: &str) -> Option<&'a InputDeclaration> {
        self.by_spelling.get(spelling).copied()
    }

    fn known(&self) -> String {
        self.by_spelling
            .keys()
            .copied()
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Separates the declared flags and options from the positional arguments. A program
/// with no flags or options keeps every argument positional.
/// The declared flag and option values, and the arguments that stay positional.
struct SplitArguments {
    flagged: Vec<(String, Value)>,
    positional: Vec<String>,
}

fn split_flags(declarations: &[InputDeclaration], arguments: &[String]) -> Result<SplitArguments> {
    let table = FlagTable::new(declarations);
    if table.is_empty() {
        return Ok(SplitArguments {
            flagged: Vec::new(),
            positional: arguments.to_vec(),
        });
    }
    let mut found: Vec<(String, Value)> = Vec::new();
    let mut positional = Vec::new();
    let mut remaining = arguments.iter();
    while let Some(argument) = remaining.next() {
        // `--name=value` carries its value; a bare spelling takes the next argument.
        let (spelling, inline) = match argument.split_once('=') {
            Some((head, value)) if head.starts_with("--") => (head, Some(value)),
            _ => (argument.as_str(), None),
        };
        let Some(input) = table.find(spelling) else {
            if argument.starts_with("--") && argument != "--" {
                bail!(
                    "unknown flag {spelling:?}; the program declares {}",
                    table.known()
                );
            }
            positional.push(argument.clone());
            continue;
        };
        if found.iter().any(|(name, _)| *name == input.name) {
            bail!("{spelling} is given more than once");
        }
        let value = match &input.source {
            Some(InputSource::Flag { .. }) => {
                if inline.is_some() {
                    bail!("{spelling} takes no value");
                }
                Value::Bool(true)
            }
            _ => {
                let text = match inline {
                    Some(text) => text.to_owned(),
                    None => remaining
                        .next()
                        .with_context(|| format!("{spelling} needs a value"))?
                        .clone(),
                };
                input.parse_cli(&text)?
            }
        };
        found.push((input.name.clone(), value));
    }
    Ok(SplitArguments {
        flagged: found,
        positional,
    })
}

/// Whether the program declares any flag or option.
pub fn declares_flags(declarations: &[InputDeclaration]) -> bool {
    !FlagTable::new(declarations).is_empty()
}

/// Whether the arguments ask for the generated help. `--help` does for any program;
/// `-h` only for programs that declare flags or options, since it is otherwise a value.
pub fn is_help_request(declarations: &[InputDeclaration], arguments: &[String]) -> bool {
    match arguments.first().map(String::as_str) {
        Some("--help") => true,
        Some("-h") => declares_flags(declarations),
        _ => false,
    }
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
            Some(InputSource::ProgramDirectory) => {
                spec["source"] = serde_json::json!({"kind":"program_directory"});
            }
            Some(InputSource::ProgramPath) => {
                spec["source"] = serde_json::json!({"kind":"program_path"});
            }
            Some(InputSource::Prompt { question, secret }) => {
                spec["source"] =
                    serde_json::json!({"kind":"prompt", "question":question, "secret":secret});
            }
            Some(InputSource::Flag { name, alias }) => {
                spec["source"] = serde_json::json!({"kind":"flag", "name":name, "alias":alias});
            }
            Some(InputSource::Option { name, alias }) => {
                spec["source"] = serde_json::json!({"kind":"option", "name":name, "alias":alias});
            }
            None => {}
        }
        if let Some(text) = &input.description {
            spec["description"] = text.clone().into();
        }
        if let Some(choices) = &input.choices {
            spec["choices"] = choices.clone().into();
        }
        // A sensitive input's value is never printed, so it is withheld, not shown.
        let secret = crate::ast::is_sensitive(input);
        if let Some(value) = known.get(&input.name) {
            if secret {
                spec["value_withheld"] = true.into();
            } else {
                spec["value"] = literal_json(value)?;
            }
        } else {
            missing.push(&input.name);
        }
        inputs.push(spec);
    }
    Ok(serde_json::json!({
        "protocol": "execute.inspect/1", "validation": "static",
        "language_version": crate::parser::LANGUAGE_VERSION,
        "inputs": inputs, "inputs_complete": missing.is_empty(), "missing_inputs": missing
    }))
}

pub(crate) fn literal_json(value: &Value) -> Result<serde_json::Value> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(kind: InputType) -> InputDeclaration {
        InputDeclaration {
            name: "answer".into(),
            kind,
            default: None,
            source: None,
            line: 1,
            description: None,
            choices: None,
        }
    }

    #[test]
    fn input_validation_distinguishes_type_overflow_and_nul() {
        let wrong_type = declaration(InputType::Text).validate_value(&Value::Integer(5));
        assert!(wrong_type.unwrap_err().to_string().contains("got number"));
        let overflow = declaration(InputType::Integer).validate_value(&Value::Number(
            serde_json::Number::from(9_223_372_036_854_775_808u64),
        ));
        assert!(overflow.unwrap_err().to_string().contains("64-bit integer"));
        let nul = declaration(InputType::Text).validate_value(&Value::String("x\0y".into()));
        assert!(nul.unwrap_err().to_string().contains("NUL byte"));
    }

    #[cfg(all(feature = "live", not(target_arch = "wasm32")))]
    #[test]
    fn live_unsigned_integer_lists_preserve_exact_json_numbers() {
        let cases = [
            (InputType::UnsignedIntegerList, u64::MAX),
            (InputType::UsizeList, 42),
        ];

        for (kind, value) in cases {
            let parsed = declaration(kind)
                .parse_json(&serde_json::json!([value]))
                .unwrap();
            assert_eq!(
                parsed,
                Value::List(vec![Value::Number(serde_json::Number::from(value))])
            );
        }
    }

    #[test]
    fn null_is_reserved_as_an_input_name() {
        let mut input = declaration(InputType::Text);
        input.name = "null".into();
        assert!(validate(&[input])
            .unwrap_err()
            .to_string()
            .contains("invalid input name"));
    }
}
