//! Versioned, declarative applet input, output, outcome, and lifecycle contracts.
//!
//! These types describe boundaries. They do not grant a sandbox or enforce
//! capabilities by themselves; hosts decide which declared capabilities to
//! permit.

use crate::ast::{InputType, Value};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// How a host should connect one standard stream for a manifest's native
/// command. An omitted stream in [`StdioContract`] keeps the host's legacy
/// behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StdioDisposition {
    Capture,
    Discard,
    Inherit,
}

/// Optional manifest defaults for stdout and stderr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdioContract {
    #[serde(default, deserialize_with = "deserialize_present")]
    pub stdout: Option<StdioDisposition>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub stderr: Option<StdioDisposition>,
}

/// Per-applet environment changes. Values are part of the manifest execution
/// identity, while Debug and discovery output expose names only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppletEnvironment {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub unset: BTreeSet<String>,
}

impl std::fmt::Debug for AppletEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppletEnvironment")
            .field("set", &self.set.keys().collect::<Vec<_>>())
            .field("unset", &self.unset)
            .finish()
    }
}

impl AppletEnvironment {
    pub(crate) fn validate(&self) -> Result<()> {
        for (name, value) in &self.set {
            if !valid_env_name(name) {
                bail!("environment variable name {name:?} is invalid");
            }
            if value.contains('\0') {
                bail!("environment variable {name:?} contains a NUL byte");
            }
        }
        for name in &self.unset {
            if !valid_env_name(name) {
                bail!("environment variable name {name:?} is invalid");
            }
            if self.set.contains_key(name) {
                bail!("environment variable {name:?} cannot be both set and unset");
            }
        }
        Ok(())
    }
}

impl StdioContract {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.stdout.is_none() && self.stderr.is_none() {
            bail!("stdio must declare stdout, stderr, or both");
        }
        Ok(())
    }
}

/// The backend selected by a versioned applet contract.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppletBackend {
    /// Launch the manifest's binary as a native command.
    #[default]
    Command,
    /// Dispatch to a built-in applet implementation in the host process.
    InProcess,
    /// Keep a JSON-lines worker alive across calls.
    JsonLinesWorker,
    /// Execute a finite program entry selected from a verified local module,
    /// or supervise a scheduled/listener module service when paired with its v2
    /// interface.
    ProgramModule,
    /// Invoke the versioned `execute.applet/1` HTTP protocol.
    Remote,
}

/// The supported applet lifecycle protocols.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Lifecycle {
    #[serde(rename = "oneshot")]
    #[default]
    OneShot,
    /// Run an interface-2 `.program` schedule or listener under the host's
    /// service supervisor. The source trigger selects the service driver.
    ProgramService,
    #[serde(rename = "json_lines")]
    JsonLines {
        /// Arguments passed when the worker process starts.
        #[serde(default = "default_worker_startup_arguments")]
        startup_arguments: Vec<String>,
        /// Maximum time in milliseconds to wait for a worker response.
        #[serde(default = "default_worker_timeout_ms")]
        timeout_ms: u64,
        /// Maximum encoded request or response frame size.
        #[serde(default = "default_worker_max_frame_bytes")]
        max_frame_bytes: usize,
        /// Maximum retained bytes from worker stderr.
        #[serde(default = "default_worker_max_stderr_bytes")]
        max_stderr_bytes: usize,
    },
}

fn default_worker_startup_arguments() -> Vec<String> {
    vec!["--worker".into()]
}

fn default_worker_timeout_ms() -> u64 {
    30_000
}

fn default_worker_max_frame_bytes() -> usize {
    8 * 1_048_576
}

fn default_worker_max_stderr_bytes() -> usize {
    64 * 1_024
}

fn default_remote_timeout_ms() -> u64 {
    30_000
}

fn default_remote_frame_bytes() -> usize {
    1_048_576
}

impl Lifecycle {
    pub fn validate(&self) -> Result<()> {
        if let Self::JsonLines {
            startup_arguments,
            timeout_ms,
            max_frame_bytes,
            max_stderr_bytes,
        } = self
        {
            if *timeout_ms == 0 || *timeout_ms > 300_000 {
                bail!("json_lines timeout_ms must be between 1 and 300000");
            }
            if *max_frame_bytes == 0 || *max_frame_bytes > 64 * 1024 * 1024 {
                bail!("json_lines max_frame_bytes must be between 1 and 67108864");
            }
            if *max_stderr_bytes == 0 || *max_stderr_bytes > 1024 * 1024 {
                bail!("json_lines max_stderr_bytes must be between 1 and 1048576");
            }
            if startup_arguments
                .iter()
                .any(|argument| argument.contains('\0'))
            {
                bail!("json_lines startup arguments cannot contain a NUL byte");
            }
        }
        Ok(())
    }

    pub fn is_json_lines(&self) -> bool {
        matches!(self, Self::JsonLines { .. })
    }

    pub fn is_program_service(&self) -> bool {
        matches!(self, Self::ProgramService)
    }
}

/// Reference to a finite or persistent program exported by a local `.program` module.
/// Paths are resolved relative to the applet manifest and confined to the
/// configured module root by the module loader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramModule {
    pub source: PathBuf,
    pub entry: String,
    pub interface_version: u32,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub package_manifest: Option<PathBuf>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub package_lock: Option<PathBuf>,
}

impl ProgramModule {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.interface_version, 1 | 2) {
            bail!(
                "unsupported program module interface version {}; supported versions are 1 (finite) and 2 (persistent service)",
                self.interface_version
            );
        }
        validate_manifest_relative_path(&self.source, "program module source")?;
        if !valid_port_name(&self.entry) {
            bail!("program module entry must be an identifier");
        }
        if self.package_manifest.is_some() != self.package_lock.is_some() {
            bail!("program module package_manifest and package_lock must be declared together");
        }
        if let Some(path) = &self.package_manifest {
            validate_manifest_relative_path(path, "program module package_manifest")?;
        }
        if let Some(path) = &self.package_lock {
            validate_manifest_relative_path(path, "program module package_lock")?;
        }
        Ok(())
    }
}

/// Explicit source for a bearer credential used by a remote applet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteAuthSource {
    BearerEnv { name: String },
    BearerFile { path: PathBuf },
}

/// Transport settings for one `execute.applet/1` endpoint. This declaration
/// identifies the remote boundary; hosts still validate URL policy before
/// reading credentials or opening a connection.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteEndpointConfig {
    pub endpoint: String,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub auth: Option<RemoteAuthSource>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_loopback_http_fixture: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_discovery: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub descriptor_cache: Option<PathBuf>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub descriptor_sha256: Option<String>,
    #[serde(default = "default_remote_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_remote_frame_bytes")]
    pub max_request_bytes: usize,
    #[serde(default = "default_remote_frame_bytes")]
    pub max_response_bytes: usize,
}

