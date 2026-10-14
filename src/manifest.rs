use crate::ast::{InputType, Invocation, Value};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// A declarative description of an applet. Argument nodes produce argv elements
/// directly; no shell is involved in expanding a manifest.
#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub name: String,
    pub binary: String,
    pub builtin: Option<Builtin>,
    pub accepts_stdin: bool,
    pub cacheable: bool,
    /// A failed attempt may already have caused an external effect, such as billing.
    #[serde(skip_serializing_if = "is_false")]
    pub uncertain_on_failure: bool,
    pub phrases: Vec<String>,
    parameters: BTreeMap<String, ParameterSpec>,
    arguments: Vec<ArgumentNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Builtin {
    Cd,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "RawParameter")]
struct ParameterSpec {
    kind: ParameterType,
    item_type: Option<ParameterType>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    min_items: Option<usize>,
}

impl From<RawParameter> for ParameterSpec {
    fn from(raw: RawParameter) -> Self {
        Self {
            kind: raw.kind,
            item_type: raw.items,
            required: raw.required,
            min_items: raw.min_items,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum ParameterType {
    String,
    Integer,
    Boolean,
    List,
    #[serde(skip_deserializing)]
    Scalar,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ArgumentNode {
    Literal {
        literal: String,
    },
    Parameter {
        parameter: String,
    },
    Spread {
        spread: String,
    },
    WhenPresent {
        when_present: String,
        append: Vec<ArgumentNode>,
    },
}

/// Loaded manifest catalog. `resolve` accepts either a canonical name or phrase.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    data: Arc<RegistryData>,
}

#[derive(Debug, Default)]
struct RegistryData {
    manifests: Vec<Manifest>,
    names: BTreeMap<String, usize>,
}

impl Registry {
    /// Missing optional catalogs do not prevent direct executable calls.
    /// Explicit catalog paths should use `load` so configuration mistakes fail.
    pub fn load_optional(path: &Path) -> Result<Self> {
        match fs::metadata(path) {
            Ok(_) => Self::load(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("reading manifest path {}", path.display()))
            }
        }
    }

    pub fn resolve_call(&self, call: &Invocation) -> Result<&Manifest> {
        if !call.direct {
            return self.resolve(&call.operation);
        }
        if call.operation.is_empty() || call.operation.contains('\0') {
            bail!("executable name must be nonempty and cannot contain a NUL byte");
        }
        static DIRECT: OnceLock<Manifest> = OnceLock::new();
        Ok(DIRECT.get_or_init(|| Manifest {
            name: "direct executable".into(),
            binary: "".into(),
            builtin: None,
            accepts_stdin: true,
            cacheable: false,
            uncertain_on_failure: false,
            phrases: Vec::new(),
            parameters: BTreeMap::from([(
                "arguments".into(),
                ParameterSpec {
                    kind: ParameterType::List,
                    item_type: Some(ParameterType::Scalar),
                    required: false,
                    min_items: None,
                },
            )]),
            arguments: vec![ArgumentNode::WhenPresent {
                when_present: "arguments".into(),
                append: vec![ArgumentNode::Spread {
                    spread: "arguments".into(),
                }],
            }],
        }))
    }

    /// Load one JSON manifest file or all `.json` files in a directory.
    pub fn load(path: &Path) -> Result<Self> {
        let paths = if path.is_dir() {
            let mut entries = fs::read_dir(path)
                .with_context(|| format!("reading manifest directory {}", path.display()))?
                .map(|entry| entry.map(|e| e.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            entries.retain(|p| p.extension().is_some_and(|ext| ext == "json"));
            entries.sort();
            entries
        } else {
            vec![path.to_path_buf()]
        };
        if paths.is_empty() {
            bail!("no JSON manifests found in {}", path.display());
        }

        let mut data = RegistryData {
            manifests: Vec::with_capacity(paths.len()),
            names: BTreeMap::new(),
        };
        for manifest_path in paths {
            let raw = fs::read_to_string(&manifest_path)
                .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
            let manifest = parse_manifest(&raw)
                .with_context(|| format!("invalid manifest {}", manifest_path.display()))?;
            let index = data.manifests.len();
            for key in std::iter::once(&manifest.name).chain(manifest.phrases.iter()) {
                if let Some(previous) = data.names.insert(key.clone(), index) {
                    let previous_name = &data.manifests[previous].name;
                    bail!("duplicate manifest name or phrase {key:?} (used by {previous_name:?} and {:?})", manifest.name);
                }
            }
            data.manifests.push(manifest);
        }
        Ok(Self {
            data: Arc::new(data),
        })
    }

    pub fn resolve(&self, name_or_phrase: &str) -> Result<&Manifest> {
        self.data
            .names
            .get(name_or_phrase)
            .map(|&index| &self.data.manifests[index])
            .ok_or_else(|| anyhow!("unknown applet {name_or_phrase:?}"))
    }

    pub fn manifests(&self) -> &[Manifest] {
        &self.data.manifests
    }
}

impl Manifest {
    pub(crate) fn checkpoint_identity(&self, call: &Invocation) -> Result<Vec<u8>> {
        if call.direct {
            // Widening accepted scalar argv does not change the command mapping
            // for existing calls. Keep the 0.7.0 direct-call identity; actual
            // values and new input signatures are fingerprinted independently.
            let mut legacy = self.clone();
            legacy
                .parameters
                .get_mut("arguments")
                .expect("direct argv schema")
                .item_type = Some(ParameterType::String);
            Ok(serde_json::to_vec(&legacy)?)
        } else {
            Ok(serde_json::to_vec(self)?)
        }
    }

    pub fn name_for<'a>(&'a self, invocation: &'a Invocation) -> &'a str {
        if invocation.direct {
            &invocation.operation
        } else {
            &self.name
        }
    }

    pub fn binary_for<'a>(&'a self, invocation: &'a Invocation) -> &'a str {
        if invocation.direct {
            &invocation.operation
        } else {
            &self.binary
        }
    }

    /// Check names, required parameters, and statically known types. Named
    /// results contain text; their string contents are checked after resolution.
    pub fn validate(&self, invocation: &Invocation) -> Result<()> {
        self.validate_with_types(invocation, &BTreeMap::new())
    }

    pub(crate) fn validate_with_types(
        &self,
        invocation: &Invocation,
        bindings: &BTreeMap<String, InputType>,
    ) -> Result<()> {
        for name in invocation.parameters.keys() {
            if !self.parameters.contains_key(name) {
                bail!("applet {:?} has no parameter {name:?}", self.name);
            }
        }
        for (name, spec) in &self.parameters {
            if spec.required && !invocation.parameters.contains_key(name) {
                bail!("applet {:?} requires parameter {name:?}", self.name);
            }
        }
        for (name, value) in &invocation.parameters {
            let spec = &self.parameters[name];
            if let Value::Reference(reference) = value {
                let kind = bindings.get(reference).copied().unwrap_or(InputType::Text);
                let compatible = matches!(
                    (spec.kind, kind),
                    (ParameterType::String, InputType::Text)
                        | (ParameterType::Integer, InputType::Integer)
                        | (ParameterType::Boolean, InputType::Boolean)
                ) || (spec.kind == ParameterType::List
                    && kind == InputType::TextList
                    && matches!(
                        spec.item_type,
                        Some(ParameterType::String | ParameterType::Scalar)
                    ));
                if !compatible {
                    bail!(
                        "parameter {name:?} expects {}, but reference {reference:?} contains {kind}; named results contain text",
                        spec.kind,
                    );
                }
                continue;
            }
            if let Value::List(items) = value {
                for item in items {
                    if let Value::Reference(reference) = item {
                        if bindings.get(reference).is_some_and(|kind| {
                            (spec.item_type == Some(ParameterType::String)
                                && *kind != InputType::Text)
                                || matches!(kind, InputType::TextList | InputType::Json)
                        }) {
                            bail!("parameter {name:?} requires text list items, but {reference:?} contains {}", bindings[reference]);
                        }
                    }
                }
            }
            validate_value(name, value, spec, true)?;
        }
        Ok(())
    }

    /// Validate resolved values and produce the exact argv vector for the child.
    pub fn build_arguments(&self, values: &BTreeMap<String, Value>) -> Result<Vec<String>> {
        self.validate_argument_values(values)?;
        let mut argv = Vec::with_capacity(self.arguments.len());
        for node in &self.arguments {
            append_arguments(node, values, &mut argv)?;
        }
        Ok(argv)
    }

    /// Build argv for immediate Command construction without cloning borrowed text.
    pub(crate) fn build_arguments_borrowed<'a>(
        &'a self,
        values: &'a BTreeMap<String, Value>,
    ) -> Result<Vec<Cow<'a, str>>> {
        self.validate_argument_values(values)?;
        let mut argv = Vec::with_capacity(self.arguments.len());
        for node in &self.arguments {
            append_arguments_borrowed(node, values, &mut argv)?;
        }
        Ok(argv)
    }

    fn validate_argument_values(&self, values: &BTreeMap<String, Value>) -> Result<()> {
        // Both maps are ordered. Merge their keys to find the first unknown
        // supplied name in linear time while preserving the old error order.
        let supplied_names = values.keys();
        let mut parameter_names = self.parameters.keys().peekable();
        for name in supplied_names {
            while parameter_names
                .peek()
                .is_some_and(|parameter| parameter.as_str() < name.as_str())
            {
                parameter_names.next();
            }
            if !parameter_names
                .peek()
                .is_some_and(|parameter| parameter.as_str() == name.as_str())
            {
                bail!("applet {:?} has no parameter {name:?}", self.name);
            }
        }

        let mut supplied = values.iter().peekable();
        for (name, spec) in &self.parameters {
            while supplied
                .peek()
                .is_some_and(|(supplied_name, _)| supplied_name.as_str() < name.as_str())
            {
                supplied.next();
            }
            let value = match supplied.peek() {
                Some((supplied_name, value)) if supplied_name.as_str() == name.as_str() => {
                    let value = *value;
                    supplied.next();
                    Some(value)
                }
                _ => None,
            };
            match value {
                None if spec.required => {
                    bail!("applet {:?} requires parameter {name:?}", self.name)
                }
                None => {}
                Some(value) => validate_value(name, value, spec, false)?,
            }
        }
        Ok(())
    }
}

fn validate_value(
    name: &str,
    value: &Value,
    spec: &ParameterSpec,
    allow_reference_items: bool,
) -> Result<()> {
    let valid = matches!(
        (spec.kind, value),
        (ParameterType::String, Value::String(_))
            | (ParameterType::Integer, Value::Integer(_))
            | (ParameterType::Boolean, Value::Bool(_))
            | (ParameterType::List, Value::List(_))
    );
    if !valid {
        bail!(
            "parameter {name:?} expects {}, got {}",
            spec.kind,
            value_kind(value)
        );
    }
    if let Value::String(string) = value {
        if string.contains('\0') {
            bail!("parameter {name:?} contains a NUL byte, which cannot be passed in argv");
        }
    }
    if let (ParameterType::List, Value::List(items)) = (spec.kind, value) {
        let item_type = spec.item_type.expect("list schema validated");
        let mut contains_nul = false;
        for item in items {
            if !is_scalar_type(item, item_type)
                && !(allow_reference_items
                    && matches!(item_type, ParameterType::String | ParameterType::Scalar)
                    && matches!(item, Value::Reference(_)))
            {
                bail!(
                    "parameter {name:?} contains an item that is not {}",
                    item_type
                );
            }
            contains_nul |= matches!(item, Value::String(string) if string.contains('\0'));
        }
        if contains_nul {
            bail!("parameter {name:?} contains a NUL byte, which cannot be passed in argv");
        }
        if let Some(minimum) = spec.min_items {
            if items.len() < minimum {
                bail!("parameter {name:?} needs at least {minimum} item(s)");
            }
        }
    }
    Ok(())
}

fn is_scalar_type(value: &Value, expected: ParameterType) -> bool {
    matches!(
        (expected, value),
        (ParameterType::String, Value::String(_))
            | (ParameterType::Integer, Value::Integer(_))
            | (ParameterType::Boolean, Value::Bool(_))
            | (
                ParameterType::Scalar,
                Value::String(_) | Value::Integer(_) | Value::Bool(_)
            )
    )
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Integer(_) => "integer",
        Value::Bool(_) => "boolean",
        Value::List(_) => "list",
        Value::Reference(_) => "reference",
        Value::Record(_) | Value::Null | Value::Number(_) => "json",
    }
}

fn append_arguments(
    node: &ArgumentNode,
    values: &BTreeMap<String, Value>,
    out: &mut Vec<String>,
) -> Result<()> {
    match node {
        ArgumentNode::Literal { literal } => out.push(literal.clone()),
        ArgumentNode::Parameter { parameter } => {
            let value = values
                .get(parameter)
                .ok_or_else(|| anyhow!("missing parameter {parameter:?}"))?;
            out.push(value_to_string(value)?);
        }
        ArgumentNode::Spread { spread } => {
            let value = values
                .get(spread)
                .ok_or_else(|| anyhow!("missing parameter {spread:?}"))?;
            let Value::List(items) = value else {
                bail!("parameter {spread:?} must be a list")
            };
            out.reserve(items.len());
            for item in items {
                out.push(value_to_string(item)?);
            }
        }
        ArgumentNode::WhenPresent {
            when_present,
            append,
        } => {
            if values.contains_key(when_present) {
                for child in append {
                    append_arguments(child, values, out)?;
                }
            }
        }
    }
    Ok(())
}

fn append_arguments_borrowed<'a>(
    node: &'a ArgumentNode,
    values: &'a BTreeMap<String, Value>,
    out: &mut Vec<Cow<'a, str>>,
) -> Result<()> {
    match node {
        ArgumentNode::Literal { literal } => out.push(Cow::Borrowed(literal)),
        ArgumentNode::Parameter { parameter } => {
            let value = values
                .get(parameter)
                .ok_or_else(|| anyhow!("missing parameter {parameter:?}"))?;
            out.push(value_to_cow(value)?);
        }
        ArgumentNode::Spread { spread } => {
            let value = values
                .get(spread)
                .ok_or_else(|| anyhow!("missing parameter {spread:?}"))?;
            let Value::List(items) = value else {
                bail!("parameter {spread:?} must be a list")
            };
            out.reserve(items.len());
            for item in items {
                out.push(value_to_cow(item)?);
            }
        }
        ArgumentNode::WhenPresent {
            when_present,
            append,
        } => {
            if values.contains_key(when_present) {
                for child in append {
                    append_arguments_borrowed(child, values, out)?;
                }
            }
        }
    }
    Ok(())
}