impl std::fmt::Debug for RemoteEndpointConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteEndpointConfig")
            .field("endpoint", &"<redacted>")
            .field(
                "auth",
                &self.auth.as_ref().map(|auth| match auth {
                    RemoteAuthSource::BearerEnv { .. } => "bearer_env",
                    RemoteAuthSource::BearerFile { .. } => "bearer_file",
                }),
            )
            .field(
                "allow_loopback_http_fixture",
                &self.allow_loopback_http_fixture,
            )
            .field("allow_discovery", &self.allow_discovery)
            .field(
                "descriptor_cache",
                &self.descriptor_cache.as_ref().map(|_| "<configured>"),
            )
            .field(
                "descriptor_sha256",
                &self.descriptor_sha256.as_ref().map(|_| "<configured>"),
            )
            .field("timeout_ms", &self.timeout_ms)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl RemoteEndpointConfig {
    pub fn validate_declaration(&self) -> Result<()> {
        if self.endpoint.is_empty() || self.endpoint.contains('\0') {
            bail!("remote endpoint must be a nonempty string without NUL bytes");
        }
        if self.descriptor_cache.is_some() != self.descriptor_sha256.is_some() {
            bail!("remote descriptor_cache and descriptor_sha256 must be declared together");
        }
        if let Some(path) = &self.descriptor_cache {
            validate_manifest_relative_path(path, "remote descriptor_cache")?;
        }
        if let Some(digest) = &self.descriptor_sha256 {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                bail!("remote descriptor_sha256 must be 64 lowercase hexadecimal characters");
            }
        }
        if self.timeout_ms == 0 || self.timeout_ms > 300_000 {
            bail!("remote timeout_ms must be between 1 and 300000");
        }
        if self.max_request_bytes == 0
            || self.max_request_bytes > 16 * 1_048_576
            || self.max_response_bytes == 0
            || self.max_response_bytes > 16 * 1_048_576
        {
            bail!("remote frame limits must be between 1 and 16777216 bytes");
        }
        match &self.auth {
            Some(RemoteAuthSource::BearerEnv { name }) if valid_env_name(name) => {}
            Some(RemoteAuthSource::BearerFile { path }) => {
                if path.as_os_str().is_empty() || path.to_string_lossy().contains('\0') {
                    bail!("remote bearer file path must be nonempty and contain no NUL bytes");
                }
            }
            Some(RemoteAuthSource::BearerEnv { .. }) => {
                bail!("remote bearer environment name is invalid")
            }
            None => {}
        }
        Ok(())
    }
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Validate a relative reference that is resolved from a manifest origin.
/// Parent components stay visible so the owning loader can resolve and apply
/// any required canonical-root policy before opening the path.
fn validate_manifest_relative_path(path: &std::path::Path, label: &str) -> Result<()> {
    let path_text = path.to_string_lossy();
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path_text.contains('\0')
        || path_text.len() > 4096
    {
        bail!("{label} must be a nonempty relative path without NUL bytes and at most 4096 bytes");
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Prefix(_)
        )
    }) {
        bail!("{label} cannot contain root path components");
    }
    Ok(())
}

/// Namespaced machine-readable capability identifier. The vocabulary is
/// extensible; hosts exact-match these IDs against their grant policy.
pub type Capability = String;

/// Effects use the same extensible identifier syntax as capabilities. A
/// declared effect must also be present in the capability set.
pub type Effect = String;

/// How the output bytes become a typed value at the explicit contract boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputEncoding {
    Raw,
    Json,
}

/// Declared information sensitivity for one applet port. Unknown remains a
/// distinct, conservative value rather than being treated as public data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    Public,
    Internal,
    Confidential,
    Secret,
    #[default]
    Unknown,
}

impl Sensitivity {
    pub fn rank(self) -> u8 {
        match self {
            Self::Public => 0,
            Self::Internal => 1,
            Self::Confidential => 2,
            Self::Secret => 3,
            Self::Unknown => 4,
        }
    }
}

/// A top-level output schema. `encoding` is explicit whenever a contract
/// declares an output schema; an omitted output schema means raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSchema {
    #[serde(rename = "type")]
    pub kind: OutputType,
    pub encoding: OutputEncoding,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub items: Option<Box<ValueSchema>>,
    #[serde(default)]
    pub fields: BTreeMap<String, ValueSchema>,
    #[serde(default)]
    pub required: Vec<String>,
    /// Allowed string values when `type` is `text`.
    #[serde(
        default,
        rename = "enum",
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub enum_values: Option<Vec<String>>,
    /// Remove exactly one final LF from raw UTF-8 text or path output. No other
    /// whitespace or line ending is normalized.
    #[serde(default)]
    pub trim_final_lf: bool,
}

// This is only a fast-path eligibility bound. Deeper values go through the
// original strict byte parser, which remains authoritative for recursion
// limits and therefore preserves acceptance and error behavior.
#[cfg(any(not(target_arch = "wasm32"), test))]
const MAX_PREDECODED_JSON_CONTAINER_DEPTH: usize = 64;

#[cfg(any(not(target_arch = "wasm32"), test))]
fn predecoded_json_depth_is_safe(value: &serde_json::Value) -> bool {
    fn visit(value: &serde_json::Value, depth: usize) -> bool {
        match value {
            serde_json::Value::Array(items) => {
                if depth >= MAX_PREDECODED_JSON_CONTAINER_DEPTH {
                    return false;
                }
                items.iter().all(|item| visit(item, depth + 1))
            }
            serde_json::Value::Object(fields) => {
                if depth >= MAX_PREDECODED_JSON_CONTAINER_DEPTH {
                    return false;
                }
                fields.values().all(|field| visit(field, depth + 1))
            }
            _ => true,
        }
    }

    visit(value, 0)
}

/// Types supported at an applet output boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputType {
    Bytes,
    Text,
    Path,
    Integer,
    /// Any finite JSON numeric value, including integers and fractions.
    Number,
    Boolean,
    Json,
    Record,
    List,
}

/// A recursive JSON field schema used inside a record output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueSchema {
    #[serde(rename = "type")]
    pub kind: OutputType,
    /// Permit JSON null in addition to `kind`. JSON itself already includes
    /// null; this flag is useful for typed union fields such as envelopes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub nullable: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub items: Option<Box<ValueSchema>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, ValueSchema>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    /// Allowed string values when `type` is `text`.
    #[serde(
        default,
        rename = "enum",
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub enum_values: Option<Vec<String>>,
}

/// A named typed port on a versioned applet interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppletPort {
    pub schema: ValueSchema,
    pub delivery: PortDelivery,
    #[serde(default, skip_serializing_if = "is_unknown_sensitivity")]
    pub sensitivity: Sensitivity,
    /// JSON Pointer into the applet's canonical output value. Valid only for
    /// stdout-backed output ports.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub pointer: Option<String>,
    /// Explicit framing for the special `stdin` input port. Other named
    /// parameter and event ports carry typed values directly.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub encoding: Option<OutputEncoding>,
}

fn is_unknown_sensitivity(value: &Sensitivity) -> bool {
    *value == Sensitivity::Unknown
}