fn value_to_cow(value: &Value) -> Result<Cow<'_, str>> {
    match value {
        Value::String(value) => Ok(Cow::Borrowed(value)),
        Value::Integer(value) => Ok(Cow::Owned(value.to_string())),
        Value::Bool(value) => Ok(Cow::Borrowed(if *value { "true" } else { "false" })),
        Value::List(_) => bail!("cannot use a list as one argument"),
        Value::Reference(_) => bail!("unresolved reference cannot become an argument"),
        Value::Record(_) | Value::Null | Value::Number(_) => {
            bail!("select a typed scalar or encode JSON before passing an argument")
        }
    }
}

fn value_to_string(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Integer(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::List(_) => bail!("cannot use a list as one argument"),
        Value::Reference(_) => bail!("unresolved reference cannot become an argument"),
        Value::Record(_) | Value::Null | Value::Number(_) => {
            bail!("select a typed scalar or encode JSON before passing an argument")
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    #[serde(default)]
    phrases: Vec<String>,
    binary: String,
    #[serde(default)]
    builtin: Option<Builtin>,
    #[serde(default)]
    accepts_stdin: bool,
    #[serde(default)]
    cacheable: bool,
    #[serde(default)]
    uncertain_on_failure: bool,
    #[serde(default)]
    parameters: BTreeMap<String, ParameterSpec>,
    arguments: Vec<RawArgument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParameter {
    #[serde(rename = "type")]
    kind: ParameterType,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    items: Option<ParameterType>,
    #[serde(default)]
    min_items: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
#[serde(deny_unknown_fields)]
enum RawArgument {
    Literal {
        literal: String,
    },
    Parameter {
        parameter: String,
    },
    Spread {
        spread: String,
    },
    WhenPresent {
        when_present: String,
        append: Vec<RawArgument>,
    },
}

fn parse_manifest(raw: &str) -> Result<Manifest> {
    let raw: RawManifest = serde_json::from_str(raw)?;
    if raw.name.trim().is_empty() || raw.binary.trim().is_empty() {
        bail!("manifest name and binary must be nonempty");
    }
    if raw.binary.contains('\0') {
        bail!("manifest binary cannot contain a NUL byte");
    }
    let mut phrases = BTreeSet::new();
    for phrase in &raw.phrases {
        if phrase.trim().is_empty() || phrase == &raw.name || !phrases.insert(phrase) {
            bail!(
                "manifest {:?} has an empty or duplicate phrase {phrase:?}",
                raw.name
            );
        }
    }
    let parameters = raw.parameters;
    for (name, spec) in &parameters {
        if name.trim().is_empty() {
            bail!("parameter names must be nonempty");
        }
        let is_list = spec.kind == ParameterType::List;
        if is_list != spec.item_type.is_some() {
            bail!(
                "parameter {name:?}: list requires an items type, and scalar types cannot have one"
            );
        }
        if spec.item_type == Some(ParameterType::List) {
            bail!("parameter {name:?}: nested lists are not supported");
        }
        if !is_list && spec.min_items.is_some() {
            bail!("parameter {name:?}: min_items only applies to lists");
        }
    }
    let arguments = raw
        .arguments
        .into_iter()
        .map(|node| convert_argument(node, &parameters))
        .collect::<Result<Vec<_>>>()?;
    let manifest = Manifest {
        name: raw.name,
        phrases: raw.phrases,
        binary: raw.binary,
        builtin: raw.builtin,
        accepts_stdin: raw.accepts_stdin,
        cacheable: raw.cacheable,
        uncertain_on_failure: raw.uncertain_on_failure,
        parameters,
        arguments,
    };
    validate_builtin(&manifest)?;
    if manifest.uncertain_on_failure && (manifest.cacheable || manifest.builtin.is_some()) {
        bail!("uncertain_on_failure requires a non-cacheable external applet");
    }
    validate_argument_usage(&manifest)?;
    Ok(manifest)
}

fn validate_builtin(manifest: &Manifest) -> Result<()> {
    match manifest.builtin {
        None => Ok(()),
        Some(Builtin::Cd) => {
            if manifest.binary != "cd"
                || manifest.accepts_stdin
                || manifest.cacheable
                || manifest.parameters.len() != 1
                || manifest.parameters.get("path").is_none_or(|path| {
                    path.kind != ParameterType::String
                        || !path.required
                        || path.item_type.is_some()
                        || path.min_items.is_some()
                })
                || !matches!(
                    manifest.arguments.as_slice(),
                    [ArgumentNode::Parameter { parameter }] if parameter == "path"
                )
            {
                bail!("cd builtin must use binary cd, one required string path parameter, no stdin or caching, and map only path as its argument");
            }
            Ok(())
        }
    }
}

fn convert_argument(
    raw: RawArgument,
    parameters: &BTreeMap<String, ParameterSpec>,
) -> Result<ArgumentNode> {
    Ok(match raw {
        RawArgument::Literal { literal } => {
            if literal.contains('\0') {
                bail!("literal argument cannot contain a NUL byte");
            }
            ArgumentNode::Literal { literal }
        }
        RawArgument::Parameter { parameter } => {
            let spec = parameters.get(&parameter).ok_or_else(|| {
                anyhow!("argument mapping references undeclared parameter {parameter:?}")
            })?;
            if spec.kind == ParameterType::List {
                bail!("list parameter {parameter:?} must use spread");
            }
            ArgumentNode::Parameter { parameter }
        }
        RawArgument::Spread { spread } => {
            let spec = parameters.get(&spread).ok_or_else(|| {
                anyhow!("argument mapping references undeclared parameter {spread:?}")
            })?;
            if spec.kind != ParameterType::List {
                bail!("spread parameter {spread:?} must be a list");
            }
            ArgumentNode::Spread { spread }
        }
        RawArgument::WhenPresent {
            when_present,
            append,
        } => {
            if !parameters.contains_key(&when_present) {
                bail!("conditional references undeclared parameter {when_present:?}");
            }
            ArgumentNode::WhenPresent {
                when_present,
                append: append
                    .into_iter()
                    .map(|n| convert_argument(n, parameters))
                    .collect::<Result<_>>()?,
            }
        }
    })
}

fn validate_argument_usage(manifest: &Manifest) -> Result<()> {
    fn visit<'a>(
        nodes: &'a [ArgumentNode],
        guards: &mut Vec<&'a str>,
        manifest: &Manifest,
    ) -> Result<()> {
        for node in nodes {
            match node {
                ArgumentNode::Literal { .. } => {}
                ArgumentNode::Parameter { parameter }
                | ArgumentNode::Spread { spread: parameter } => {
                    if !manifest.parameters[parameter].required
                        && !guards.contains(&parameter.as_str())
                    {
                        bail!("optional parameter {parameter:?} must be emitted within its own when_present condition");
                    }
                }
                ArgumentNode::WhenPresent {
                    when_present,
                    append,
                } => {
                    guards.push(when_present);
                    visit(append, guards, manifest)?;
                    guards.pop();
                }
            }
        }
        Ok(())
    }
    visit(&manifest.arguments, &mut Vec::new(), manifest)
}

impl std::fmt::Display for ParameterType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::List => "list",
            Self::Scalar => "string, integer, or boolean",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_manifest(dir: &Path, name: &str, json: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn catalog_resolves_canonical_names_and_phrases() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "one.json",
            r#"{"name":"say","phrases":["say text"],"binary":"printf","arguments":[]}"#,
        );
        let registry = Registry::load(dir.path()).unwrap();
        assert_eq!(registry.resolve("say").unwrap().binary, "printf");
        assert_eq!(registry.resolve("say text").unwrap().name, "say");
        assert_eq!(registry.manifests().len(), 1);
        assert!(registry.resolve("missing").is_err());
    }

    #[test]
    fn rejects_unknown_fields_bad_mappings_and_duplicate_aliases() {
        let dir = tempdir().unwrap();
        let values_dir = tempdir().unwrap();
        let path = write_manifest(
            values_dir.path(),
            "bad.json",
            r#"{"name":"x","binary":"x","surprise":true,"arguments":[]}"#,
        );
        assert!(Registry::load(&path).is_err());

        let path = write_manifest(
            dir.path(),
            "bad.json",
            r#"{"name":"x","binary":"x","parameters":{"p":{"type":"string"}},"arguments":[{"parameter":"nope"}]}"#,
        );
        assert!(Registry::load(&path).is_err());

        write_manifest(
            dir.path(),
            "one.json",
            r#"{"name":"x","phrases":["same"],"binary":"x","arguments":[]}"#,
        );
        write_manifest(
            dir.path(),
            "two.json",
            r#"{"name":"y","phrases":["same"],"binary":"y","arguments":[]}"#,
        );
        assert!(Registry::load(dir.path()).is_err());
    }

    #[test]
    fn validates_references_then_builds_conditional_spread_argv_without_shell_expansion() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "commit.json",
            r#"{
          "name":"commit","binary":"git",
          "parameters":{"message":{"type":"string","required":true},"files":{"type":"list","items":"string","min_items":1}},
          "arguments":[{"literal":"commit"},{"literal":"-m"},{"parameter":"message"},{"when_present":"files","append":[{"literal":"--only"},{"literal":"--"},{"spread":"files"}]}]
        }"#,
        );
        let manifest = Registry::load(dir.path())
            .unwrap()
            .resolve("commit")
            .unwrap()
            .clone();
        let invocation = Invocation {
            operation: "commit".into(),
            direct: false,
            parameters: BTreeMap::from([
                ("message".into(), Value::Reference("message".into())),
                (
                    "files".into(),
                    Value::List(vec![Value::Reference("files".into())]),
                ),
            ]),
            binding: None,
            cache: false,
            status_binding: None,
        };
        assert!(manifest.validate(&invocation).is_ok());
        let values = BTreeMap::from([
            ("message".into(), Value::String("hello; echo unsafe".into())),
            (
                "files".into(),
                Value::List(vec![
                    Value::String("file with spaces.txt".into()),
                    Value::String("--flag".into()),
                ]),
            ),
        ]);
        assert_eq!(
            manifest.build_arguments(&values).unwrap(),
            [
                "commit",
                "-m",
                "hello; echo unsafe",
                "--only",
                "--",
                "file with spaces.txt",
                "--flag"
            ]
        );
        assert!(manifest
            .build_arguments(&BTreeMap::from([
                ("message".into(), Value::String("ok".into())),
                ("files".into(), Value::List(vec![]))
            ]))
            .is_err());
        assert!(manifest
            .build_arguments(&BTreeMap::from([("message".into(), Value::Integer(7))]))
            .is_err());
    }

    #[test]
    fn catches_missing_and_mistyped_invocation_parameters() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "say.json",
            r#"{"name":"say","binary":"printf","parameters":{"text":{"type":"string","required":true}},"arguments":[{"parameter":"text"}]}"#,
        );
        let manifest = Registry::load(dir.path())
            .unwrap()
            .resolve("say")
            .unwrap()
            .clone();
        let missing = Invocation {
            operation: "say".into(),
            direct: false,
            parameters: BTreeMap::new(),
            binding: None,
            cache: false,
            status_binding: None,
        };
        assert!(manifest.validate(&missing).is_err());
        let wrong = Invocation {
            operation: "say".into(),
            direct: false,
            parameters: BTreeMap::from([("text".into(), Value::Integer(1))]),
            binding: None,
            cache: false,
            status_binding: None,
        };
        assert!(manifest.validate(&wrong).is_err());
    }

    #[test]
    fn list_item_references_are_deferred_until_values_are_resolved() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "files.json",
            r#"{"name":"files","binary":"cat","parameters":{"paths":{"type":"list","items":"string","required":true}},"arguments":[{"spread":"paths"}]}"#,
        );
        let manifest = Registry::load(dir.path())
            .unwrap()
            .resolve("files")
            .unwrap()
            .clone();
        let invocation = Invocation {
            operation: "files".into(),
            direct: false,
            parameters: BTreeMap::from([(
                "paths".into(),
                Value::List(vec![Value::Reference("path".into())]),
            )]),
            binding: None,
            cache: false,
            status_binding: None,
        };
        assert!(manifest.validate(&invocation).is_ok());
        assert!(manifest
            .build_arguments(&BTreeMap::from([(
                "paths".into(),
                Value::List(vec![Value::Reference("path".into())]),
            )]))
            .is_err());
        assert!(manifest
            .build_arguments(&BTreeMap::from([(
                "paths".into(),
                Value::List(vec![Value::String("valid.txt".into())]),
            )]))
            .is_ok());
    }

    #[test]
    fn references_only_defer_for_string_destinations() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "typed.json",
            r#"{"name":"typed","binary":"tool","parameters":{"text":{"type":"string"},"number":{"type":"integer"},"flag":{"type":"boolean"},"strings":{"type":"list","items":"string"},"numbers":{"type":"list","items":"integer"}},"arguments":[]}"#,
        );
        let manifest = Registry::load(dir.path())
            .unwrap()
            .resolve("typed")
            .unwrap()
            .clone();
        let invocation = |name: &str, value: Value| Invocation {
            operation: "typed".into(),
            direct: false,
            parameters: BTreeMap::from([(name.into(), value)]),
            binding: None,
            cache: false,
            status_binding: None,
        };

        assert!(manifest
            .validate(&invocation("text", Value::Reference("result".into())))
            .is_ok());
        assert!(manifest
            .validate(&invocation(
                "strings",
                Value::List(vec![Value::Reference("result".into())]),
            ))
            .is_ok());
        for (name, value) in [
            ("number", Value::Reference("result".into())),
            ("flag", Value::Reference("result".into())),
            ("strings", Value::Reference("result".into())),
            (
                "numbers",
                Value::List(vec![Value::Reference("result".into())]),
            ),
        ] {
            assert!(manifest.validate(&invocation(name, value)).is_err());
        }
        assert!(manifest
            .build_arguments(&BTreeMap::from([(
                "text".into(),
                Value::Reference("result".into()),
            )]))
            .is_err());
    }

    #[test]
    fn optional_parameter_must_be_guarded_by_its_own_condition() {
        let dir = tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            "bad-guard.json",
            r#"{"name":"bad","binary":"tool","parameters":{"first":{"type":"string"},"second":{"type":"string"}},"arguments":[{"when_present":"second","append":[{"parameter":"first"}]}]}"#,
        );
        assert!(Registry::load(&path).is_err());
    }

    #[test]
    fn unix_cli_manifests_build_plain_argv_and_resolve_tail_alias() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let registry = Registry::load(&manifest_dir).unwrap();

        assert_eq!(registry.resolve("tail").unwrap().name, "tail");
        assert_eq!(registry.resolve("follow file").unwrap().name, "tail.follow");
        assert_eq!(
            registry
                .resolve("tail")
                .unwrap()
                .build_arguments(&BTreeMap::from([(
                    "path".into(),
                    Value::String("file".into()),
                )]))
                .unwrap(),
            ["--", "file"]
        );
        assert_eq!(
            registry
                .resolve("tail")
                .unwrap()
                .build_arguments(&BTreeMap::from([
                    (
                        "flags".into(),
                        Value::List(vec![
                            Value::String("-n".into()),
                            Value::String("0".into()),
                            Value::String("-F".into()),
                        ]),
                    ),
                    ("path".into(), Value::String("file".into())),
                ]))
                .unwrap(),
            ["-n", "0", "-F", "--", "file"]
        );
        assert_eq!(
            registry
                .resolve("tail.follow")
                .unwrap()
                .build_arguments(&BTreeMap::from([(
                    "path".into(),
                    Value::String("file".into()),
                )]))
                .unwrap(),
            ["-n", "0", "-F", "--", "file"]
        );
        assert_eq!(registry.resolve("cd").unwrap().builtin, Some(Builtin::Cd));
        assert_eq!(
            registry
                .resolve("cd")
                .unwrap()
                .build_arguments(&BTreeMap::from([(
                    "path".into(),
                    Value::String("folder with spaces".into()),
                )]))
                .unwrap(),
            ["folder with spaces"]
        );
        assert!(registry
            .resolve("pwd")
            .unwrap()
            .build_arguments(&BTreeMap::new())
            .unwrap()
            .is_empty());
        assert_eq!(
            registry
                .resolve("ls")
                .unwrap()
                .build_arguments(&BTreeMap::from([
                    (
                        "flags".into(),
                        Value::List(vec![Value::String("-a".into())]),
                    ),
                    ("path".into(), Value::String("-leading-name".into()),),
                ]))
                .unwrap(),
            ["-a", "--", "-leading-name"]
        );
        assert_eq!(
            registry
                .resolve("wc")
                .unwrap()
                .build_arguments(&BTreeMap::from([(
                    "flags".into(),
                    Value::List(vec![Value::String("-l".into())]),
                )]))
                .unwrap(),
            ["-l"]
        );
        assert!(!registry.resolve("wc").unwrap().cacheable);
        assert_eq!(
            registry
                .resolve("tr")
                .unwrap()
                .build_arguments(&BTreeMap::from([
                    ("from".into(), Value::String("[:lower:]".into())),
                    ("to".into(), Value::String("[:upper:]".into())),
                ]))
                .unwrap(),
            ["[:lower:]", "[:upper:]"]
        );
    }

    #[test]
    fn cd_builtin_rejects_non_path_argument_mappings() {
        let dir = tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            "bad-cd.json",
            r#"{"name":"cd","binary":"cd","builtin":"cd","parameters":{"path":{"type":"string","required":true}},"arguments":[{"literal":"ignored"}]}"#,
        );
        assert!(Registry::load(&path).is_err());
    }

    #[test]
    fn cat_path_is_optional_and_is_passed_as_a_positional_argument() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let registry = Registry::load(&manifest_dir).unwrap();
        let cat = registry.resolve("cat").unwrap();

        assert!(cat.accepts_stdin);
        assert!(!cat.cacheable);
        assert_eq!(
            cat.build_arguments(&BTreeMap::from([(
                "path".into(),
                Value::String("-input.txt".into()),
            )]))
            .unwrap(),
            ["--", "-input.txt"]
        );
        assert_eq!(cat.build_arguments(&BTreeMap::new()).unwrap(), ["--"]);
        assert_eq!(
            cat.build_arguments(&BTreeMap::from([
                ("path".into(), Value::String("first.txt".into())),
                (
                    "files".into(),
                    Value::List(vec![
                        Value::String("-second.txt".into()),
                        Value::String("third.txt".into())
                    ])
                ),
            ]))
            .unwrap(),
            ["--", "first.txt", "-second.txt", "third.txt"]
        );
    }

    #[test]
    fn rejects_nul_in_binary_literals_and_string_values() {
        let dir = tempdir().unwrap();
        for (filename, manifest) in [
            (
                "nul-binary.json",
                r#"{"name":"bad","binary":"tool\u0000bad","arguments":[]}"#,
            ),
            (
                "nul-argument.json",
                r#"{"name":"bad","binary":"tool","arguments":[{"literal":"bad\u0000arg"}]}"#,
            ),
        ] {
            let path = write_manifest(dir.path(), filename, manifest);
            assert!(Registry::load(&path).is_err(), "accepted {filename}");
        }

        let path = write_manifest(
            dir.path(),
            "strings.json",
            r#"{"name":"strings","binary":"tool","parameters":{"text":{"type":"string"},"items":{"type":"list","items":"string"}},"arguments":[]}"#,
        );
        let manifest = Registry::load(&path)
            .unwrap()
            .resolve("strings")
            .unwrap()
            .clone();
        for value in [
            Value::String("bad\0text".into()),
            Value::List(vec![Value::String("bad\0item".into())]),
        ] {
            let parameter = if matches!(value, Value::List(_)) {
                "items"
            } else {
                "text"
            };
            let invocation = Invocation {
                operation: "strings".into(),
                direct: false,
                parameters: BTreeMap::from([(parameter.into(), value.clone())]),
                binding: None,
                cache: false,
                status_binding: None,
            };
            assert!(manifest.validate(&invocation).is_err());
            assert!(manifest
                .build_arguments(&BTreeMap::from([(parameter.into(), value)]))
                .is_err());
        }
    }
    #[test]
    fn opt_manifest_borrowed_argv_matches_owned_for_all_scalar_and_spread_values() {
        let dir = tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            "borrowed.json",
            r#"{"name":"check","binary":"tool","parameters":{"text":{"type":"string","required":true},"number":{"type":"integer","required":true},"flag":{"type":"boolean","required":true},"items":{"type":"list","items":"string","required":true},"extra":{"type":"list","items":"string"}},"arguments":[{"literal":"π"},{"parameter":"text"},{"parameter":"number"},{"parameter":"flag"},{"spread":"items"},{"when_present":"extra","append":[{"literal":"extra"},{"spread":"extra"}]}]}"#,
        );
        let registry = Registry::load(&path).unwrap();
        let manifest = registry.resolve("check").unwrap();
        let values = BTreeMap::from([
            ("text".into(), Value::String(String::new())),
            ("number".into(), Value::Integer(-7)),
            ("flag".into(), Value::Bool(false)),
            (
                "items".into(),
                Value::List(vec![
                    Value::String(String::new()),
                    Value::String("雪".into()),
                ]),
            ),
            (
                "extra".into(),
                Value::List(vec![Value::String("🌱".into())]),
            ),
        ]);
        let expected = ["π", "", "-7", "false", "", "雪", "extra", "🌱"];
        assert_eq!(
            manifest.build_arguments(&values).unwrap(),
            expected.map(str::to_owned)
        );
        let borrowed = manifest.build_arguments_borrowed(&values).unwrap();
        let rendered: Vec<&str> = borrowed.iter().map(|argument| argument.as_ref()).collect();
        assert_eq!(rendered, expected);
        assert!(matches!(
            &borrowed[0],
            Cow::Borrowed(value) if *value == "π"
        ));
        assert!(matches!(
            &borrowed[2],
            Cow::Owned(value) if value.as_str() == "-7"
        ));

        let mut invalid = values.clone();
        invalid.insert("text".into(), Value::String("bad\0text".into()));
        assert_eq!(
            manifest.build_arguments(&invalid).unwrap_err().to_string(),
            manifest
                .build_arguments_borrowed(&invalid)
                .unwrap_err()
                .to_string()
        );
    }
}