/// Whether a port carries one value, a stream of data records, or events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortDelivery {
    Single,
    Records,
    Events,
}

/// Named port namespaces for a versioned applet interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppletPorts {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, AppletPort>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, AppletPort>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub events: BTreeMap<String, AppletPort>,
}

impl ValueSchema {
    /// Validate recursive record/list structure and semantic path constraints.
    pub fn validate(&self) -> Result<()> {
        validate_schema_parts(
            self.kind,
            self.items.as_deref(),
            &self.fields,
            &self.required,
            self.enum_values.as_deref(),
            self.nullable,
            true,
        )
    }

    pub fn json_schema(&self) -> serde_json::Value {
        schema_json(
            self.kind,
            self.items.as_deref(),
            &self.fields,
            &self.required,
            self.enum_values.as_deref(),
            self.nullable,
        )
    }

    /// Validate an AST value against this recursive schema. Byte payloads are
    /// carried separately and cannot be represented by `Value`.
    pub fn validate_value(&self, value: &Value) -> Result<()> {
        self.validate()?;
        validate_ast_value(self, value)
    }

    /// Return whether every value produced by this schema is accepted by
    /// `target`, without a conversion. This relation is directional: for
    /// example, Integer is assignable to Number, while Number is not
    /// assignable to Integer.
    pub fn is_assignable_to(&self, target: &Self) -> bool {
        schema_is_assignable(self, target)
    }
}

fn schema_is_assignable(source: &ValueSchema, target: &ValueSchema) -> bool {
    let kinds_assignable = source.kind == target.kind
        || matches!(
            (source.kind, target.kind),
            (OutputType::Integer, OutputType::Number | OutputType::Json)
                | (OutputType::Number, OutputType::Json)
                | (
                    OutputType::Text
                        | OutputType::Path
                        | OutputType::Boolean
                        | OutputType::Record
                        | OutputType::List,
                    OutputType::Json
                )
        );
    if !kinds_assignable {
        return false;
    }
    let source_may_be_null = source.nullable || source.kind == OutputType::Json;
    let target_accepts_null = target.nullable || target.kind == OutputType::Json;
    if source_may_be_null && !target_accepts_null {
        return false;
    }
    if source.kind != target.kind {
        return true;
    }
    match source.kind {
        OutputType::Text => {
            enum_assignable(source.enum_values.as_deref(), target.enum_values.as_deref())
        }
        OutputType::Record => {
            // A target may declare optional fields the source never emits.
            // Conversely, every source field must be accepted by the target,
            // and any target-required field must always be present in source
            // values.
            source.fields.iter().all(|(name, source_field)| {
                target
                    .fields
                    .get(name)
                    .is_some_and(|target_field| schema_is_assignable(source_field, target_field))
            }) && target
                .required
                .iter()
                .all(|name| source.required.contains(name))
        }
        OutputType::List => match (source.items.as_deref(), target.items.as_deref()) {
            (Some(source_item), Some(target_item)) => {
                schema_is_assignable(source_item, target_item)
            }
            _ => false,
        },
        _ => true,
    }
}

fn enum_assignable(source: Option<&[String]>, target: Option<&[String]>) -> bool {
    match (source, target) {
        (_, None) => true,
        (Some(source), Some(target)) => source.iter().all(|value| target.contains(value)),
        (None, Some(_)) => false,
    }
}

fn validate_enum_schema(kind: OutputType, values: Option<&[String]>) -> Result<()> {
    let Some(values) = values else {
        return Ok(());
    };
    if kind != OutputType::Text {
        bail!("enum constraints apply only to text schemas");
    }
    if values.is_empty() {
        bail!("text enum constraints must contain at least one value");
    }
    let mut unique = BTreeSet::new();
    if values.iter().any(|value| !unique.insert(value)) {
        bail!("text enum constraints must not contain duplicate values");
    }
    Ok(())
}

fn validate_enum_value(values: Option<&[String]>, value: &str) -> Result<()> {
    if values.is_some_and(|values| !values.iter().any(|allowed| allowed == value)) {
        bail!("text value {value:?} is outside the declared enum");
    }
    Ok(())
}

fn validate_ast_value(schema: &ValueSchema, value: &Value) -> Result<()> {
    if matches!(value, Value::Null) && schema.nullable {
        return Ok(());
    }
    match (schema.kind, value) {
        (OutputType::Bytes, _) => {
            bail!("bytes use the raw byte transport and cannot be represented as a Value")
        }
        (OutputType::Text, Value::String(text)) => {
            validate_enum_value(schema.enum_values.as_deref(), text)
        }
        (OutputType::Path, Value::String(path)) => {
            if path.is_empty() || path.contains('\0') {
                bail!("path value must be nonempty and contain no NUL bytes");
            }
            Ok(())
        }
        (OutputType::Integer, Value::Integer(_))
        | (OutputType::Number, Value::Integer(_) | Value::Number(_))
        | (OutputType::Boolean, Value::Bool(_)) => Ok(()),
        (OutputType::Json, value) => crate::structured::to_json(value)
            .context("validating value as JSON")
            .map(|_| ()),
        (OutputType::Record, Value::Record(values)) => {
            for name in &schema.required {
                if !values.contains_key(name) {
                    bail!("record is missing required field {name:?}");
                }
            }
            for (name, value) in values {
                let Some(field_schema) = schema.fields.get(name) else {
                    bail!("record contains undeclared field {name:?}");
                };
                validate_ast_value(field_schema, value)
                    .with_context(|| format!("validating record field {name:?}"))?;
            }
            Ok(())
        }
        (OutputType::List, Value::List(values)) => {
            let item_schema = schema
                .items
                .as_deref()
                .ok_or_else(|| anyhow!("list schema is missing items"))?;
            for (index, value) in values.iter().enumerate() {
                validate_ast_value(item_schema, value)
                    .with_context(|| format!("validating list item {index}"))?;
            }
            Ok(())
        }
        (kind, _) => bail!("expected {kind:?} value"),
    }
}

impl AppletPort {
    pub(crate) fn validate(&self) -> Result<()> {
        self.schema.validate()?;
        if let Some(pointer) = &self.pointer {
            json_pointer_segments(pointer)?;
        }
        Ok(())
    }
}

impl AppletPorts {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.inputs.is_empty() && self.outputs.is_empty() && self.events.is_empty() {
            bail!("contract ports must declare at least one input, output, or event");
        }
        for namespace in [&self.inputs, &self.outputs, &self.events] {
            for (name, port) in namespace {
                if !valid_port_name(name) {
                    bail!("contract port names must be identifiers; invalid name {name:?}");
                }
                port.validate()?;
            }
        }
        for (name, port) in &self.inputs {
            if port.pointer.is_some() {
                bail!("input ports cannot declare output JSON pointers");
            }
            if name == "stdin" {
                validate_stdin_port(port)?;
            } else if port.encoding.is_some() {
                bail!("only the special stdin input port can declare an encoding");
            }
        }
        for port in self.outputs.values() {
            if port.encoding.is_some() {
                bail!("output ports use the contract output encoding, not per-port encoding");
            }
        }
        for port in self.events.values() {
            if port.delivery != PortDelivery::Events {
                bail!("event namespace ports must use events delivery");
            }
            if port.pointer.is_some() || port.encoding.is_some() {
                bail!("event ports cannot declare stdout JSON pointers or stdin encoding");
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_stdin_port(port: &AppletPort) -> Result<()> {
    match (port.schema.kind, port.encoding) {
        (OutputType::Bytes, Some(OutputEncoding::Raw)) => {}
        (OutputType::Bytes, Some(OutputEncoding::Json)) => {
            bail!("bytes stdin requires raw encoding")
        }
        (OutputType::Text | OutputType::Path, Some(OutputEncoding::Raw | OutputEncoding::Json)) => {
        }
        (
            OutputType::Integer
            | OutputType::Number
            | OutputType::Boolean
            | OutputType::Json
            | OutputType::Record
            | OutputType::List,
            Some(OutputEncoding::Json),
        ) => {}
        (
            OutputType::Bytes
            | OutputType::Text
            | OutputType::Path
            | OutputType::Integer
            | OutputType::Number
            | OutputType::Boolean
            | OutputType::Json
            | OutputType::Record
            | OutputType::List,
            None,
        ) => {
            bail!("stdin input requires an explicit encoding")
        }
        (
            OutputType::Integer
            | OutputType::Number
            | OutputType::Boolean
            | OutputType::Json
            | OutputType::Record
            | OutputType::List,
            Some(OutputEncoding::Raw),
        ) => {
            bail!("structured stdin input requires JSON encoding")
        }
    }
    Ok(())
}

fn valid_port_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric() || ch == '-')
}

/// Parse RFC 6901 JSON Pointer syntax, returning decoded path segments.
pub(crate) fn json_pointer_segments(pointer: &str) -> Result<Vec<String>> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(pointer) = pointer.strip_prefix('/') else {
        bail!("JSON pointer must be empty or begin with '/'");
    };
    pointer
        .split('/')
        .map(|segment| {
            let mut decoded = String::with_capacity(segment.len());
            let mut chars = segment.chars();
            while let Some(ch) = chars.next() {
                if ch != '~' {
                    decoded.push(ch);
                    continue;
                }
                match chars.next() {
                    Some('0') => decoded.push('~'),
                    Some('1') => decoded.push('/'),
                    _ => bail!("JSON pointer contains an invalid escape"),
                }
            }
            Ok(decoded)
        })
        .collect()
}

/// Infer a schema reached by a JSON Pointer. `None` means traversal entered
/// an unconstrained JSON subtree; malformed or impossible static traversal is
/// an error.
pub(crate) fn schema_at_pointer(root: &ValueSchema, pointer: &str) -> Result<Option<ValueSchema>> {
    let segments = json_pointer_segments(pointer)?;
    let mut current = root.clone();
    for segment in segments {
        match current.kind {
            OutputType::Json => return Ok(None),
            OutputType::Record => {
                let Some(next) = current.fields.get(&segment) else {
                    bail!("JSON pointer selects undeclared record field {segment:?}");
                };
                current = next.clone();
            }
            OutputType::List => {
                if segment == "-"
                    || segment.is_empty()
                    || (segment.len() > 1 && segment.starts_with('0'))
                    || !segment.bytes().all(|byte| byte.is_ascii_digit())
                {
                    bail!("JSON pointer list segment must be a canonical nonnegative index");
                }
                segment
                    .parse::<usize>()
                    .context("JSON pointer list index is out of range")?;
                current = *current
                    .items
                    .ok_or_else(|| anyhow!("list schema is missing items"))?;
            }
            _ => bail!("JSON pointer cannot traverse through a scalar value"),
        }
    }
    Ok(Some(current))
}

impl OutputSchema {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_schema_parts(
            self.kind,
            self.items.as_deref(),
            &self.fields,
            &self.required,
            self.enum_values.as_deref(),
            false,
            true,
        )?;
        match (self.kind, self.encoding) {
            (OutputType::Bytes, OutputEncoding::Raw)
            | (OutputType::Text | OutputType::Path, OutputEncoding::Raw | OutputEncoding::Json)
            | (
                OutputType::Integer
                | OutputType::Number
                | OutputType::Boolean
                | OutputType::Json
                | OutputType::Record
                | OutputType::List,
                OutputEncoding::Json,
            ) => {}
            (OutputType::Bytes, OutputEncoding::Json) => {
                bail!("bytes output requires raw encoding")
            }
            (
                OutputType::Integer
                | OutputType::Number
                | OutputType::Boolean
                | OutputType::Json
                | OutputType::Record
                | OutputType::List,
                OutputEncoding::Raw,
            ) => {
                bail!("integer, boolean, json, record, and list output require json encoding")
            }
        }
        if self.trim_final_lf
            && (self.encoding != OutputEncoding::Raw
                || !matches!(self.kind, OutputType::Text | OutputType::Path))
        {
            bail!("trim_final_lf applies only to raw text or path output");
        }
        Ok(())
    }

    pub fn input_kind(&self) -> Option<InputType> {
        match self.kind {
            OutputType::Bytes => None,
            OutputType::Text | OutputType::Path => Some(InputType::Text),
            OutputType::Integer => Some(InputType::Integer),
            OutputType::Number => Some(InputType::Json),
            OutputType::Boolean => Some(InputType::Boolean),
            OutputType::Json | OutputType::Record => Some(InputType::Json),
            OutputType::List => Some(match self.items.as_deref().map(|item| item.kind) {
                Some(OutputType::Text | OutputType::Path) => InputType::TextList,
                _ => InputType::Json,
            }),
        }
    }

    pub fn decode(&self, bytes: &[u8]) -> Result<AppletOutput> {
        match self.kind {
            OutputType::Bytes => Ok(AppletOutput::Bytes(bytes.to_vec())),
            OutputType::Text | OutputType::Path => match self.encoding {
                OutputEncoding::Raw => {
                    let end = if self.trim_final_lf && bytes.last() == Some(&b'\n') {
                        bytes.len() - 1
                    } else {
                        bytes.len()
                    };
                    let value = std::str::from_utf8(&bytes[..end])
                        .context("decoding applet output as UTF-8")?
                        .to_owned();
                    self.decode_string_value(value)
                }
                OutputEncoding::Json => {
                    let value = parse_json_strict(bytes)
                        .context("decoding applet output as JSON string")?;
                    self.decode_json_value(value)
                }
            },
            OutputType::Integer
            | OutputType::Number
            | OutputType::Boolean
            | OutputType::Json
            | OutputType::Record
            | OutputType::List => {
                let json = parse_json_strict(bytes).context("decoding applet output as JSON")?;
                self.decode_json_value(json)
            }
        }
    }

    /// Decode JSON that an in-process applet already produced as a typed
    /// value. This applies the same output-schema checks as the byte decoder
    /// without serializing and parsing the JSON a second time.
    pub(crate) fn decode_json_value(&self, json: serde_json::Value) -> Result<AppletOutput> {
        if self.encoding != OutputEncoding::Json {
            bail!("predecoded JSON output requires json encoding");
        }
        match self.kind {
            OutputType::Text | OutputType::Path => {
                let value = match json {
                    serde_json::Value::String(value) => value,
                    _ => bail!("expected JSON string output"),
                };
                self.decode_string_value(value)
            }
            OutputType::Integer
            | OutputType::Number
            | OutputType::Boolean
            | OutputType::Json
            | OutputType::Record
            | OutputType::List => {
                validate_json_value(
                    self.kind,
                    self.items.as_deref(),
                    &self.fields,
                    &self.required,
                    self.enum_values.as_deref(),
                    false,
                    &json,
                )?;
                Ok(AppletOutput::Value(crate::structured::from_json(json)))
            }
            OutputType::Bytes => bail!("bytes output requires raw encoding"),
        }
    }

    #[cfg(any(not(target_arch = "wasm32"), test))]
    pub(crate) fn decode_json_value_or_bytes(
        &self,
        json: serde_json::Value,
        bytes: &[u8],
    ) -> Result<AppletOutput> {
        if self.encoding == OutputEncoding::Json && predecoded_json_depth_is_safe(&json) {
            self.decode_json_value(json)
        } else {
            self.decode(bytes)
        }
    }

    fn decode_raw_text_value(&self, bytes: &[u8]) -> Result<AppletOutput> {
        let value = std::str::from_utf8(bytes)
            .context("decoding in-process text output as UTF-8")?
            .to_owned();
        self.decode_string_value(value)
    }

    fn decode_string_value(&self, value: String) -> Result<AppletOutput> {
        if self.kind == OutputType::Path {
            if value.is_empty() {
                bail!("path output must be nonempty");
            }
            if value.contains('\0') {
                bail!("path output contains a NUL byte");
            }
        }
        validate_enum_value(self.enum_values.as_deref(), &value)?;
        Ok(AppletOutput::Value(Value::String(value)))
    }

    pub fn json_schema(&self) -> serde_json::Value {
        let mut schema = schema_json(
            self.kind,
            self.items.as_deref(),
            &self.fields,
            &self.required,
            self.enum_values.as_deref(),
            false,
        );
        let object = schema
            .as_object_mut()
            .expect("schema projection is an object");
        object.insert("encoding".into(), serde_json::json!(self.encoding));
        if self.trim_final_lf {
            object.insert("trim_final_lf".into(), serde_json::Value::Bool(true));
        }
        schema
    }
}

fn validate_schema_parts(
    kind: OutputType,
    items: Option<&ValueSchema>,
    fields: &BTreeMap<String, ValueSchema>,
    required: &[String],
    enum_values: Option<&[String]>,
    nullable: bool,
    top_level: bool,
) -> Result<()> {
    if kind != OutputType::Record && (!fields.is_empty() || !required.is_empty()) {
        bail!("fields and required only apply to record schemas");
    }
    if (kind == OutputType::List) != items.is_some() {
        bail!("list schemas require items, and non-list schemas cannot declare items");
    }
    if nullable && kind == OutputType::Bytes {
        bail!("bytes schemas cannot be nullable; use the JSON output kind for null");
    }
    validate_enum_schema(kind, enum_values)?;
    let mut unique = BTreeSet::new();
    for name in required {
        if !fields.contains_key(name) || !unique.insert(name) {
            bail!("record required fields must be declared once in fields");
        }
    }
    for (name, schema) in fields {
        if name.is_empty() {
            bail!("record field names must be nonempty");
        }
        if schema.kind == OutputType::Bytes {
            bail!("bytes cannot be nested in a JSON record");
        }
        validate_schema_parts(
            schema.kind,
            schema.items.as_deref(),
            &schema.fields,
            &schema.required,
            schema.enum_values.as_deref(),
            schema.nullable,
            false,
        )?;
    }
    if let Some(items) = items {
        if items.kind == OutputType::Bytes {
            bail!("bytes cannot be nested in a JSON list");
        }
        validate_schema_parts(
            items.kind,
            items.items.as_deref(),
            &items.fields,
            &items.required,
            items.enum_values.as_deref(),
            items.nullable,
            false,
        )?;
    }
    if top_level && kind == OutputType::Bytes && (!fields.is_empty() || !required.is_empty()) {
        bail!("bytes output cannot declare record fields");
    }
    Ok(())
}

fn schema_json(
    kind: OutputType,
    items: Option<&ValueSchema>,
    fields: &BTreeMap<String, ValueSchema>,
    required: &[String],
    enum_values: Option<&[String]>,
    nullable: bool,
) -> serde_json::Value {
    let mut schema = match kind {
        OutputType::Bytes => serde_json::json!({"type":"bytes"}),
        OutputType::Text => {
            let mut schema = serde_json::json!({"type":"string"});
            if let Some(values) = enum_values {
                schema["enum"] = serde_json::json!(values);
            }
            schema
        }
        OutputType::Path => serde_json::json!({
            "type":"string", "format":"path", "minLength":1,
            "pattern":"^[^\\u0000]+$"
        }),
        OutputType::Integer => serde_json::json!({
            "type":"integer", "minimum":i64::MIN, "maximum":i64::MAX
        }),
        OutputType::Number => serde_json::json!({"type":"number"}),
        OutputType::Boolean => serde_json::json!({"type":"boolean"}),
        OutputType::Json => serde_json::json!({}),
        OutputType::Record => {
            let properties = fields
                .iter()
                .map(|(name, schema)| {
                    (
                        name.clone(),
                        schema_json(
                            schema.kind,
                            schema.items.as_deref(),
                            &schema.fields,
                            &schema.required,
                            schema.enum_values.as_deref(),
                            schema.nullable,
                        ),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            serde_json::json!({
                "type":"object",
                "properties":properties,
                "required":required,
                "additionalProperties":false
            })
        }
        OutputType::List => {
            let item = items.expect("list schema validated");
            serde_json::json!({
                "type":"array",
                "items":schema_json(
                    item.kind,
                    item.items.as_deref(),
                    &item.fields,
                    &item.required,
                    item.enum_values.as_deref(),
                    item.nullable,
                )
            })
        }
    };
    if nullable && kind != OutputType::Json {
        if enum_values.is_some() {
            return serde_json::json!({"anyOf":[schema,{"type":"null"}]});
        }
        let object = schema
            .as_object_mut()
            .expect("schema projection is an object");
        object.insert("nullable".into(), serde_json::Value::Bool(true));
        if let Some(kind_schema) = object.get_mut("type") {
            match kind_schema {
                serde_json::Value::String(kind) => {
                    *kind_schema = serde_json::json!([kind, "null"]);
                }
                serde_json::Value::Array(kinds) => kinds.push(serde_json::json!("null")),
                _ => {}
            }
        }
    }
    schema
}

fn validate_json_value(
    kind: OutputType,
    items: Option<&ValueSchema>,
    fields: &BTreeMap<String, ValueSchema>,
    required: &[String],
    enum_values: Option<&[String]>,
    nullable: bool,
    value: &serde_json::Value,
) -> Result<()> {
    if nullable && value.is_null() {
        return Ok(());
    }
    match kind {
        OutputType::Bytes | OutputType::Text | OutputType::Path => {
            if !value.is_string() {
                bail!("expected JSON string, got {}", json_kind(value));
            }
            if kind == OutputType::Path {
                let path = value.as_str().expect("string checked above");
                if path.is_empty() {
                    bail!("path output must be nonempty");
                }
                if path.contains('\0') {
                    bail!("path output contains a NUL byte");
                }
            }
            if kind == OutputType::Text {
                validate_enum_value(enum_values, value.as_str().expect("string checked above"))?;
            }
        }
        OutputType::Integer => {
            if value.as_i64().is_none() {
                bail!(
                    "expected JSON signed 64-bit integer, got {}",
                    json_kind(value)
                );
            }
        }
        OutputType::Number => {
            if !value.is_number() {
                bail!("expected JSON number, got {}", json_kind(value));
            }
        }
        OutputType::Boolean => {
            if !value.is_boolean() {
                bail!("expected JSON boolean, got {}", json_kind(value));
            }
        }
        OutputType::Json => {}
        OutputType::Record => {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("expected JSON object, got {}", json_kind(value)))?;
            for name in required {
                if !object.contains_key(name) {
                    bail!("record output is missing required field {name:?}");
                }
            }
            for name in object.keys() {
                if !fields.contains_key(name) {
                    bail!("record output contains undeclared field {name:?}");
                }
            }
            for (name, schema) in fields {
                if let Some(value) = object.get(name) {
                    validate_json_value(
                        schema.kind,
                        schema.items.as_deref(),
                        &schema.fields,
                        &schema.required,
                        schema.enum_values.as_deref(),
                        schema.nullable,
                        value,
                    )
                    .with_context(|| format!("validating record field {name:?}"))?;
                }
            }
        }
        OutputType::List => {
            let values = value
                .as_array()
                .ok_or_else(|| anyhow!("expected JSON array, got {}", json_kind(value)))?;
            let item_schema = items.expect("list schema validated");
            for (index, value) in values.iter().enumerate() {
                validate_json_value(
                    item_schema.kind,
                    item_schema.items.as_deref(),
                    &item_schema.fields,
                    &item_schema.required,
                    item_schema.enum_values.as_deref(),
                    item_schema.nullable,
                    value,
                )
                .with_context(|| format!("validating list item {index}"))?;
            }
        }
    }
    Ok(())
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Parse JSON while rejecting duplicate object keys. A normal `Value` parse
/// silently keeps the last duplicate key, which could change a validated
/// record after an applet's bytes cross the contract boundary.
fn parse_json_strict(bytes: &[u8]) -> Result<serde_json::Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictJsonValue::deserialize(&mut deserializer).context("parsing JSON")?;
    deserializer.end().context("parsing JSON")?;
    Ok(value.into_json())
}

enum StrictJsonValue {
    Null,
    Boolean(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

impl StrictJsonValue {
    fn into_json(self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Boolean(value) => serde_json::Value::Bool(value),
            Self::Number(value) => serde_json::Value::Number(value),
            Self::String(value) => serde_json::Value::String(value),
            Self::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(Self::into_json).collect())
            }
            Self::Object(values) => serde_json::Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, value.into_json()))
                    .collect(),
            ),
        }
    }
}

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StrictVisitor;
        impl<'de> serde::de::Visitor<'de> for StrictVisitor {
            type Value = StrictJsonValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value with unique object keys")
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Null)
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Null)
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Boolean(value))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(StrictJsonValue::Number)
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(StrictJsonValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, StrictJsonValue>()? {
                    if values.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON object key {key:?}"
                        )));
                    }
                }
                Ok(StrictJsonValue::Object(values))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

/// Classification for one exit status. Only `Success` is implicitly accepted
/// by runtimes; the other kinds remain explicit control-flow outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Success,
    Absence,
    Failure,
    Retryable,
}

/// Host-visible certainty about whether an applet's declared effect occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectCertainty {
    NotStarted,
    KnownResponse,
    Uncertain,
}

impl EffectCertainty {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::KnownResponse => "known_response",
            Self::Uncertain => "uncertain",
        }
    }
}

impl OutcomeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Absence => "absence",
            Self::Failure => "failure",
            Self::Retryable => "retryable",
        }
    }
}

/// A declared mapping from a native status code to a named outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeSpec {
    pub status: i32,
    pub name: String,
    pub kind: OutcomeKind,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub effect_certainty: Option<EffectCertainty>,
}

impl OutcomeSpec {
    pub fn resolved_effect_certainty(&self) -> EffectCertainty {
        self.effect_certainty.unwrap_or(match self.kind {
            OutcomeKind::Success | OutcomeKind::Absence => EffectCertainty::KnownResponse,
            OutcomeKind::Failure | OutcomeKind::Retryable => EffectCertainty::Uncertain,
        })
    }
}

/// Resolved status classification returned to runtimes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: i32,
    pub name: String,
    pub kind: OutcomeKind,
    pub effect_certainty: EffectCertainty,
}

/// Output preserved as raw bytes or decoded only at an explicit typed boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum AppletOutput {
    Bytes(Vec<u8>),
    Value(Value),
}

/// Version 1 applet execution contract. A missing output schema means bytes;
/// missing outcomes use the conventional status-zero success rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppletContract {
    pub version: u32,
    #[serde(default)]
    pub backend: AppletBackend,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub program: Option<ProgramModule>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub remote: Option<RemoteEndpointConfig>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub environment: Option<AppletEnvironment>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub output: Option<OutputSchema>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<OutcomeSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub result_envelope: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub ports: Option<AppletPorts>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl AppletContract {
    pub(crate) fn validate(&self) -> Result<()> {
        self.validate_interface()?;
        self.lifecycle.validate()?;
        if let Some(environment) = &self.environment {
            environment.validate()?;
        }
        if self.lifecycle.is_program_service() && self.backend != AppletBackend::ProgramModule {
            bail!("program_service lifecycle requires program_module backend");
        }
        match (self.backend, self.lifecycle.is_json_lines()) {
            (AppletBackend::JsonLinesWorker, true)
            | (
                AppletBackend::Command
                | AppletBackend::InProcess
                | AppletBackend::ProgramModule
                | AppletBackend::Remote,
                false,
            ) => {}
            (AppletBackend::JsonLinesWorker, false) => {
                bail!("json_lines_worker backend requires json_lines lifecycle")
            }
            (_, true) => bail!("json_lines lifecycle requires json_lines_worker backend"),
        }
        match self.backend {
            AppletBackend::ProgramModule => {
                let program = self.program.as_ref().ok_or_else(|| {
                    anyhow!("program_module backend requires a program descriptor")
                })?;
                program.validate()?;
                match (program.interface_version, &self.lifecycle) {
                    (1, Lifecycle::OneShot) | (2, Lifecycle::ProgramService) => {}
                    (1, Lifecycle::ProgramService) => bail!(
                        "program_service lifecycle requires program module interface version 2"
                    ),
                    (2, _) => bail!(
                        "program module interface version 2 requires program_service lifecycle"
                    ),
                    _ => unreachable!("ProgramModule::validate checks supported interfaces"),
                }
                if self.remote.is_some() {
                    bail!("program_module backend cannot declare a remote endpoint");
                }
            }
            AppletBackend::Remote => {
                self.remote
                    .as_ref()
                    .ok_or_else(|| anyhow!("remote backend requires a remote endpoint config"))?
                    .validate_declaration()?;
                if self.program.is_some() {
                    bail!("remote backend cannot declare a program module");
                }
            }
            _ => {
                if self.lifecycle.is_program_service() {
                    bail!("program_service lifecycle requires program_module backend");
                }
                if self.program.is_some() || self.remote.is_some() {
                    bail!("program and remote descriptors require their matching backend");
                }
            }
        }
        if self.backend == AppletBackend::InProcess
            && !self.outcomes.is_empty()
            && !self
                .outcomes
                .iter()
                .any(|outcome| outcome.status == 0 && outcome.kind == OutcomeKind::Success)
        {
            bail!("in_process outcome tables must declare status 0 as success");
        }
        Ok(())
    }

    /// Validate interface-only metadata. Remote discovery descriptors use
    /// this without requiring the caller's endpoint/backend configuration.
    pub(crate) fn validate_interface(&self) -> Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported applet contract version {}; supported version is 1",
                self.version
            );
        }
        if let Some(output) = &self.output {
            output.validate()?;
        }
        if let Some(ports) = &self.ports {
            ports.validate()?;
        }
        let mut statuses = BTreeSet::new();
        let mut names = BTreeSet::new();
        for outcome in &self.outcomes {
            if !statuses.insert(outcome.status) {
                bail!("contract outcome statuses must be unique");
            }
            if !valid_outcome_name(&outcome.name) || !names.insert(&outcome.name) {
                bail!("contract outcome names must be unique lowercase identifiers");
            }
        }
        let mut capabilities = BTreeSet::new();
        for capability in &self.capabilities {
            if !valid_capability_id(capability) || !capabilities.insert(capability) {
                bail!("contract capabilities must be unique namespaced identifiers");
            }
        }
        let mut effects = BTreeSet::new();
        for effect in &self.effects {
            if !valid_capability_id(effect) || !effects.insert(effect) {
                bail!("contract effects must be unique namespaced identifiers");
            }
        }
        if !effects.is_subset(&capabilities) {
            bail!("contract effects must be included in declared capabilities");
        }
        if self.result_envelope
            && self
                .output
                .as_ref()
                .is_none_or(|output| output.kind == OutputType::Bytes)
        {
            bail!("result_envelope requires a typed non-bytes output schema");
        }
        Ok(())
    }

    pub fn output_kind(&self) -> Option<InputType> {
        if self.result_envelope {
            Some(InputType::Json)
        } else {
            self.output.as_ref().and_then(OutputSchema::input_kind)
        }
    }

    /// Schema for the typed value produced when `result_envelope` is enabled.
    /// The outcome metadata is part of this interface so callers can inspect
    /// uncertainty without guessing from the exit status.
    pub fn result_envelope_schema(&self) -> Option<ValueSchema> {
        if !self.result_envelope {
            return None;
        }
        let output = self.output.as_ref()?;
        let value_schema = ValueSchema {
            kind: output.kind,
            nullable: true,
            items: output.items.clone(),
            fields: output.fields.clone(),
            required: output.required.clone(),
            enum_values: output.enum_values.clone(),
        };
        let text_schema = || ValueSchema {
            kind: OutputType::Text,
            nullable: false,
            items: None,
            fields: BTreeMap::new(),
            required: Vec::new(),
            enum_values: None,
        };
        let integer_schema = ValueSchema {
            kind: OutputType::Integer,
            nullable: false,
            items: None,
            fields: BTreeMap::new(),
            required: Vec::new(),
            enum_values: None,
        };
        let fields = BTreeMap::from([
            ("value".into(), value_schema),
            ("outcome".into(), text_schema()),
            ("kind".into(), text_schema()),
            ("effect_certainty".into(), text_schema()),
            ("status".into(), integer_schema),
        ]);
        Some(ValueSchema {
            kind: OutputType::Record,
            nullable: false,
            items: None,
            fields,
            required: vec![
                "value".into(),
                "outcome".into(),
                "kind".into(),
                "effect_certainty".into(),
                "status".into(),
            ],
            enum_values: None,
        })
    }

    pub fn success_status(&self) -> Option<i32> {
        if self.outcomes.is_empty() {
            Some(0)
        } else {
            self.outcomes
                .iter()
                .filter(|outcome| outcome.kind == OutcomeKind::Success)
                .map(|outcome| outcome.status)
                .min()
        }
    }

    pub fn outcome_for(&self, status: i32) -> Outcome {
        if let Some(declared) = self
            .outcomes
            .iter()
            .find(|outcome| outcome.status == status)
        {
            return Outcome {
                status,
                name: declared.name.clone(),
                kind: declared.kind,
                effect_certainty: declared.resolved_effect_certainty(),
            };
        }
        Outcome {
            status,
            name: if self.outcomes.is_empty() && status == 0 {
                "success".into()
            } else {
                format!("exit_{status}")
            },
            kind: if self.outcomes.is_empty() && status == 0 {
                OutcomeKind::Success
            } else {
                OutcomeKind::Failure
            },
            effect_certainty: if self.outcomes.is_empty() && status == 0 {
                EffectCertainty::KnownResponse
            } else {
                EffectCertainty::Uncertain
            },
        }
    }

    pub fn decode_output(&self, bytes: &[u8], status: i32) -> Result<AppletOutput> {
        // In-process text and path results cross runtime boundaries as raw UTF-8.
        let raw_text = self.in_process_text_output();
        self.decode_output_with(
            status,
            |schema| {
                if raw_text {
                    schema.decode_raw_text_value(bytes)
                } else {
                    schema.decode(bytes)
                }
            },
            || Ok(AppletOutput::Bytes(bytes.to_vec())),
        )
    }

    /// Whether this in-process applet returns text or a path. Such results are
    /// raw UTF-8 bytes at the pipe, `then`, save and terminal boundaries.
    pub(crate) fn in_process_text_output(&self) -> bool {
        self.backend == AppletBackend::InProcess
            && self
                .output
                .as_ref()
                .is_some_and(|schema| matches!(schema.kind, OutputType::Text | OutputType::Path))
    }

    #[cfg(any(not(target_arch = "wasm32"), test))]
    pub(crate) fn decode_json_output_value(
        &self,
        value: serde_json::Value,
        bytes: &[u8],
        status: i32,
    ) -> Result<AppletOutput> {
        self.decode_output_with(
            status,
            |schema| schema.decode_json_value_or_bytes(value, bytes),
            || bail!("typed JSON output requires a declared output schema"),
        )
    }

    fn decode_output_with(
        &self,
        status: i32,
        decode_success: impl FnOnce(&OutputSchema) -> Result<AppletOutput>,
        decode_without_schema: impl FnOnce() -> Result<AppletOutput>,
    ) -> Result<AppletOutput> {
        let outcome = self.outcome_for(status);
        if outcome.kind != OutcomeKind::Success && !self.result_envelope {
            bail!(
                "applet outcome {:?} ({}) is not an implicitly accepted success",
                outcome.name,
                outcome.kind.as_str()
            );
        }
        let decoded = if outcome.kind == OutcomeKind::Success {
            match &self.output {
                Some(schema) => decode_success(schema)?,
                None => decode_without_schema()?,
            }
        } else {
            AppletOutput::Value(Value::Null)
        };
        if !self.result_envelope {
            return Ok(decoded);
        }
        let value = match decoded {
            AppletOutput::Value(value) => value,
            AppletOutput::Bytes(_) => bail!("result_envelope cannot contain raw bytes"),
        };
        let envelope = BTreeMap::from([
            ("value".to_owned(), value),
            ("outcome".to_owned(), Value::String(outcome.name)),
            (
                "kind".to_owned(),
                Value::String(outcome.kind.as_str().into()),
            ),
            (
                "effect_certainty".to_owned(),
                Value::String(outcome.effect_certainty.as_str().into()),
            ),
            ("status".to_owned(), Value::Integer(i64::from(status))),
        ]);
        Ok(AppletOutput::Value(Value::Record(envelope)))
    }

    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    pub fn effects(&self) -> &[String] {
        &self.effects
    }
}

fn valid_capability_id(value: &str) -> bool {
    let mut segments = value.split('.');
    let first = segments.next().unwrap_or_default();
    if first.is_empty()
        || !first
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
    {
        return false;
    }
    let mut found_namespace_separator = false;
    for segment in segments {
        found_namespace_separator = true;
        if segment.is_empty()
            || !segment
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
        {
            return false;
        }
    }
    found_namespace_separator
}

fn valid_outcome_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

/// Deserialize an optional field while rejecting an explicit JSON `null`.
/// The default handler still maps an omitted field to `None`.
pub(crate) fn deserialize_present<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod typed_json_decode_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn predecoded_json_uses_the_same_schema_validation_as_bytes() {
        let cases = [
            (
                json!({"type":"text","encoding":"json","enum":["ready"]}),
                json!("ready"),
            ),
            (json!({"type":"path","encoding":"json"}), json!("/tmp/item")),
            (json!({"type":"integer","encoding":"json"}), json!(7)),
            (json!({"type":"number","encoding":"json"}), json!(1.25)),
            (json!({"type":"json","encoding":"json"}), json!(u64::MAX)),
            (json!({"type":"json","encoding":"json"}), json!(-0.0)),
            (json!({"type":"json","encoding":"json"}), json!(1e100)),
            (json!({"type":"boolean","encoding":"json"}), json!(true)),
            (
                json!({"type":"json","encoding":"json"}),
                json!({"x":[null,2]}),
            ),
            (
                json!({
                    "type":"record","encoding":"json",
                    "fields":{"count":{"type":"integer"}},"required":["count"]
                }),
                json!({"count":4}),
            ),
            (
                json!({"type":"list","encoding":"json","items":{"type":"integer"}}),
                json!([1, 2, 3]),
            ),
        ];

        for (schema, value) in cases {
            let schema: OutputSchema = serde_json::from_value(schema).unwrap();
            let bytes = serde_json::to_vec(&value).unwrap();
            assert_eq!(
                schema.decode(&bytes).unwrap(),
                schema.decode_json_value(value).unwrap()
            );
        }
    }

    #[test]
    fn predecoded_json_keeps_schema_errors_and_result_envelope_semantics() {
        let invalid_cases = [
            (
                json!({"type":"text","encoding":"json","enum":["ready"]}),
                json!("waiting"),
            ),
            (json!({"type":"path","encoding":"json"}), json!("")),
            (json!({"type":"integer","encoding":"json"}), json!("four")),
            (
                json!({
                    "type":"record","encoding":"json",
                    "fields":{"count":{"type":"integer"}},"required":["count"]
                }),
                json!({"count":"four"}),
            ),
        ];
        for (schema, invalid) in invalid_cases {
            let schema: OutputSchema = serde_json::from_value(schema).unwrap();
            let bytes = serde_json::to_vec(&invalid).unwrap();
            assert_eq!(
                format!("{:#}", schema.decode(&bytes).unwrap_err()),
                format!(
                    "{:#}",
                    schema
                        .decode_json_value_or_bytes(invalid, &bytes)
                        .unwrap_err()
                )
            );
        }

        let contract: AppletContract = serde_json::from_value(json!({
            "version":1,
            "output":{
                "type":"record","encoding":"json",
                "fields":{"count":{"type":"integer"}},"required":["count"]
            },
            "result_envelope":true
        }))
        .unwrap();
        let value = json!({"count":5});
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            contract.decode_output(&bytes, 0).unwrap(),
            contract.decode_json_output_value(value, &bytes, 0).unwrap()
        );
        let non_success = json!({"count":"ignored for declared non-success"});
        let bytes = serde_json::to_vec(&non_success).unwrap();
        assert_eq!(
            contract.decode_output(&bytes, 7).unwrap(),
            contract
                .decode_json_output_value(non_success, &bytes, 7)
                .unwrap()
        );
    }

    #[test]
    fn predecoded_json_falls_back_to_bytes_at_the_strict_parser_depth_boundary() {
        let schema: OutputSchema =
            serde_json::from_value(json!({"type":"json","encoding":"json"})).unwrap();
        let mut parser_limit_depth = None;
        for depth in 0..=256 {
            let value = (0..depth).fold(json!(null), |nested, _| json!([nested]));
            let fast_path_safe = predecoded_json_depth_is_safe(&value);
            let bytes = serde_json::to_vec(&value).unwrap();
            let byte_result = schema.decode(&bytes);
            let parser_limit_rejected = byte_result
                .as_ref()
                .err()
                .is_some_and(|error| format!("{error:#}").contains("recursion limit"));
            let carried_result = schema.decode_json_value_or_bytes(value, &bytes);
            assert_eq!(describe(byte_result), describe(carried_result));

            if parser_limit_rejected {
                assert!(!fast_path_safe);
                parser_limit_depth = Some(depth);
                break;
            }
        }
        assert!(
            parser_limit_depth.is_some(),
            "expected serde_json's strict parser to reject deeply nested arrays"
        );
    }

    fn describe(result: Result<AppletOutput>) -> std::result::Result<AppletOutput, String> {
        result.map_err(|error| format!("{error:#}"))
    }
}
