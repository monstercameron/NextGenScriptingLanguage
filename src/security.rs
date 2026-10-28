//! Explicit data-flow and persistence authorization for applet execution.
//!
//! Sensitivity is declared at data boundaries. Missing provenance remains
//! `Unknown`; the runtime never upgrades it to `Public` by guessing from bytes
//! or content. Remote egress requires an exact origin grant. Persistent sinks
//! allow public data by default, while non-public labels require an explicit
//! policy entry. Plaintext persistence of `Secret` is always rejected.

use crate::contracts::Sensitivity;
use crate::manifest::Registry;
use crate::manifest::{AppletBackend, Manifest};
use anyhow::{anyhow, Result};
#[cfg(windows)]
use anyhow::{bail, Context};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Install once at a CLI host boundary, before starting its threads. Abort builds
/// cannot recover a panicking worker: the supervisor must restart the process.
/// Panic payloads are deliberately withheld because they may contain secrets.
/// Embedded hosts choose their own hook instead of being changed at library load.
pub fn install_panic_hook(host: &'static str, steps: Option<&[crate::ast::Step]>) {
    let lines: BTreeMap<String, usize> = steps
        .map(|steps| {
            crate::ast::step_ids(steps)
                .into_iter()
                .zip(steps.iter().map(|step| step.line))
                .collect()
        })
        .unwrap_or_default();
    #[cfg(feature = "failpoints")]
    let force_abort = std::env::var_os("EXECUTE_TEST_PANIC_ABORT").is_some();
    #[cfg(not(feature = "failpoints"))]
    let force_abort = false;
    std::panic::set_hook(Box::new(move |information| {
        use std::io::Write;
        let step = crate::runtime::current_step_id();
        let line = step.as_deref().and_then(|id| lines.get(id)).copied();
        let abort = cfg!(panic = "abort") || force_abort;
        let diagnostic = serde_json::json!({
            "protocol": "execute.panic/1",
            "host": host,
            "thread": std::thread::current().name(),
            "action": if abort { "abort" } else { "unwind" },
            "step": step.as_deref(),
            "line": line,
            "rust_location": information.location().map(|location| serde_json::json!({
                "file": location.file(), "line": location.line(), "column": location.column()
            })),
            "message": "internal interpreter panic; payload withheld"
        });
        // Write directly: a panic may have occurred while a runtime diagnostic,
        // record, or redactor lock was held. Never invoke the previous hook too.
        let _ = writeln!(std::io::stderr().lock(), "{diagnostic}");
        if force_abort {
            std::process::abort();
        }
    }));
    #[cfg(feature = "failpoints")]
    if std::env::var("EXECUTE_TEST_PANIC_HOST").is_ok_and(|name| name == host) {
        panic!("injected host-boundary panic");
    }
}

/// The distinct sensitivity labels known to contribute to a value.
///
/// Keeping a set matters because `Unknown` must not hide a known `Secret` (or
/// vice versa) when an exact-label policy is applied to mixed provenance.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SensitivitySet(BTreeSet<Sensitivity>);

impl Default for SensitivitySet {
    fn default() -> Self {
        Self::from_label(Sensitivity::Unknown)
    }
}

impl SensitivitySet {
    pub fn empty() -> Self {
        Self(BTreeSet::new())
    }

    pub fn from_label(label: Sensitivity) -> Self {
        Self(BTreeSet::from([label]))
    }

    pub fn from_labels(labels: impl IntoIterator<Item = Sensitivity>) -> Self {
        let labels = labels.into_iter().collect::<BTreeSet<_>>();
        if labels.is_empty() {
            Self::default()
        } else {
            Self(labels)
        }
    }

    pub fn insert(&mut self, label: Sensitivity) {
        self.0.insert(label);
    }

    pub fn extend(&mut self, other: &Self) {
        self.0.extend(other.0.iter().copied());
    }

    pub fn contains(&self, label: &Sensitivity) -> bool {
        self.0.contains(label)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = Sensitivity> + '_ {
        self.0.iter().copied()
    }
}

/// Version of the data-flow policy document format. Parsing rejects anything
/// else, so an installed (active) policy is always this version.
pub const DATA_FLOW_POLICY_VERSION: u8 = 1;

const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_ORIGINS: usize = 256;
const MAX_TRUSTED_CACHE_APPLETS: usize = 1024;
const MAX_DECISION_RECORDS: usize = 1024;
/// How many policy decisions a run record keeps. The in-memory log above is the
/// live window; the persisted audit is a bounded tail of it so `run.json` stays
/// within its size limit. The total count is recorded alongside, so a truncated
/// audit is explicit rather than silent.
pub const MAX_PERSISTED_DECISIONS: usize = 256;
const MAX_REDACTION_SECRETS: usize = 256;
const MAX_REDACTION_SECRET_BYTES: usize = 256 * 1024;
const MAX_REDACTION_SECRET_BYTES_EACH: usize = 64 * 1024;
const MAX_DECLARED_SECRET_ENVIRONMENTS: usize = 256;

const REVIEWED_PURE_IN_PROCESS_APPLETS: &[&str] = &[
    "color.parse_rgb",
    "color.parse_rgb.result",
    "color.rgb_to_hex",
    "json.filter",
    "json.get",
    "json.map",
    "json.parse",
    "json.set",
    "json.stringify",
    "jsonc.edit_content",
    "number.add",
    "number.compare",
    "number.divide",
    "number.multiply",
    "number.parse_integer",
    "number.parse_integer.result",
    "number.subtract",
    "number.to_text",
    "path.basename",
    "path.dirname",
    "path.remove_suffix",
    "text.concat",
    "text.matches",
    "text.normalize",
    "text.trim_final_lf",
];

const PURE_CAPABILITIES: &[&str] = &[
    "color.convert",
    "json.convert",
    "json.inspect",
    "json.transform",
    "jsonc.transform",
    "number.compute",
    "number.convert",
    "path.inspect",
    "text.transform",
];

/// One remote origin and the sensitivity labels it may receive.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteSinkGrant {
    allow: BTreeSet<Sensitivity>,
}

/// Kinds of persistence that can retain applet data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceKind {
    Cache,
    Checkpoint,
    Save,
    /// Captured output kept by `execute run --record`.
    Record,
}

impl PersistenceKind {
    fn action(self) -> &'static str {
        match self {
            Self::Cache => "cache.persist",
            Self::Checkpoint => "checkpoint.persist",
            Self::Save => "filesystem.persist",
            Self::Record => "record.persist",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDocument {
    version: u32,
    #[serde(default)]
    remote_sinks: UniqueMap<RemoteSinkGrant>,
    #[serde(default)]
    persistence: UniqueMap<Vec<Sensitivity>>,
    #[serde(default)]
    cache_purity: CachePurityGrant,
    #[serde(default)]
    secrets: SecretSources,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretSources {
    /// Applet-scoped names whose values should be redacted from diagnostics.
    /// This never causes the host environment to be scanned: only names and
    /// values in the effective environment snapshot for that applet count.
    #[serde(default)]
    environment: UniqueMap<Vec<String>>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachePurityGrant {
    /// These names are an operator's explicit trust assertion, not a proof
    /// derived from the applet's own effect declarations.
    #[serde(default)]
    trusted_applets: Vec<String>,
}

/// Validated origin, persistence, and cache-purity grants. A parsed policy is
/// active; its default behavior denies remote egress and non-public
/// persistence. The inactive default preserves legacy behavior when the user
/// did not opt into data-flow policy enforcement.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
pub struct DataFlowPolicy {
    active: bool,
    remote_sinks: BTreeMap<String, BTreeSet<Sensitivity>>,
    persistence: BTreeMap<PersistenceKind, BTreeSet<Sensitivity>>,
    trusted_cache_applets: BTreeSet<String>,
    secret_environment: BTreeMap<String, BTreeSet<String>>,
}

impl fmt::Debug for DataFlowPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DataFlowPolicy")
            .field(
                "remote_origins",
                &self.remote_sinks.keys().collect::<Vec<_>>(),
            )
            .field(
                "persistence_kinds",
                &self.persistence.keys().collect::<Vec<_>>(),
            )
            .field("trusted_cache_applets", &self.trusted_cache_applets)
            .field(
                "declared_secret_environment_count",
                &self
                    .secret_environment
                    .values()
                    .map(BTreeSet::len)
                    .sum::<usize>(),
            )
            .finish()
    }
}

/// Lowercase hex encoding shared by the policy-audit digests and seals. A
/// table lookup keeps every digest site on one implementation instead of
/// each monomorphizing hex formatting; the output matches
/// `format!("{byte:02x}")` exactly.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

impl DataFlowPolicy {
    /// Whether a policy was installed. The default preserves legacy behavior;
    /// only a parsed policy constrains egress and persistence.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Canonical content digest of the parsed policy: the SHA-256 of its
    /// canonical JSON encoding. Run records store this digest so a reviewer
    /// can tell which policy produced an allow/deny log. A digest is one-way:
    /// it names the policy without recording any origin, label, or other
    /// policy content.
    pub fn canonical_digest(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("a parsed data-flow policy is serializable");
        hex_encode(Sha256::digest(encoded).as_slice())
    }

    /// Load a bounded, owner-private policy file. The file contains only
    /// authorization labels and canonical origins, never credential values.
    pub fn load(path: &Path) -> Result<Self, PolicyError> {
        let mut file = open_private_file(path).map_err(|_| PolicyError::PolicyUnavailable)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_POLICY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| PolicyError::PolicyUnavailable)?;
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(PolicyError::InvalidPolicy);
        }
        Self::from_json(&bytes)
    }

    /// Parse a strict version-1 data-flow policy.
    pub fn from_json(bytes: &[u8]) -> Result<Self, PolicyError> {
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(PolicyError::InvalidPolicy);
        }
        let document: PolicyDocument =
            serde_json::from_slice(bytes).map_err(|_| PolicyError::InvalidPolicy)?;
        if document.version != 1
            || document.remote_sinks.0.len() > MAX_ORIGINS
            || document.persistence.0.len() > 4
            || document.cache_purity.trusted_applets.len() > MAX_TRUSTED_CACHE_APPLETS
            || document.secrets.environment.0.len() > MAX_DECLARED_SECRET_ENVIRONMENTS
        {
            return Err(PolicyError::InvalidPolicy);
        }

        let mut remote_sinks = BTreeMap::new();
        for (origin, grant) in document.remote_sinks.0 {
            let canonical =
                canonical_policy_origin(&origin).map_err(|_| PolicyError::InvalidPolicy)?;
            if canonical != origin
                || grant.allow.is_empty()
                || remote_sinks.insert(origin, grant.allow).is_some()
            {
                return Err(PolicyError::InvalidPolicy);
            }
        }

        let mut persistence = BTreeMap::new();
        for (kind_name, labels) in document.persistence.0 {
            let kind = match kind_name.as_str() {
                "cache" => PersistenceKind::Cache,
                "checkpoint" => PersistenceKind::Checkpoint,
                "save" => PersistenceKind::Save,
                "record" => PersistenceKind::Record,
                _ => return Err(PolicyError::InvalidPolicy),
            };
            let labels = labels.into_iter().collect::<BTreeSet<_>>();
            if labels.is_empty()
                // This runtime does not encrypt persistent state. Secret and
                // unknown values must never be written as plaintext.
                || labels.contains(&Sensitivity::Secret)
                || labels.contains(&Sensitivity::Unknown)
                || persistence.insert(kind, labels).is_some()
            {
                return Err(PolicyError::InvalidPolicy);
            }
        }
        let mut trusted_cache_applets = BTreeSet::new();
        for applet in document.cache_purity.trusted_applets {
            if !valid_applet_id(&applet) || !trusted_cache_applets.insert(applet) {
                return Err(PolicyError::InvalidPolicy);
            }
        }
        let mut secret_environment = BTreeMap::new();
        let mut declared_secret_count = 0usize;
        for (applet, names) in document.secrets.environment.0 {
            if !valid_applet_id(&applet) || names.len() > MAX_DECLARED_SECRET_ENVIRONMENTS {
                return Err(PolicyError::InvalidPolicy);
            }
            let mut unique_names = BTreeSet::new();
            for name in names {
                declared_secret_count = declared_secret_count.saturating_add(1);
                if declared_secret_count > MAX_DECLARED_SECRET_ENVIRONMENTS
                    || !valid_environment_name(&name)
                    || !unique_names.insert(name)
                {
                    return Err(PolicyError::InvalidPolicy);
                }
            }
            secret_environment.insert(applet, unique_names);
        }
        Ok(Self {
            active: true,
            remote_sinks,
            persistence,
            trusted_cache_applets,
            secret_environment,
        })
    }
}

/// Shared exact-value redaction registry. Values enter only through explicit
/// credential sources or applet-scoped environment declarations. It does not
/// attempt to infer secrets from arbitrary content.
#[derive(Default)]
pub struct SecretRedactor {
    state: Mutex<RedactorState>,
}

#[derive(Default)]
struct RedactorState {
    values: Vec<SecretBytes>,
    total_bytes: usize,
}

struct SecretBytes(Vec<u8>);

impl Drop for SecretBytes {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // SAFETY: `byte` is a `&mut u8` from iterating `self.0`, so it points to an
            // initialized, aligned byte of this Vec's buffer. Drop has `&mut self`, so no other
            // reference to that byte exists. The write stores 0 and keeps the length.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
    }
}

impl fmt::Debug for SecretRedactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("SecretRedactor")
            .field("secret_count", &state.values.len())
            .field("registered_bytes", &state.total_bytes)
            .finish()
    }
}

impl SecretRedactor {
    /// Register one exact secret value. Duplicate registrations are harmless.
    /// Overflow is an error so callers can fail closed instead of emitting a
    /// diagnostic that might expose an unregistered value.
    pub fn register(&self, secret: &[u8]) -> Result<(), PolicyError> {
        if secret.is_empty() {
            return Ok(());
        }
        if secret.len() > MAX_REDACTION_SECRET_BYTES_EACH {
            return Err(PolicyError::RedactionLimitExceeded);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.values.iter().any(|registered| registered.0 == secret) {
            return Ok(());
        }
        if state.values.len() >= MAX_REDACTION_SECRETS
            || state.total_bytes.saturating_add(secret.len()) > MAX_REDACTION_SECRET_BYTES
        {
            return Err(PolicyError::RedactionLimitExceeded);
        }
        state.values.push(SecretBytes(secret.to_vec()));
        state
            .values
            .sort_by_key(|secret| std::cmp::Reverse(secret.0.len()));
        state.total_bytes += secret.len();
        Ok(())
    }

    /// Replace exact registered byte sequences in an emitted byte string.
    pub fn redact_bytes(&self, input: &[u8]) -> Vec<u8> {
        const REDACTED: &[u8] = b"[REDACTED]";
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.values.is_empty() || input.is_empty() {
            return input.to_vec();
        }
        let mut output = Vec::with_capacity(input.len());
        let mut cursor = 0;
        while cursor < input.len() {
            let matched = state
                .values
                .iter()
                .find(|secret| input[cursor..].starts_with(&secret.0));
            if let Some(secret) = matched {
                output.extend_from_slice(REDACTED);
                cursor += secret.0.len();
            } else {
                output.push(input[cursor]);
                cursor += 1;
            }
        }
        output
    }

    /// UTF-8 display helper for CLI errors and diagnostics.
    pub fn redact_text(&self, input: &str) -> String {
        String::from_utf8_lossy(&self.redact_bytes(input.as_bytes())).into_owned()
    }
}

/// Obtain the registry-scoped shared redactor. Registry clones share this
/// state; callers should register credentials before any request or child
/// process can emit diagnostics.
pub fn secret_redactor(registry: &Registry) -> std::sync::Arc<SecretRedactor> {
    registry.host_state::<SecretRedactor>()
}

/// Register explicitly sensitive inputs before runtime steps can emit derived values.
pub fn register_sensitive_inputs(
    registry: &Registry,
    declarations: &[crate::ast::InputDeclaration],
    inputs: &BTreeMap<String, crate::ast::Value>,
) -> Result<()> {
    let resolved = crate::inputs::resolve(declarations, inputs, false)?;
    for declaration in declarations
        .iter()
        .filter(|input| crate::ast::is_sensitive(input))
    {
        if let Some(value) = resolved.get(&declaration.name) {
            register_sensitive_value(registry, value)?;
        }
    }
    Ok(())
}

/// Register exact string and numeric leaves of an explicitly sensitive value.
/// Booleans, nulls and references are metadata, not secret material.
pub(crate) fn register_sensitive_value(
    registry: &Registry,
    value: &crate::ast::Value,
) -> Result<()> {
    fn register(redactor: &SecretRedactor, value: &crate::ast::Value) -> Result<()> {
        let text = match value {
            crate::ast::Value::String(value) => value.clone(),
            crate::ast::Value::Integer(value) => value.to_string(),
            crate::ast::Value::Number(value) => value.to_string(),
            crate::ast::Value::List(values) => {
                return values
                    .iter()
                    .try_for_each(|value| register(redactor, value));
            }
            crate::ast::Value::Record(values) => {
                for (key, value) in values {
                    redactor
                        .register(key.as_bytes())
                        .map_err(|error| anyhow!("registering sensitive value key: {error}"))?;
                    register(redactor, value)?;
                }
                return Ok(());
            }
            crate::ast::Value::Bool(_)
            | crate::ast::Value::Null
            | crate::ast::Value::Reference(_) => return Ok(()),
        };
        redactor
            .register(text.as_bytes())
            .map_err(|error| anyhow!("registering sensitive value: {error}"))
    }
    register(&secret_redactor(registry), value)
}

/// Register opaque bytes derived from a sensitive value before they reach a
/// live binding, observer, or diagnostic surface.
pub(crate) fn register_sensitive_bytes(registry: &Registry, bytes: &[u8]) -> Result<()> {
    secret_redactor(registry)
        .register(bytes)
        .map_err(|error| anyhow!("registering sensitive bytes: {error}"))
}

/// Register literal leaves from `and keep secret` invocations before observers
/// or other sinks can see their source copy. Runtime-acquired values are added
/// when produced.
pub fn register_sensitive_program(
    registry: &Registry,
    program: &crate::ast::Program,
) -> Result<()> {
    let redactor = secret_redactor(registry);
    fn register_call(redactor: &SecretRedactor, call: &crate::ast::Invocation) -> Result<()> {
        for value in call.parameters.values() {
            register_sensitive_ast_value(redactor, value)?;
        }
        Ok(())
    }
    fn register_embedded_calls(
        redactor: &SecretRedactor,
        step: &crate::ast::Step,
        only_marked: bool,
    ) -> Result<()> {
        use crate::ast::{LoopKind, StepKind};
        let calls: Vec<&crate::ast::Invocation> = match &step.kind {
            StepKind::TaskGroup { tasks, .. } => tasks.iter().collect(),
            StepKind::SpawnJob { stages, .. } => stages.iter().map(|stage| &stage.call).collect(),
            StepKind::IfCommand { call, .. }
            | StepKind::Loop {
                kind: LoopKind::WhileCommand { call, .. },
                ..
            } => vec![call],
            _ => Vec::new(),
        };
        for call in calls {
            if !only_marked || call.marks_sensitive_output() {
                register_call(redactor, call)?;
            }
        }
        Ok(())
    }
    fn register_region(
        redactor: &SecretRedactor,
        steps: &[crate::ast::Step],
        start: usize,
        end: usize,
    ) -> Result<()> {
        use crate::ast::StepKind;
        let mut index = start;
        while index < end {
            let step = &steps[index];
            match &step.kind {
                StepKind::Invoke { call, .. } => register_call(redactor, call)?,
                StepKind::CallProgram {
                    call, end: span, ..
                } => {
                    register_call(redactor, call)?;
                    let body_end = index.saturating_add((*span).max(1)).min(end);
                    if body_end > index + 1 {
                        register_region(redactor, steps, index + 1, body_end - 1)?;
                    }
                    index = body_end.saturating_sub(1);
                }
                StepKind::UseValue { source } | StepKind::BindValue { source, .. } => {
                    register_sensitive_ast_value(redactor, source)?;
                }
                StepKind::EncodeJson { source } => register_sensitive_ast_value(redactor, source)?,
                _ => {}
            }
            register_embedded_calls(redactor, step, false)?;
            index += 1;
        }
        Ok(())
    }

    fn scan(redactor: &SecretRedactor, steps: &[crate::ast::Step]) -> Result<()> {
        use crate::ast::StepKind;
        for (index, step) in steps.iter().enumerate() {
            match &step.kind {
                StepKind::Invoke { call, .. } if call.marks_sensitive_output() => {
                    register_call(redactor, call)?;
                }
                StepKind::CallProgram { call, end, .. } if call.marks_sensitive_output() => {
                    register_call(redactor, call)?;
                    let body_end = index.saturating_add(*end).min(steps.len());
                    if body_end > index + 1 {
                        register_region(redactor, steps, index + 1, body_end - 1)?;
                    }
                }
                _ => register_embedded_calls(redactor, step, true)?,
            }
        }
        Ok(())
    }

    scan(&redactor, &program.steps)?;
    for definition in program.definitions.values() {
        scan(&redactor, &definition.steps)?;
    }
    Ok(())
}

fn register_sensitive_ast_value(
    redactor: &SecretRedactor,
    value: &crate::ast::Value,
) -> Result<()> {
    let text = match value {
        crate::ast::Value::String(value) => value.clone(),
        crate::ast::Value::Integer(value) => value.to_string(),
        crate::ast::Value::Number(value) => value.to_string(),
        crate::ast::Value::List(values) => {
            return values
                .iter()
                .try_for_each(|value| register_sensitive_ast_value(redactor, value));
        }
        crate::ast::Value::Record(values) => {
            for (key, value) in values {
                redactor
                    .register(key.as_bytes())
                    .map_err(|error| anyhow!("registering sensitive source key: {error}"))?;
                register_sensitive_ast_value(redactor, value)?;
            }
            return Ok(());
        }
        crate::ast::Value::Bool(_) | crate::ast::Value::Null | crate::ast::Value::Reference(_) => {
            return Ok(());
        }
    };
    redactor
        .register(text.as_bytes())
        .map_err(|error| anyhow!("registering sensitive program value: {error}"))
}

/// Whether the active data-flow policy declares secret environment variables for this
/// applet, so its child environment must be built explicitly to register their values.
pub fn declares_secret_environment(registry: &Registry, applet_name: &str) -> bool {
    let policy = registry.host_state::<DataFlowPolicy>();
    policy.active
        && policy
            .secret_environment
            .get(applet_name)
            .is_some_and(|names| !names.is_empty())
}

/// Capture explicit secret environment declarations from the already-filtered
/// environment snapshot that will be passed to this applet. No ambient
/// process environment is read here.
pub fn capture_declared_environment_secrets(
    registry: &Registry,
    applet_name: &str,
    effective_environment: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Result<(), PolicyError> {
    let policy = registry.host_state::<DataFlowPolicy>();
    if !policy.active {
        return Ok(());
    }
    let Some(declared_names) = policy.secret_environment.get(applet_name) else {
        return Ok(());
    };
    if declared_names.is_empty() {
        return Ok(());
    }
    let redactor = secret_redactor(registry);
    for (name, value) in effective_environment {
        let Some(name) = name.to_str() else {
            continue;
        };
        let declared = declared_names.iter().any(|declared| {
            if cfg!(windows) {
                declared.eq_ignore_ascii_case(name)
            } else {
                declared == name
            }
        });
        if declared {
            register_environment_secret(&redactor, value)?;
        }
    }
    Ok(())
}

/// Every environment variable name the active data-flow policy declares secret,
/// sorted and without values. Used to name what a run record redacted.
pub fn declared_secret_environment_names(registry: &Registry) -> Vec<String> {
    let policy = registry.host_state::<DataFlowPolicy>();
    if !policy.active {
        return Vec::new();
    }
    let names: BTreeSet<String> = policy
        .secret_environment
        .values()
        .flatten()
        .cloned()
        .collect();
    names.into_iter().collect()
}

/// Register every declared secret environment value present in `environment`
/// with the shared redactor, for every applet the registry knows. Records use
/// this before a run starts, so secrets are redacted even before an applet
/// launches with them.
pub fn register_declared_environment_secrets(
    registry: &Registry,
    environment: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Result<(), PolicyError> {
    for name in registry.applet_names() {
        capture_declared_environment_secrets(registry, name, environment)?;
    }
    Ok(())
}

/// Whether policy declares this applet to receive credential-bearing
/// environment inputs. Runtime dataflow and persistence checks should treat
/// its outputs as `Secret`, even before a cache lookup or process launch, so
/// a cache hit cannot bypass the provenance decision.
pub fn applet_has_declared_secret_environment(registry: &Registry, applet_name: &str) -> bool {
    let policy = registry.host_state::<DataFlowPolicy>();
    policy.active
        && policy
            .secret_environment
            .get(applet_name)
            .is_some_and(|names| !names.is_empty())
}

#[cfg(unix)]
fn register_environment_secret(
    redactor: &SecretRedactor,
    value: &std::ffi::OsStr,
) -> Result<(), PolicyError> {
    use std::os::unix::ffi::OsStrExt;
    redactor.register(value.as_bytes())
}

#[cfg(windows)]
fn register_environment_secret(
    redactor: &SecretRedactor,
    value: &std::ffi::OsStr,
) -> Result<(), PolicyError> {
    // Environment variables used as credentials should be representable as
    // Unicode. Windows process stderr is byte-oriented, so register its UTF-8
    // form, which is what the CLI and supported protocol clients emit.
    let value = value
        .to_str()
        .ok_or(PolicyError::SecretRedactionUnavailable)?;
    redactor.register(value.as_bytes())
}

#[cfg(not(any(unix, windows)))]
fn register_environment_secret(
    redactor: &SecretRedactor,
    value: &std::ffi::OsStr,
) -> Result<(), PolicyError> {
    let value = value
        .to_str()
        .ok_or(PolicyError::SecretRedactionUnavailable)?;
    redactor.register(value.as_bytes())
}

/// Join declared labels without declassification. `Unknown` stays distinct
/// from `Public` and dominates every known label.
pub fn join_sensitivities(labels: impl IntoIterator<Item = Sensitivity>) -> Sensitivity {
    labels
        .into_iter()
        .max_by_key(|label| sensitivity_rank(*label))
        .unwrap_or(Sensitivity::Unknown)
}

/// Authorize one applet data flow to a remote endpoint. `endpoint` may include
/// the transport's root path; the policy always matches scheme/host/port only.
/// The call records the policy decision without retaining payload values.
pub fn authorize_data_flow(
    registry: &Registry,
    subject: &str,
    endpoint: &str,
    sensitivity: Sensitivity,
) -> Result<PolicyDecision, PolicyError> {
    authorize_data_flow_set(
        registry,
        subject,
        endpoint,
        &SensitivitySet::from_label(sensitivity),
    )
}

/// Authorize a remote sink against every label known to contribute to it.
/// Unknown and a known confidential label remain separate exact grants.
pub fn authorize_data_flow_set(
    registry: &Registry,
    subject: &str,
    endpoint: &str,
    sensitivities: &SensitivitySet,
) -> Result<PolicyDecision, PolicyError> {
    let labels = effective_labels(sensitivities);
    let origin = match canonical_origin(endpoint) {
        Ok(origin) => origin,
        Err(()) => {
            record_policy_decision(
                registry,
                subject,
                "network.egress",
                None,
                PolicyRisk::High,
                PolicyDecisionOutcome::Denied,
                PolicyDecisionReason::NetworkOriginNotGranted,
            );
            return Err(PolicyError::InvalidOrigin);
        }
    };
    let policy = registry.host_state::<DataFlowPolicy>();
    if !policy.active {
        return Ok(record_policy_decision(
            registry,
            subject,
            "network.egress",
            Some(&origin),
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::NoPolicyConfigured,
        ));
    }
    let allowed = policy
        .remote_sinks
        .get(&origin)
        .is_some_and(|grants| labels.iter().all(|label| grants.contains(label)));
    let reason = if allowed {
        PolicyDecisionReason::DataFlowGranted
    } else if labels.contains(&Sensitivity::Unknown) {
        PolicyDecisionReason::UnclassifiedDataDenied
    } else {
        PolicyDecisionReason::DataFlowDenied
    };
    let decision = record_policy_decision(
        registry,
        subject,
        "network.egress",
        Some(&origin),
        PolicyRisk::High,
        if allowed {
            PolicyDecisionOutcome::Approved
        } else {
            PolicyDecisionOutcome::Denied
        },
        reason,
    );
    if allowed {
        Ok(decision)
    } else {
        Err(PolicyError::DataFlowDenied)
    }
}

/// Authorize bytes before a cache, checkpoint, or file write retains them.
/// Public values are safe by default. Unknown and Secret are never persisted
/// as plaintext; Internal and Confidential require an explicit grant.
pub fn authorize_persistence(
    registry: &Registry,
    subject: &str,
    kind: PersistenceKind,
    sensitivity: Sensitivity,
) -> Result<PolicyDecision, PolicyError> {
    authorize_persistence_set(
        registry,
        subject,
        kind,
        &SensitivitySet::from_label(sensitivity),
    )
}

/// Authorize a persistent sink against every label known to contribute to it.
/// A Secret label always rejects plaintext persistence, including in mixed
/// flows where another source is Unknown.
pub fn authorize_persistence_set(
    registry: &Registry,
    subject: &str,
    kind: PersistenceKind,
    sensitivities: &SensitivitySet,
) -> Result<PolicyDecision, PolicyError> {
    let labels = effective_labels(sensitivities);
    let policy = registry.host_state::<DataFlowPolicy>();
    let allowed = if !policy.active {
        !labels.contains(&Sensitivity::Secret)
    } else {
        labels.iter().all(|sensitivity| match sensitivity {
            Sensitivity::Public => true,
            Sensitivity::Unknown | Sensitivity::Secret => false,
            Sensitivity::Internal | Sensitivity::Confidential => policy
                .persistence
                .get(&kind)
                .is_some_and(|labels| labels.contains(sensitivity)),
        })
    };
    let contains_secret = labels.contains(&Sensitivity::Secret);
    let contains_unknown = labels.contains(&Sensitivity::Unknown);
    let reason = match (policy.active, contains_secret, contains_unknown, allowed) {
        (_, true, _, _) => PolicyDecisionReason::SecretPersistenceDenied,
        (false, false, _, true) => PolicyDecisionReason::NoPolicyConfigured,
        (true, false, true, _) => PolicyDecisionReason::UnclassifiedDataDenied,
        (_, false, false, true) => PolicyDecisionReason::PersistenceGranted,
        (_, false, false, false) => PolicyDecisionReason::PersistenceDenied,
        _ => PolicyDecisionReason::PersistenceDenied,
    };
    let decision = record_policy_decision(
        registry,
        subject,
        kind.action(),
        None,
        if policy.active {
            PolicyRisk::Elevated
        } else {
            PolicyRisk::Low
        },
        if allowed {
            PolicyDecisionOutcome::Approved
        } else {
            PolicyDecisionOutcome::Denied
        },
        reason,
    );
    if allowed {
        Ok(decision)
    } else if contains_secret {
        Err(PolicyError::SecretPersistenceDenied)
    } else {
        Err(PolicyError::PersistenceDenied)
    }
}

fn effective_labels(sensitivities: &SensitivitySet) -> BTreeSet<Sensitivity> {
    if sensitivities.is_empty() {
        BTreeSet::from([Sensitivity::Unknown])
    } else {
        sensitivities.0.clone()
    }
}

/// Enforce the opt-in cache-purity contract before looking up or writing a
/// cache entry. When no data-flow policy is installed, legacy cache behavior
/// is preserved. With a policy installed, only fixed reviewed in-process
/// operations pass by default; other applets need an explicit trusted policy
/// entry because declarations alone cannot prove native code purity.
pub fn authorize_cache_purity(registry: &Registry, manifest: &Manifest) -> Result<(), PolicyError> {
    let policy = registry.host_state::<DataFlowPolicy>();
    if !policy.active {
        // Without a data-flow policy every cache lookup is allowed, and the
        // audit says so explicitly instead of staying silent about it.
        record_policy_decision(
            registry,
            &manifest.name,
            "cache.purity",
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::NoPolicyConfigured,
        );
        return Ok(());
    }

    let versioned_contract = manifest
        .execution_contract()
        .is_some_and(|contract| contract.version == 1);
    let trusted_override = policy.trusted_cache_applets.contains(&manifest.name);
    let reviewed_pure = reviewed_pure_in_process(manifest);
    let allowed = manifest.cacheable && versioned_contract && (trusted_override || reviewed_pure);
    let reason = if allowed {
        PolicyDecisionReason::CachePurityGranted
    } else {
        PolicyDecisionReason::CachePurityDenied
    };
    record_policy_decision(
        registry,
        &manifest.name,
        "cache.purity",
        None,
        PolicyRisk::Elevated,
        if allowed {
            PolicyDecisionOutcome::Approved
        } else {
            PolicyDecisionOutcome::Denied
        },
        reason,
    );
    if allowed {
        Ok(())
    } else {
        Err(PolicyError::CachePurityDenied)
    }
}

/// Treat an applet that declares a host filesystem write effect as a
/// persistence sink for the current workflow's effective labels.
pub fn authorize_declared_filesystem_write(
    registry: &Registry,
    manifest: &Manifest,
    sensitivities: &SensitivitySet,
) -> Result<(), PolicyError> {
    let declares_write = manifest.execution_contract().is_some_and(|contract| {
        contract
            .effects
            .iter()
            .any(|effect| effect == "filesystem.write")
    });
    if declares_write {
        authorize_persistence_set(
            registry,
            &manifest.name,
            PersistenceKind::Save,
            sensitivities,
        )?;
    }
    Ok(())
}

fn reviewed_pure_in_process(manifest: &Manifest) -> bool {
    if !manifest.cacheable
        || manifest.binary != "@execute-applet"
        || !REVIEWED_PURE_IN_PROCESS_APPLETS.contains(&manifest.name.as_str())
    {
        return false;
    }
    let Some(contract) = manifest.execution_contract() else {
        return false;
    };
    contract.version == 1
        && contract.backend == AppletBackend::InProcess
        && contract.remote.is_none()
        && matches!(&contract.lifecycle, crate::contracts::Lifecycle::OneShot)
        && contract.effects.is_empty()
        && contract
            .capabilities
            .iter()
            .all(|capability| PURE_CAPABILITIES.contains(&capability.as_str()))
}

/// Risk level recorded with one policy authorization result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRisk {
    Low,
    Elevated,
    High,
}

/// Whether a policy-approved action was allowed to proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionOutcome {
    Approved,
    Denied,
}

/// A bounded reason code for an approval or denial record. These are enums so
/// callers cannot accidentally attach payload data, credentials, or paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionReason {
    PolicyMatched,
    MissingGrant,
    CapabilityNotGranted,
    EffectNotGranted,
    BinaryIdentityMismatch,
    NetworkOriginNotGranted,
    DataFlowGranted,
    DataFlowDenied,
    UnclassifiedDataDenied,
    PersistenceGranted,
    PersistenceDenied,
    SecretPersistenceDenied,
    CachePurityGranted,
    CachePurityDenied,
    NoPolicyConfigured,
}

/// One in-memory policy decision record. Scope contains only a canonical
/// network origin; it never stores endpoint paths, request values or secrets.
/// The same shape is persisted into run records, so it stays deserializable.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PolicyDecision {
    pub sequence: u64,
    pub subject: String,
    pub action: String,
    pub scope_origin: Option<String>,
    pub risk: PolicyRisk,
    pub outcome: PolicyDecisionOutcome,
    pub reason: PolicyDecisionReason,
}

/// Bounded in-memory approval history shared by Registry clones.
#[derive(Default)]
pub struct DecisionRecorder {
    sequence: AtomicU64,
    records: Mutex<VecDeque<PolicyDecision>>,
}

impl DecisionRecorder {
    pub fn snapshot(&self) -> Vec<PolicyDecision> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    fn push(&self, mut decision: PolicyDecision) -> PolicyDecision {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The sequence is assigned under the queue lock, so a snapshot taken
        // under the same lock always agrees with the lifetime total: every
        // assigned sequence is already queued, and no assigned decision is
        // still waiting for its number.
        decision.sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        if records.len() == MAX_DECISION_RECORDS {
            records.pop_front();
        }
        records.push_back(decision.clone());
        decision
    }

    /// Snapshot the bounded window together with the lifetime total, under one
    /// lock acquisition. Reading the two separately can observe a push in
    /// between, so an audit built from separate reads can claim a total past
    /// its own last sequence and fail its own contiguity check. This pair
    /// never disagrees, even while other threads are recording.
    pub fn snapshot_with_total(&self) -> (Vec<PolicyDecision>, u64) {
        let records = self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            records.iter().cloned().collect(),
            self.sequence.load(Ordering::Relaxed),
        )
    }

    /// How many decisions have ever been recorded through this recorder. The
    /// snapshot above is a bounded window; this counter is the lifetime total,
    /// so a persisted audit can state its truncation honestly even after the
    /// window has dropped the prefix.
    pub fn total(&self) -> u64 {
        self.sequence.load(Ordering::Relaxed)
    }
}

/// Record a policy authorization outcome without including dynamic values.
/// Invalid scope strings are omitted rather than logged verbatim.
pub fn record_policy_decision(
    registry: &Registry,
    subject: &str,
    action: &str,
    scope_origin: Option<&str>,
    risk: PolicyRisk,
    outcome: PolicyDecisionOutcome,
    reason: PolicyDecisionReason,
) -> PolicyDecision {
    let decision = PolicyDecision {
        sequence: 0,
        subject: safe_identifier(subject),
        action: safe_identifier(action),
        scope_origin: scope_origin.and_then(|origin| canonical_origin(origin).ok()),
        risk,
        outcome,
        reason,
    };
    registry.host_state::<DecisionRecorder>().push(decision)
}

/// Read the current task's bounded decision log.
pub fn policy_decisions(registry: &Registry) -> Vec<PolicyDecision> {
    registry.host_state::<DecisionRecorder>().snapshot()
}

/// Lifetime total of recorded policy decisions, including ones the bounded
/// window has already dropped. A persisted audit reports this total next to
/// the decisions it kept, so truncation is explicit rather than silent.
pub fn policy_decision_total(registry: &Registry) -> u64 {
    registry.host_state::<DecisionRecorder>().total()
}

/// The bounded decision window together with the lifetime total, read under
/// one lock acquisition so the two cannot disagree. A persisted audit must be
/// built from this pair, never from separate `policy_decisions` and
/// `policy_decision_total` reads with pushes in between.
pub fn policy_decisions_with_total(registry: &Registry) -> (Vec<PolicyDecision>, u64) {
    registry
        .host_state::<DecisionRecorder>()
        .snapshot_with_total()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyError {
    PolicyUnavailable,
    InvalidPolicy,
    InvalidOrigin,
    DataFlowDenied,
    PersistenceDenied,
    SecretPersistenceDenied,
    CachePurityDenied,
    RedactionLimitExceeded,
    SecretRedactionUnavailable,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PolicyUnavailable => "data-flow policy is unavailable",
            Self::InvalidPolicy => "data-flow policy is invalid",
            Self::InvalidOrigin => "remote origin is invalid for data-flow authorization",
            Self::DataFlowDenied => "remote data flow is denied by policy",
            Self::PersistenceDenied => "data persistence is denied by policy",
            Self::SecretPersistenceDenied => "secret data cannot be persisted without encryption",
            Self::CachePurityDenied => "applet cache purity is not authorized by policy",
            Self::RedactionLimitExceeded => "secret redaction registration limit exceeded",
            Self::SecretRedactionUnavailable => {
                "declared secret cannot be safely registered for redaction"
            }
        })
    }
}

impl std::error::Error for PolicyError {}

struct UniqueMap<T>(BTreeMap<String, T>);

impl<T> Default for UniqueMap<T> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for UniqueMap<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueMapVisitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for UniqueMapVisitor<T> {
            type Value = UniqueMap<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object with unique keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = access.next_entry::<String, T>()? {
                    if values.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate policy key"));
                    }
                }
                Ok(UniqueMap(values))
            }
        }
        deserializer.deserialize_map(UniqueMapVisitor(std::marker::PhantomData))
    }
}

// Data-flow policies and capability policies canonicalize origins with the same parser,
// so an origin one accepts can never be spelled differently in the other.
fn canonical_origin(value: &str) -> Result<String, ()> {
    crate::capabilities::canonical_http_origin(value).map_err(|_| ())
}

fn canonical_policy_origin(value: &str) -> Result<String, ()> {
    crate::capabilities::canonical_http_policy_origin(value).map_err(|_| ())
}

fn sensitivity_rank(sensitivity: Sensitivity) -> u8 {
    match sensitivity {
        Sensitivity::Public => 0,
        Sensitivity::Internal => 1,
        Sensitivity::Confidential => 2,
        Sensitivity::Secret => 3,
        Sensitivity::Unknown => 4,
    }
}

fn safe_identifier(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        value.to_owned()
    } else {
        "unknown".into()
    }
}

fn valid_applet_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    value.len() <= 256
        && (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Whether a trusted input must name a directory or a regular file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrustedInputKind {
    Directory,
    File,
}

/// Ownership, writability and link gate for files that decide what runs:
/// manifest directories and files, module roots and sources, and the bundled
/// helper binaries. Unix leaf inputs must be owned by the effective user (or
/// root, which owns installed helpers and manifest trees) and must not be
/// group/world-writable, mirroring the cache rules in `src/runtime/cache.rs`.
/// File leaves additionally refuse setuid/setgid bits and extra hardlinks.
/// Every ancestor directory of the canonical path passes the same ownership
/// and writability check (a writable parent lets an attacker rename or
/// replace the verified leaf), except a sticky root-owned directory such as
/// `/tmp`, where the sticky bit stops cross-user renames. Symlinks are
/// refused so a link cannot redirect a trusted name at an untrusted target;
/// callers that accept an alias must canonicalize first and verify the
/// canonical target with these functions. On Linux a leaf or ancestor with a
/// POSIX `system.posix_acl_access` ACL fails closed: mode bits cannot attest
/// ACL-granted writes, so an ACL-bearing path is refused rather than trusted.
/// NFSv4, RichACL and macOS ACLs are not probed here. Other Unix systems do
/// not expose ACLs here and remain documented as best-effort (see
/// SECURITY.md). Windows inputs must not be reparse points, must be owned by
/// the current user, SYSTEM, Administrators or TrustedInstaller, and must grant no write access
/// to any other principal; an unreadable ACL fails closed. A private-DACL
/// demand is deliberately not made: installed trees stay readable by every
/// local runner, so read grants to others are allowed and only write grants
/// are refused; see SECURITY.md for the extraction-location guidance. Every
/// check stats the path without following a trailing link, but a replacement
/// between this check and a later open, read or exec remains a race; callers
/// narrow it with no-follow opens plus `verify_trusted_fd` on the opened
/// descriptor, and the residual race is documented in SECURITY.md alongside
/// the same-user limit. This gate therefore narrows check-then-use races but
/// does not eliminate them.
pub(crate) fn verify_trusted_input_dir(path: &Path, label: &str) -> Result<std::fs::Metadata> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| anyhow!("reading {label} {}: {error}", path.display()))?;
    verify_trusted_metadata(path, &metadata, TrustedInputKind::Directory, label)?;
    verify_trusted_ancestors(path, label)?;
    Ok(metadata)
}

/// See [`verify_trusted_input_dir`]: the same ownership, writability and link
/// gate for a single trusted file.
pub(crate) fn verify_trusted_input_file(path: &Path, label: &str) -> Result<std::fs::Metadata> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| anyhow!("reading {label} {}: {error}", path.display()))?;
    verify_trusted_metadata(path, &metadata, TrustedInputKind::File, label)?;
    verify_trusted_ancestors(path, label)?;
    Ok(metadata)
}

/// Enforce the trusted-input gate on already-stat metadata. Callers that
/// fetched their own `symlink_metadata` (to keep a distinct missing-file
/// diagnostic) use this instead of stat-ing twice.
pub(crate) fn verify_trusted_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
    kind: TrustedInputKind,
    label: &str,
) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "{label} {} is a symbolic link; pass the real path",
            path.display()
        ));
    }
    #[cfg(windows)]
    if crate::platform::windows::is_reparse(metadata) {
        return Err(anyhow!(
            "{label} {} is a reparse point; pass the real path",
            path.display()
        ));
    }
    let kind_ok = match kind {
        TrustedInputKind::Directory => metadata.is_dir(),
        TrustedInputKind::File => metadata.is_file(),
    };
    if !kind_ok {
        let noun = match kind {
            TrustedInputKind::Directory => "a directory",
            TrustedInputKind::File => "a regular file",
        };
        return Err(anyhow!("{label} {} is not {noun}", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid takes no arguments and cannot fail; the result is
        // compared with the owner uid in `metadata`.
        let owned =
            metadata.uid() == crate::platform::unix_ffi::effective_uid() || metadata.uid() == 0;
        if !owned || metadata.mode() & 0o022 != 0 {
            return Err(anyhow!(
                "{label} {} must be owned by the current user or root and not group/world-writable",
                path.display()
            ));
        }
        // A setuid/setgid helper or manifest would run or attest with elevated
        // privilege; refuse the bits outright. An extra hardlink lets an
        // attacker-controlled directory entry reach the same inode, so only a
        // single link is trusted.
        // Setuid/setgid and extra hardlinks are file-only gates: setgid
        // directories are a benign sharing layout, and directories always
        // carry extra links (`.` and each child directory's `..`). An extra
        // file hardlink lets an attacker-controlled directory entry reach the
        // same inode, and a setuid helper would run with elevated privilege.
        if kind == TrustedInputKind::File && metadata.mode() & 0o6000 != 0 {
            return Err(anyhow!(
                "{label} {} must not carry setuid/setgid bits",
                path.display()
            ));
        }
        if kind == TrustedInputKind::File && metadata.nlink() > 1 {
            return Err(anyhow!(
                "{label} {} has extra hardlinks; only a single link is trusted",
                path.display()
            ));
        }
        if has_posix_extended_acl(path) {
            return Err(anyhow!(
                "{label} {} has an extended ACL that mode bits cannot attest; remove the ACL or move the input",
                path.display()
            ));
        }
    }
    #[cfg(windows)]
    {
        verify_trusted_windows_acl(path, label)?;
    }
    Ok(())
}

/// Re-check the ownership, writability and link gate against an already-opened
/// descriptor. Callers open with no-follow semantics first, then call this
/// with the open `File` plus its `fstat` metadata, so a path swapped for a
/// link, a differently owned inode, a widened mode, a newly attached ACL, or
/// a changed owner/ACE between the path check and the open fails closed
/// instead of being read or executed. The descriptor cannot be a trailing
/// symlink after a no-follow open, but the check keeps the regular-file/dir,
/// owner, mode, setuid/setgid, hardlink and ACL requirements identical to
/// [`verify_trusted_metadata`]: on Linux the ACL probe runs against the open
/// descriptor (`fgetxattr`), not the path, and on Windows the owner plus
/// no-foreign-write-ACE gate runs against the open handle.
pub(crate) fn verify_trusted_fd(
    path: &Path,
    file: &File,
    metadata: &std::fs::Metadata,
    kind: TrustedInputKind,
    label: &str,
) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "{label} {} changed to a symbolic link while opening",
            path.display()
        ));
    }
    #[cfg(windows)]
    if crate::platform::windows::is_reparse(metadata) {
        return Err(anyhow!(
            "{label} {} changed to a reparse point while opening",
            path.display()
        ));
    }
    let kind_ok = match kind {
        TrustedInputKind::Directory => metadata.is_dir(),
        TrustedInputKind::File => metadata.is_file(),
    };
    if !kind_ok {
        let noun = match kind {
            TrustedInputKind::Directory => "a directory",
            TrustedInputKind::File => "a regular file",
        };
        return Err(anyhow!(
            "{label} {} is not {noun} after opening",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owned =
            metadata.uid() == crate::platform::unix_ffi::effective_uid() || metadata.uid() == 0;
        if !owned || metadata.mode() & 0o022 != 0 {
            return Err(anyhow!(
                "{label} {} failed the ownership check after opening: must be owned by the current user or root and not group/world-writable",
                path.display()
            ));
        }
        if kind == TrustedInputKind::File && metadata.mode() & 0o6000 != 0 {
            return Err(anyhow!(
                "{label} {} gained setuid/setgid bits while opening",
                path.display()
            ));
        }
        if kind == TrustedInputKind::File && metadata.nlink() > 1 {
            return Err(anyhow!(
                "{label} {} has extra hardlinks after opening",
                path.display()
            ));
        }
        // Descriptor-side ACL check: a POSIX ACL attached between the path
        // check and the open must fail closed here. The probe runs on the
        // open descriptor so a further path swap cannot hide it. Outside
        // Linux the probe reports no ACL (documented gap).
        if has_posix_extended_acl_fd(file) {
            return Err(anyhow!(
                "{label} {} has an extended ACL that mode bits cannot attest; remove the ACL or move the input",
                path.display()
            ));
        }
    }
    #[cfg(windows)]
    {
        // Handle-side owner/ACE re-check: an owner or DACL swap between the
        // path check and the open must fail closed here.
        check_trusted_windows_handle(file, path, label, false)?;
    }
    Ok(())
}

/// Check every ancestor directory of the canonical form of `path` for the
/// same ownership and writability gate as the leaf: a writable ancestor lets
/// an attacker rename or replace the verified leaf or helper binary. The leaf
/// itself is not re-checked here. A sticky root- or self-owned directory
/// (the `/tmp` shape: mode `1777`, owner root) is accepted because the sticky
/// bit blocks cross-user renames inside it. Symlinked ancestors are refused;
/// pass a canonical path (callers canonicalize before verifying the leaf).
/// On Linux an ancestor carrying a POSIX `system.posix_acl_access` ACL fails
/// closed for the same reason as a leaf ACL (NFSv4/RichACL/macOS ACLs are not
/// probed). The filesystem root is checked as well.
pub(crate) fn verify_trusted_ancestors(path: &Path, label: &str) -> Result<()> {
    #[cfg(windows)]
    {
        verify_trusted_windows_ancestors(path, label)
    }
    #[cfg(not(windows))]
    {
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| anyhow!("canonicalizing {label} {}: {error}", path.display()))?;
        let mut current = canonical.as_path();
        // Walk from the leaf's parent up to and including the filesystem root.
        while let Some(parent) = current.parent() {
            current = parent;
            let metadata = std::fs::symlink_metadata(current).map_err(|error| {
                anyhow!("reading {label} ancestor {}: {error}", current.display())
            })?;
            if metadata.file_type().is_symlink() {
                return Err(anyhow!(
                    "{label} ancestor {} is a symbolic link; pass the real path",
                    current.display()
                ));
            }
            if !metadata.is_dir() {
                return Err(anyhow!(
                    "{label} ancestor {} is not a directory",
                    current.display()
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let owned = metadata.uid() == crate::platform::unix_ffi::effective_uid()
                    || metadata.uid() == 0;
                if !owned {
                    return Err(anyhow!(
                        "{label} ancestor {} must be owned by the current user or root",
                        current.display()
                    ));
                }
                let writable = metadata.mode() & 0o022 != 0;
                // A sticky directory owned by root (or by us) still blocks an
                // attacker from renaming our entries, so `/tmp`-style parents
                // stay usable while plain group/world-writable parents fail.
                let sticky_guard = metadata.mode() & 0o1000 != 0
                    && (metadata.uid() == 0
                        || metadata.uid() == crate::platform::unix_ffi::effective_uid());
                if writable && !sticky_guard {
                    return Err(anyhow!(
                        "{label} ancestor {} is group/world-writable",
                        current.display()
                    ));
                }
                if has_posix_extended_acl(current) {
                    return Err(anyhow!(
                        "{label} ancestor {} has an extended ACL that mode bits cannot attest; remove the ACL or move the input",
                        current.display()
                    ));
                }
            }
            // `Path::parent` of `/` is `None`, so the root itself is checked
            // on the final iteration before this loop condition ends.
            if current.parent().is_none() {
                break;
            }
        }
        Ok(())
    }
}

/// Whether `path` carries a POSIX extended ACL that mode bits cannot attest.
/// Linux-only probe of the `system.posix_acl_access` extended attribute, read
/// without following a trailing link. This covers POSIX ACLs only: NFSv4,
/// RichACL and macOS ACLs use different attributes and are not detected here.
/// Any present attribute (including an empty one) fails closed at the call
/// site; missing-attribute, unsupported-filesystem and permission errors mean
/// "no attested ACL" and return false so plain layouts keep working. Other
/// Unix systems have no cheap link-safe probe here and return false; both
/// gaps are documented in SECURITY.md rather than overstated.
#[cfg(unix)]
fn has_posix_extended_acl(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let Ok(c_path) = std::ffi::CString::new(bytes) else {
            return false;
        };
        let name = c"system.posix_acl_access";
        // SAFETY: both pointers are live NUL-terminated strings; a null buffer
        // with size zero only queries the attribute size and changes nothing.
        let size = unsafe {
            libc::lgetxattr(
                c_path.as_ptr(),
                name.as_ptr().cast(),
                std::ptr::null_mut(),
                0,
            )
        };
        size > 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

/// Whether the already-open `file` carries a POSIX extended ACL. Linux-only
/// descriptor probe via `fgetxattr(system.posix_acl_access)`: unlike the
/// path-side probe it cannot be redirected by a rename between the path check
/// and the open. Same scope limit: POSIX ACLs only, not NFSv4/RichACL/macOS
/// ACLs. Errors mean "no attested ACL" and return false.
#[cfg(unix)]
fn has_posix_extended_acl_fd(file: &File) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let name = c"system.posix_acl_access";
        // SAFETY: the fd is a live open descriptor; a null buffer with size
        // zero only queries the attribute size and changes nothing.
        let size = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr().cast(),
                std::ptr::null_mut(),
                0,
            )
        };
        size > 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        false
    }
}

/// Allow-ACE rights that fail the trusted-input gate at the LEAF: data and
/// extended-attribute writes, child add/delete, attribute writes, delete,
/// DACL/owner takeover, and the generic umbrellas covering them. Read-level
/// rights stay out, including `FILE_READ_EA` (`0x8`): standard read grants
/// carry it, so keeping it here would refuse every path that lets another
/// principal read, such as `C:\Users` on a shared host. `GENERIC_READ` and
/// `GENERIC_EXECUTE` are likewise absent because they map to read-only
/// rights.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const TRUSTED_WINDOWS_WRITE_MASK: u32 =
    0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x10000 | 0x40000 | 0x80000 | 0x40000000 | 0x10000000;

/// Whether an allow-ACE access mask grants write-level rights under
/// [`TRUSTED_WINDOWS_WRITE_MASK`]. Pure mask logic with no platform calls, so
/// the gate's read/write boundary is pinned by unit tests on every host.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_allow_ace_grants_write(mask: u32) -> bool {
    mask & TRUSTED_WINDOWS_WRITE_MASK != 0
}

/// Allow-ACE rights that disqualify an ANCESTOR directory: the subset of
/// [`TRUSTED_WINDOWS_WRITE_MASK`] that lets another principal rename, delete
/// or replace an already-verified entry inside the ancestor. Decided per bit
/// from the ancestor threat model ("a writable ancestor lets an attacker
/// rename or replace the verified leaf"):
/// - `FILE_ADD_FILE` (`0x2`), `FILE_ADD_SUBDIRECTORY`/`FILE_APPEND_DATA`
///   (`0x4`), `FILE_WRITE_EA` (`0x10`) and `FILE_WRITE_ATTRIBUTES` (`0x100`)
///   pass: they let a principal create new entries or alter attributes, but
///   cannot rename, delete or replace an EXISTING verified entry, so they do
///   not break the ancestor threat model. Factory `C:\` grants
///   `BUILTIN\Users` a create/append right (`0x4`) at the root; refusing it
///   would refuse every manifest load on factory Windows installs.
/// - `FILE_DELETE_CHILD` (`0x40`) refuses: it lets a principal delete any
///   child entry, which enables removing the verified leaf to plant a
///   replacement.
/// - `DELETE` (`0x10000`) refuses: it lets a principal delete the ancestor
///   itself, removing the verified path.
/// - `WRITE_DAC` (`0x40000`) and `WRITE_OWNER` (`0x80000`) refuse: they let a
///   principal rewrite the DACL or retake ownership, escalating to full
///   control over the verified path.
/// - `GENERIC_WRITE` (`0x40000000`) and `GENERIC_ALL` (`0x10000000`) refuse:
///   umbrella rights that include the refused rights above.
///
/// `FILE_READ_EA` (`0x8`) is a read right and refuses nowhere (already
/// excluded from the leaf mask).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const TRUSTED_WINDOWS_ANCESTOR_WRITE_MASK: u32 =
    0x40 | 0x10000 | 0x40000 | 0x80000 | 0x40000000 | 0x10000000;

/// Whether an allow-ACE access mask on an ANCESTOR directory disqualifies it
/// under [`TRUSTED_WINDOWS_ANCESTOR_WRITE_MASK`]. Pure mask logic with no
/// platform calls, so the ancestor create/replace boundary is pinned by unit
/// tests on every host. Mirrors the discipline of
/// [`windows_owner_sid_is_trusted`]: one pure predicate per gate boundary.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_ancestor_mask_is_disqualifying(mask: u32) -> bool {
    mask & TRUSTED_WINDOWS_ANCESTOR_WRITE_MASK != 0
}

/// String form of the well-known `NT AUTHORITY\SYSTEM` principal.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const TRUSTED_WINDOWS_SYSTEM_SID: &str = "S-1-5-18";
/// String form of the well-known `BUILTIN\Administrators` group.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const TRUSTED_WINDOWS_ADMINISTRATORS_SID: &str = "S-1-5-32-544";
/// String form of the well-known `NT SERVICE\TrustedInstaller` principal: the
/// OS-installer identity that owns system locations such as the drive root
/// (`C:\`) on factory Windows installs. It holds the same standing as
/// SYSTEM/Administrators in the trusted-input gate.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const TRUSTED_WINDOWS_TRUSTED_INSTALLER_SID: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// Whether an owner SID (string form) is trusted for the Windows trusted-input
/// gate: the current user, SYSTEM, Administrators, or TrustedInstaller. Pure
/// string logic with no platform calls, so the owner boundary is pinned by
/// unit tests on every host. Comparison is ASCII case-insensitive because SID
/// string form is canonically upper-case but never guaranteed by a producer.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_owner_sid_is_trusted(owner_sid: &str, current_user_sid: &str) -> bool {
    (!current_user_sid.is_empty() && owner_sid.eq_ignore_ascii_case(current_user_sid))
        || owner_sid.eq_ignore_ascii_case(TRUSTED_WINDOWS_SYSTEM_SID)
        || owner_sid.eq_ignore_ascii_case(TRUSTED_WINDOWS_ADMINISTRATORS_SID)
        || owner_sid.eq_ignore_ascii_case(TRUSTED_WINDOWS_TRUSTED_INSTALLER_SID)
}

/// Windows writability gate for trusted input leaves. The path must be owned by
/// the current user, SYSTEM, Administrators or TrustedInstaller, and no other
/// principal may hold an allow ACE granting write-level access under the
/// strict leaf mask ([`TRUSTED_WINDOWS_WRITE_MASK`]). Read grants to others
/// are allowed so shared installs keep working; only writes fail closed. A
/// missing DACL (full access to everyone) or an unreadable ACL fails closed
/// and names the path. Deny and audit ACEs grant nothing and are ignored.
/// Every refusal names the trustee SID and the granted mask so a CI log alone
/// diagnoses which principal and right tripped the gate.
#[cfg(windows)]
pub(crate) fn verify_trusted_windows_acl(path: &Path, label: &str) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    };
    let handle = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .with_context(|| format!("reading {label} Windows ACL {}", path.display()))?;
    check_trusted_windows_handle(&handle, path, label, false)
}

/// Ancestor half of the Windows gate: every ancestor directory of the
/// canonical path must pass the same owner check as the leaf, but the
/// no-foreign-write check uses the narrowed ancestor mask
/// ([`TRUSTED_WINDOWS_ANCESTOR_WRITE_MASK`]): create-only rights
/// (`FILE_ADD_FILE`, `FILE_ADD_SUBDIRECTORY`/`FILE_APPEND_DATA`,
/// `FILE_WRITE_EA`, `FILE_WRITE_ATTRIBUTES`) cannot rename or replace an
/// existing verified entry, so they pass here while the leaf still refuses
/// them. Only replacement/removal/takeover rights disqualify an ancestor.
/// Reparse-point ancestors are refused.
#[cfg(windows)]
pub(crate) fn verify_trusted_windows_ancestors(path: &Path, label: &str) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    };
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalizing {label} {}", path.display()))?;
    let mut current = canonical.as_path();
    while let Some(parent) = current.parent() {
        current = parent;
        let metadata = std::fs::symlink_metadata(current)
            .with_context(|| format!("reading {label} ancestor {}", current.display()))?;
        if crate::platform::windows::is_reparse(&metadata) {
            bail!(
                "{label} ancestor {} is a reparse point; pass the real path",
                current.display()
            );
        }
        if !metadata.is_dir() {
            bail!("{label} ancestor {} is not a directory", current.display());
        }
        let handle = std::fs::OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(current)
            .with_context(|| {
                format!("reading {label} ancestor Windows ACL {}", current.display())
            })?;
        check_trusted_windows_handle(&handle, current, label, true)?;
        if current.parent().is_none() {
            break;
        }
    }
    Ok(())
}

/// Shared DACL inspection behind [`verify_trusted_windows_acl`] (leaf,
/// `ancestor = false`) and [`verify_trusted_windows_ancestors`] (`ancestor =
/// true`): owner must be the current user, SYSTEM, Administrators or
/// TrustedInstaller, and no allow ACE may grant disqualifying rights to
/// anyone else. The leaf uses the strict [`TRUSTED_WINDOWS_WRITE_MASK`];
/// ancestors use the narrowed [`TRUSTED_WINDOWS_ANCESTOR_WRITE_MASK`], so
/// create-only rights pass on ancestors while replacement/removal/takeover
/// rights refuse on both. TrustedInstaller holds the same standing as
/// SYSTEM/Administrators as an ACE trustee, so a write grant it holds on a
/// system ancestor does not fail the gate. Refusals name the trustee SID and
/// the granted mask.
#[cfg(windows)]
fn check_trusted_windows_handle(
    handle: &File,
    path: &Path,
    label: &str,
    ancestor: bool,
) -> Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            AclSizeInformation,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            EqualSid, GetAce, GetAclInformation, ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION,
            DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, TOKEN_QUERY,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 0x5;
    const ACE_OBJECT_TYPE_PRESENT: u32 = 0x1;
    const ACE_INHERITED_OBJECT_TYPE_PRESENT: u32 = 0x2;

    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: handle owns a live handle, and all output pointers are live local slots.
    let status = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        if !descriptor.is_null() {
            // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
            unsafe { LocalFree(descriptor) };
        }
        return Err(std::io::Error::from_raw_os_error(status as i32))
            .with_context(|| format!("reading {label} Windows ACL {}", path.display()));
    }
    let result = (|| {
        // SAFETY: GetCurrentProcess returns a valid pseudo-handle needing no close.
        let process = unsafe { GetCurrentProcess() };
        let mut token = std::ptr::null_mut();
        // SAFETY: process is the valid pseudo-handle above; token is an out-slot.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error()).context("reading current user SID");
        }
        // SAFETY: OpenProcessToken succeeded and returned an owned token handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut needed = 0;
        // SAFETY: null buffer with length zero requests the TokenUser buffer size.
        unsafe {
            windows_sys::Win32::Security::GetTokenInformation(
                token.as_raw_handle(),
                windows_sys::Win32::Security::TokenUser,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return Err(std::io::Error::last_os_error()).context("sizing current user SID");
        }
        let mut token_data = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: token_data is aligned and at least `needed` bytes; token stays live.
        if unsafe {
            windows_sys::Win32::Security::GetTokenInformation(
                token.as_raw_handle(),
                windows_sys::Win32::Security::TokenUser,
                token_data.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("reading current user SID");
        }
        // SAFETY: GetTokenInformation wrote a TOKEN_USER at the buffer start.
        let user = unsafe {
            &*(token_data
                .as_ptr()
                .cast::<windows_sys::Win32::Security::TOKEN_USER>())
        };
        if owner.is_null() || user.User.Sid.is_null() {
            bail!("{label} {} has no Windows owner; refusing", path.display());
        }
        let well_known = |kind| {
            let mut storage = [0usize;
                (windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE as usize)
                    .div_ceil(std::mem::size_of::<usize>())];
            let mut size = windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE;
            // SAFETY: storage is aligned with SECURITY_MAX_SID_SIZE bytes; these
            // built-in SIDs take no domain SID.
            if unsafe {
                windows_sys::Win32::Security::CreateWellKnownSid(
                    kind,
                    std::ptr::null_mut(),
                    storage.as_mut_ptr().cast(),
                    &mut size,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("reading trusted Windows principals");
            }
            Ok(storage)
        };
        let system_sid = well_known(windows_sys::Win32::Security::WinLocalSystemSid)?;
        let admins_sid = well_known(windows_sys::Win32::Security::WinBuiltinAdministratorsSid)?;
        // Stringify the owner and current-user SIDs so the acceptance decision
        // runs through the pure [`windows_owner_sid_is_trusted`] predicate
        // and the refusal names the actual owner. Conversion failures fail
        // closed: the input is refused rather than trusted.
        let owner_string =
            trusted_windows_sid_to_string(owner).context("reading trusted Windows owner SID")?;
        // SAFETY: user.User.Sid is non-null (checked above) and lives in the token buffer.
        let user_string = trusted_windows_sid_to_string(user.User.Sid)
            .context("reading trusted Windows user SID")?;
        // Keep the pointer comparison as a second opinion alongside the string
        // predicate: either route trusting the owner accepts the input.
        // SAFETY: owner is non-null in the live descriptor; the other SIDs are
        // live in the token and local buffers.
        let owner_trusted = windows_owner_sid_is_trusted(&owner_string, &user_string)
            || {
                // SAFETY: owner is non-null in the live descriptor; the other
                // SIDs are live in the token and local buffers.
                unsafe {
                    EqualSid(owner, user.User.Sid) != 0
                        || EqualSid(owner, system_sid.as_ptr().cast_mut().cast()) != 0
                        || EqualSid(owner, admins_sid.as_ptr().cast_mut().cast()) != 0
                }
            };
        if !owner_trusted {
            bail!(
                "{label} {} must be owned by the current user, SYSTEM, Administrators or TrustedInstaller (owner {owner_string})",
                path.display()
            );
        }
        if dacl.is_null() {
            bail!(
                "{label} {} has no Windows DACL, granting full access; refusing",
                path.display()
            );
        }
        let mut size = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: dacl is the valid ACL inside the live security descriptor.
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            bail!("{label} {} has an unreadable Windows DACL", path.display());
        }
        for index in 0..size.AceCount {
            let mut raw_ace = std::ptr::null_mut();
            // SAFETY: index is below the ACE count of this valid ACL.
            if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 || raw_ace.is_null() {
                bail!(
                    "{label} {} has an unreadable Windows ACL entry",
                    path.display()
                );
            }
            // SAFETY: GetAce returned an ACE stored inside the live ACL.
            let header = unsafe { &*(raw_ace.cast::<windows_sys::Win32::Security::ACE_HEADER>()) };
            // Only allow ACEs can grant writes; deny/audit ACEs are ignored.
            let (mask, ace_sid) = if header.AceType == ACCESS_ALLOWED_ACE_TYPE {
                // SAFETY: this ACE type guarantees the ACCESS_ALLOWED_ACE layout.
                let ace = unsafe { &*(raw_ace.cast::<ACCESS_ALLOWED_ACE>()) };
                (ace.Mask, std::ptr::addr_of!(ace.SidStart).cast_mut().cast())
            } else if header.AceType == ACCESS_ALLOWED_OBJECT_ACE_TYPE {
                // Layout: header, Mask, Flags, optional 16-byte GUIDs, SidStart.
                // SAFETY: the ACE lies inside the validated ACL; reads stay
                // within its reported AceSize.
                let base = raw_ace as *const u8;
                let ace_size = header.AceSize as usize;
                if ace_size < 12 {
                    bail!(
                        "{label} {} has a malformed Windows ACL entry",
                        path.display()
                    );
                }
                // SAFETY: the ACE lies inside the validated ACL and `ace_size >= 12`, so offsets 4 and 8 are within its reported bounds.
                let mask = unsafe { *(base.add(4) as *const u32) };
                // SAFETY: the ACE lies inside the validated ACL and `ace_size >= 12`, so offsets 4 and 8 are within its reported bounds.
                let flags = unsafe { *(base.add(8) as *const u32) };
                let mut offset = 12usize;
                if flags & ACE_OBJECT_TYPE_PRESENT != 0 {
                    offset += 16;
                }
                if flags & ACE_INHERITED_OBJECT_TYPE_PRESENT != 0 {
                    offset += 16;
                }
                if offset + 8 > ace_size {
                    bail!(
                        "{label} {} has a malformed Windows ACL entry",
                        path.display()
                    );
                }
                // SAFETY: `offset + 8 <= ace_size` was checked above, so the SID start lies within the validated ACE.
                let ace_sid = unsafe { base.add(offset) as *mut _ };
                (mask, ace_sid)
            } else {
                continue;
            };
            // Leaves refuse every write-level right; ancestors refuse only the
            // replacement/removal/takeover subset: create-only rights cannot
            // rename or replace the verified entry below them.
            let disqualifying = if ancestor {
                windows_ancestor_mask_is_disqualifying(mask)
            } else {
                windows_allow_ace_grants_write(mask)
            };
            if !disqualifying {
                continue;
            }
            // SAFETY: the ACE SID lies inside the validated ACL; the compared
            // SIDs live in the token and local buffers.
            let sid_trusted = unsafe {
                EqualSid(ace_sid, user.User.Sid) != 0
                    || EqualSid(ace_sid, owner) != 0
                    || EqualSid(ace_sid, system_sid.as_ptr().cast_mut().cast()) != 0
                    || EqualSid(ace_sid, admins_sid.as_ptr().cast_mut().cast()) != 0
            };
            // TrustedInstaller holds the same standing as SYSTEM/Administrators
            // as an ACE trustee: a write grant it holds on a system ancestor
            // (such as the drive root) does not fail the gate. The string
            // conversion is a fallback checked only when the pointer
            // comparison above refused; an unreadable SID stays refused. The
            // string form is also what the refusal below names, so a CI log
            // alone diagnoses which principal and mask tripped the gate.
            let ace_string = trusted_windows_sid_to_string(ace_sid).ok();
            let sid_trusted = sid_trusted
                || ace_string.as_deref().is_some_and(|ace| {
                    ace.eq_ignore_ascii_case(TRUSTED_WINDOWS_TRUSTED_INSTALLER_SID)
                });
            if !sid_trusted {
                let trustee = ace_string.as_deref().unwrap_or("<unreadable SID>");
                bail!(
                    "{label} {} grants Windows write access to {trustee} (mask {mask:#x}); refusing",
                    path.display()
                );
            }
        }
        Ok(())
    })();
    if !descriptor.is_null() {
        // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
        unsafe { LocalFree(descriptor) };
    }
    result
}

/// Stringify a live SID pointer via `ConvertSidToStringSidW` so the gate can
/// run its pure string predicate and name the SID in diagnostics. The caller
/// keeps the SID alive; the returned string is owned. Any conversion failure
/// is an error so callers fail closed.
#[cfg(windows)]
fn trusted_windows_sid_to_string(sid: *mut std::ffi::c_void) -> Result<String> {
    use std::os::windows::ffi::OsStringExt;
    if sid.is_null() {
        bail!("Windows SID is null; refusing");
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: sid is a live SID owned by the caller; text is a writable
    // out-slot, and the API allocates the returned string for LocalFree.
    let status = unsafe {
        windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(sid, &mut text)
    };
    if status == 0 || text.is_null() {
        return Err(std::io::Error::last_os_error()).context("converting Windows SID to string");
    }
    let mut length = 0usize;
    // SAFETY: ConvertSidToStringSidW returned a NUL-terminated LocalAlloc string.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the counted UTF-16 units precede the validated terminator, and
    // `text` stays live until LocalFree below.
    let units = unsafe { std::slice::from_raw_parts(text, length) };
    let value = std::ffi::OsString::from_wide(units)
        .to_string_lossy()
        .into_owned();
    // SAFETY: the string was allocated by ConvertSidToStringSidW with LocalAlloc.
    unsafe { windows_sys::Win32::Foundation::LocalFree(text.cast()) };
    Ok(value)
}

pub(crate) fn open_private_file(path: &Path) -> std::io::Result<File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid(2) takes no arguments, cannot fail (POSIX), and has no memory
        // preconditions. Its result is compared with the owner uid in `metadata`.
        if metadata.uid() != crate::platform::unix_ffi::effective_uid()
            || metadata.mode() & 0o077 != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file is not private",
            ));
        }
    }
    #[cfg(windows)]
    if crate::platform::windows::is_reparse(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file is a reparse point",
        ));
    }
    #[cfg(windows)]
    validate_private_windows_acl(&file, path).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("cannot verify privacy of {}: {error}", path.display()),
        )
    })?;
    Ok(file)
}

#[cfg(windows)]
pub(crate) fn validate_private_windows_acl(file: &File, path: &Path) -> Result<()> {
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const INHERITED_ACE_FLAG: u8 = 0x10;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            AclSizeInformation,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
            GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE,
            ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
            SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: file owns a live handle, and all output pointers are live local slots.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        if !descriptor.is_null() {
            // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
            unsafe { LocalFree(descriptor) };
        }
        return Err(std::io::Error::from_raw_os_error(status as i32))
            .context("reading security-sensitive file Windows ACL");
    }
    let result = (|| {
        // SAFETY: GetCurrentProcess returns a valid pseudo-handle that requires no closing.
        let process = unsafe { GetCurrentProcess() };
        let mut token = std::ptr::null_mut();
        // SAFETY: process is the valid pseudo-handle above and token is a writable out-slot.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error()).context("reading current user SID");
        }
        // SAFETY: OpenProcessToken succeeded and returned an owned token handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut needed = 0;
        // SAFETY: A null buffer with length zero requests the required TokenUser buffer size.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return Err(std::io::Error::last_os_error()).context("sizing current user SID");
        }
        let mut token_data = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: token_data is aligned and at least `needed` bytes long; the token remains live.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                token_data.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("reading current user SID");
        }
        // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of the aligned buffer.
        let user = unsafe { &*(token_data.as_ptr().cast::<TOKEN_USER>()) };
        if owner.is_null() || user.User.Sid.is_null() {
            bail!(
                "security-sensitive file {} must be owned by the current user",
                path.display()
            );
        }
        // SAFETY: both SIDs are non-null and remain live in the descriptor and token buffer.
        if unsafe { EqualSid(owner, user.User.Sid) } == 0 {
            bail!(
                "security-sensitive file {} must be owned by the current user",
                path.display()
            );
        }

        let mut present = 0;
        let mut acl_defaulted = 0;
        let mut descriptor_dacl = std::ptr::null_mut();
        // SAFETY: descriptor is the live descriptor returned by GetSecurityInfo.
        if unsafe {
            GetSecurityDescriptorDacl(
                descriptor,
                &mut present,
                &mut descriptor_dacl,
                &mut acl_defaulted,
            )
        } == 0
            || present == 0
            || acl_defaulted != 0
            || descriptor_dacl.is_null()
            || dacl != descriptor_dacl
        {
            bail!(
                "security-sensitive file {} must have an explicit private Windows DACL",
                path.display()
            );
        }
        let (mut control, mut revision) = (0, 0);
        // SAFETY: descriptor is the live security descriptor returned by GetSecurityInfo, and
        // control/revision are writable out-slots for the documented fixed-width values.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
            || control & SE_DACL_PROTECTED == 0
        {
            bail!(
                "security-sensitive file {} must have a protected Windows DACL",
                path.display()
            );
        }
        let mut size = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: dacl is the valid ACL returned in the live security descriptor.
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
            || size.AceCount == 0
        {
            bail!(
                "security-sensitive file {} must have a nonempty private Windows DACL",
                path.display()
            );
        }
        let trusted_sid = |kind| {
            let mut storage = [0usize;
                (windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE as usize)
                    .div_ceil(std::mem::size_of::<usize>())];
            let mut size = windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE;
            // SAFETY: storage is aligned and has SECURITY_MAX_SID_SIZE bytes; CreateWellKnownSid
            // writes at most that size and has no domain SID for these two built-in SIDs.
            if unsafe {
                windows_sys::Win32::Security::CreateWellKnownSid(
                    kind,
                    std::ptr::null_mut(),
                    storage.as_mut_ptr().cast(),
                    &mut size,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("reading trusted Windows principals");
            }
            Ok(storage)
        };
        let system_sid = trusted_sid(windows_sys::Win32::Security::WinLocalSystemSid)?;
        let admins_sid = trusted_sid(windows_sys::Win32::Security::WinBuiltinAdministratorsSid)?;
        for index in 0..size.AceCount {
            let mut raw_ace = std::ptr::null_mut();
            // SAFETY: index is below the ACE count reported for this valid ACL.
            if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 || raw_ace.is_null() {
                return Err(std::io::Error::last_os_error())
                    .context("reading security-sensitive file ACL entry");
            }
            // SAFETY: GetAce returned a pointer to an ACE stored inside the live ACL.
            let header = unsafe { &*(raw_ace.cast::<windows_sys::Win32::Security::ACE_HEADER>()) };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE
                || header.AceFlags & INHERITED_ACE_FLAG != 0
            {
                bail!(
                    "security-sensitive file {} has a nonprivate or inherited Windows ACL entry",
                    path.display()
                );
            }
            // SAFETY: ACCESS_ALLOWED_ACE_TYPE guarantees this ACE layout; SidStart begins its SID.
            let ace = unsafe { &*(raw_ace.cast::<ACCESS_ALLOWED_ACE>()) };
            let ace_sid = std::ptr::addr_of!(ace.SidStart).cast_mut().cast();
            // SAFETY: the ACE SID lies inside the validated ACL and the token SID buffer is live.
            // SYSTEM and local Administrators are trusted platform principals. No other
            // principal may receive an allow ACE, even if a particular ACE currently grants
            // only a narrow subset of rights: future rights changes must not expose the file.
            // SAFETY: all candidate SIDs live in the checked ACL/token/local SID buffers.
            let is_trusted = unsafe {
                EqualSid(ace_sid, user.User.Sid) != 0
                    || EqualSid(ace_sid, system_sid.as_ptr().cast_mut().cast()) != 0
                    || EqualSid(ace_sid, admins_sid.as_ptr().cast_mut().cast()) != 0
            };
            if !is_trusted {
                bail!(
                    "security-sensitive file {} grants access to another Windows principal",
                    path.display()
                );
            }
        }
        Ok(())
    })();
    if !descriptor.is_null() {
        // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
        unsafe { LocalFree(descriptor) };
    }
    result
}

/// Streaming SHA-256 of a bundled helper binary, as lowercase hex. The file
/// is read in bounded chunks so a large helper is never loaded whole.
pub(crate) fn bundled_helper_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .map_err(|error| anyhow!("reading bundled helper {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| anyhow!("reading bundled helper {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The helper hash recorded beside the helper at build/packaging time, in
/// `<helper>.sha256` (for `execute-applet.exe` that is `execute-applet.sha256`).
/// The sidecar holds one whitespace-separated lowercase hex digest, as
/// `sha256sum` writes it. No sidecar means no recorded hash; a present but
/// malformed sidecar fails closed instead of silently dropping the check.
pub(crate) fn recorded_bundled_helper_sha256(path: &Path) -> Result<Option<String>> {
    let sidecar = path.with_extension("sha256");
    let text = match std::fs::read_to_string(&sidecar) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow!(
                "reading bundled helper hash {}: {error}",
                sidecar.display()
            ))
        }
    };
    let digest = text.split_whitespace().next().unwrap_or_default();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "bundled helper hash {} is malformed; refusing to trust {}",
            sidecar.display(),
            path.display()
        ));
    }
    Ok(Some(digest.to_ascii_lowercase()))
}

/// Trust gate for a bundled helper binary (`execute-applet`/`execute-model`)
/// resolved beside the running interpreter. Location alone is not trust: on
/// Unix the helper file and its parent directory must not be group- or
/// world-writable (a writable directory lets another local user replace the
/// verified binary), a symlinked helper is refused, and when a hash was
/// recorded at build time the helper's content must match it exactly. The
/// caller applies the stronger trusted-input gate first (ownership, full
/// ancestor chain, hardlink/setuid, ACLs) and runs the version/identity
/// handshake afterwards, so a replaced helper that does not mimic the
/// version line is refused even in a plain `cargo build` tree with no
/// sidecar. The sidecar ships in the same directory it protects, so a
/// same-directory attacker who can replace the helper can also rewrite the
/// sidecar: the recorded hash binds the packaged artifact against stale or
/// mismatched helpers, not against that attacker. Hash and handshake bytes
/// also cannot be pinned through `exec`; see SECURITY.md.
pub(crate) fn verify_bundled_helper(path: &Path, expected_sha256: Option<&str>) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| anyhow!("reading bundled helper {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "bundled helper {} is a symbolic link; refusing to trust it",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(anyhow!(
            "bundled helper {} is not a regular file",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(anyhow!(
                "bundled helper {} is group/world-writable; refusing to trust it",
                path.display()
            ));
        }
        if let Some(parent) = path.parent() {
            let parent_metadata = std::fs::metadata(parent).map_err(|error| {
                anyhow!(
                    "reading bundled helper directory {}: {error}",
                    parent.display()
                )
            })?;
            if parent_metadata.permissions().mode() & 0o022 != 0 {
                return Err(anyhow!(
                    "bundled helper directory {} is group/world-writable; refusing to trust {}",
                    parent.display(),
                    path.display()
                ));
            }
        }
    }
    if let Some(expected) = expected_sha256 {
        let actual = bundled_helper_sha256(path)?;
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            return Err(anyhow!(
                "bundled helper {} hash mismatch: expected {}, got {actual}; refusing to run an untrusted helper",
                path.display(),
                expected.trim().to_ascii_lowercase()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Registry;

    fn policy(text: &str) -> DataFlowPolicy {
        DataFlowPolicy::from_json(text.as_bytes()).unwrap()
    }

    #[cfg(all(windows, feature = "live"))]
    fn current_user_sid_string() -> String {
        use std::os::windows::ffi::OsStringExt;
        // SAFETY: GetCurrentProcess returns a valid pseudo-handle that needs no closing.
        let process = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
        let sid = crate::platform::windows::user_sid(process).unwrap();
        let mut text = std::ptr::null_mut();
        // SAFETY: sid owns a live current-user SID buffer; text is a writable out-slot,
        // and the API allocates the returned string for LocalFree on success.
        let status = unsafe {
            windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(
                sid.as_ptr().cast_mut().cast(),
                &mut text,
            )
        };
        assert_ne!(status, 0);
        let mut length = 0;
        // SAFETY: ConvertSidToStringSidW returned a NUL-terminated LocalAlloc string.
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: the counted UTF-16 units precede the validated terminator.
        // SAFETY: `text` came from ConvertSidToStringSidW and remains live until LocalFree below;
        // `length` counts only the UTF-16 units before its validated NUL terminator.
        let units = unsafe { std::slice::from_raw_parts(text, length) };
        let value = std::ffi::OsString::from_wide(units)
            .to_string_lossy()
            .into_owned();
        // SAFETY: the string was allocated by ConvertSidToStringSidW with LocalAlloc.
        unsafe { windows_sys::Win32::Foundation::LocalFree(text.cast()) };
        value
    }

    #[cfg(all(windows, feature = "live"))]
    fn replace_test_dacl(path: &std::path::Path, sddl: &str) {
        use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::{SetSecurityInfo, SE_FILE_OBJECT},
                GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION,
                PROTECTED_DACL_SECURITY_INFORMATION,
            },
            Storage::FileSystem::{FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC},
        };

        let file = std::fs::OpenOptions::new()
            .access_mode(WRITE_DAC | READ_CONTROL | FILE_READ_ATTRIBUTES)
            .open(path)
            .unwrap();
        let wide = std::ffi::OsStr::new(sddl)
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: wide is a live NUL-terminated SDDL string, descriptor is a writable
        // out-slot, and the optional size pointer may be null for this API.
        let status = unsafe {
            windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                windows_sys::Win32::Security::Authorization::SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(status, 0);
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = std::ptr::null_mut();
        // SAFETY: descriptor is live from the preceding conversion; present, dacl, and
        // defaulted are writable out-slots, and returned DACL memory is borrowed from it.
        let status = unsafe {
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
        };
        assert_ne!(status, 0);
        assert_ne!(present, 0);
        // SAFETY: file is live, dacl belongs to descriptor, and all outputs passed to the API
        // are valid. SetSecurityInfo reads the DACL before the descriptor is freed below.
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: descriptor is the LocalAlloc allocation returned by the SDDL conversion.
        unsafe { LocalFree(descriptor.cast()) };
        assert_eq!(status, 0);
    }

    #[cfg(all(windows, feature = "live"))]
    #[test]
    fn windows_security_sensitive_loaders_require_private_acl() {
        let directory = tempfile::tempdir().unwrap();
        let insecure = directory.path().join("inherited.json");
        std::fs::write(&insecure, br#"{"version":1}"#).unwrap();
        assert!(open_private_file(&insecure).is_err());
        assert!(crate::capabilities::CapabilityPolicy::load(&insecure).is_err());
        assert!(DataFlowPolicy::load(&insecure).is_err());
        assert!(crate::agent::what_if_key::load_existing_at(&insecure).is_err());
        assert!(crate::live::auth::AgentCredential::from_token_file("agent", &insecure).is_err());

        // Token creation installs the same protected DACL used by the Windows
        // private-file writer. Reuse that ACL while checking every loader path.
        let private = directory.path().join("private");
        let credential = crate::live::auth::AgentCredential::create_token_file("agent", &private)
            .expect("new live token should receive a private DACL");
        assert_eq!(
            crate::live::auth::AgentCredential::from_token_file("agent", &private)
                .expect("private token should load")
                .token_sha256(),
            credential.token_sha256()
        );
        std::fs::write(&private, br#"{"version":1}"#).unwrap();
        assert!(crate::capabilities::CapabilityPolicy::load(&private).is_ok());
        assert!(DataFlowPolicy::load(&private).is_ok());

        let key_path = directory.path().join("what-if.key");
        let created_key = crate::agent::what_if_key::load_or_create_at(&key_path).unwrap();
        assert_eq!(
            crate::agent::what_if_key::load_existing_at(&key_path).unwrap(),
            created_key,
            "a newly-created key must pass privacy validation on reload"
        );
    }

    #[cfg(all(windows, feature = "live"))]
    #[test]
    fn windows_acl_rejects_explicit_outsider_and_accepts_administrators() {
        let directory = tempfile::tempdir().unwrap();
        let user_sid = current_user_sid_string();

        let outsider = directory.path().join("explicit-outsider.json");
        drop(crate::platform::windows::create_private_file(&outsider).unwrap());
        replace_test_dacl(&outsider, &format!("D:P(A;;FA;;;WD)(A;;FA;;;{user_sid})"));
        let error = open_private_file(&outsider).expect_err("Everyone ACE must be refused");
        assert!(
            error.to_string().contains("another Windows principal"),
            "{error}"
        );

        let administrators = directory.path().join("administrators.json");
        drop(crate::platform::windows::create_private_file(&administrators).unwrap());
        replace_test_dacl(
            &administrators,
            &format!("D:P(A;;FA;;;BA)(A;;FA;;;SY)(A;;FA;;;{user_sid})"),
        );
        assert!(
            open_private_file(&administrators).is_ok(),
            "Administrators and SYSTEM ACEs are trusted exceptions"
        );
    }

    #[test]
    fn unknown_is_not_public_and_labels_join_without_declassification() {
        assert_eq!(join_sensitivities([]), Sensitivity::Unknown);
        assert_eq!(
            join_sensitivities([Sensitivity::Public, Sensitivity::Confidential]),
            Sensitivity::Confidential
        );
        assert_eq!(
            join_sensitivities([Sensitivity::Public, Sensitivity::Unknown]),
            Sensitivity::Unknown
        );
    }

    #[test]
    fn remote_egress_requires_exact_origin_and_explicit_label_grant() {
        let registry = Registry::default();
        registry.set_host_state(policy(
            r#"{"version":1,"remote_sinks":{"https://api.example":{"allow":["public"]}}}"#,
        ));
        assert!(authorize_data_flow(
            &registry,
            "remote.lookup",
            "https://api.example/v2/complete?mode=fast",
            Sensitivity::Public
        )
        .is_ok());
        assert!(authorize_data_flow(
            &registry,
            "remote.lookup",
            "https://api.example/",
            Sensitivity::Unknown
        )
        .is_err());
        assert!(authorize_data_flow(
            &registry,
            "remote.lookup",
            "https://other.example/",
            Sensitivity::Public
        )
        .is_err());
        let decisions = policy_decisions(&registry);
        assert_eq!(decisions.len(), 3);
        assert_eq!(decisions[0].outcome, PolicyDecisionOutcome::Approved);
        assert_eq!(
            decisions[1].reason,
            PolicyDecisionReason::UnclassifiedDataDenied
        );
    }

    #[test]
    fn absent_policy_preserves_legacy_persistence_but_never_writes_secret() {
        let registry = Registry::default();
        assert!(authorize_persistence(
            &registry,
            "text.clean",
            PersistenceKind::Cache,
            Sensitivity::Public
        )
        .is_ok());
        assert!(authorize_persistence(
            &registry,
            "credential.read",
            PersistenceKind::Checkpoint,
            Sensitivity::Secret
        )
        .is_err());
        assert!(authorize_persistence(
            &registry,
            "source.input",
            PersistenceKind::Cache,
            Sensitivity::Unknown
        )
        .is_ok());
        assert_eq!(policy_decisions(&registry).len(), 3);
    }

    #[test]
    fn active_policy_denies_unknown_and_nonpublic_persistence_by_default() {
        let registry = Registry::default();
        registry.set_host_state(policy(r#"{"version":1}"#));
        assert!(authorize_persistence(
            &registry,
            "text.normalize",
            PersistenceKind::Cache,
            Sensitivity::Public
        )
        .is_ok());
        assert!(authorize_persistence(
            &registry,
            "source.input",
            PersistenceKind::Cache,
            Sensitivity::Unknown
        )
        .is_err());
        assert!(authorize_persistence(
            &registry,
            "source.input",
            PersistenceKind::Checkpoint,
            Sensitivity::Internal
        )
        .is_err());
    }

    #[test]
    fn absent_flow_policy_preserves_remote_behavior_and_records_the_default() {
        let registry = Registry::default();
        let decision = authorize_data_flow(
            &registry,
            "remote.lookup",
            "https://api.example/v2/complete",
            Sensitivity::Unknown,
        )
        .unwrap();
        assert_eq!(decision.outcome, PolicyDecisionOutcome::Approved);
        assert_eq!(decision.reason, PolicyDecisionReason::NoPolicyConfigured);
        assert_eq!(
            decision.scope_origin.as_deref(),
            Some("https://api.example")
        );
    }

    #[test]
    fn classified_persistence_requires_a_grant_and_policy_cannot_allow_secret() {
        let registry = Registry::default();
        let policy = policy(
            r#"{"version":1,"persistence":{"cache":["internal"],"checkpoint":["confidential"]}}"#,
        );
        registry.set_host_state(policy);
        assert!(authorize_persistence(
            &registry,
            "text.normalize",
            PersistenceKind::Cache,
            Sensitivity::Internal
        )
        .is_ok());
        assert!(authorize_persistence(
            &registry,
            "text.normalize",
            PersistenceKind::Checkpoint,
            Sensitivity::Internal
        )
        .is_err());
        assert!(
            DataFlowPolicy::from_json(br#"{"version":1,"persistence":{"cache":["secret"]}}"#)
                .is_err()
        );
    }

    #[test]
    fn policies_reject_noncanonical_origins_duplicate_keys_and_unknown_fields() {
        for input in [
            r#"{"version":1,"remote_sinks":{"https://user@api.example":{"allow":["public"]}}}"#,
            r#"{"version":1,"remote_sinks":{"https://api.example/path":{"allow":["public"]}}}"#,
            r#"{"version":1,"remote_sinks":{"https://api.example":{"allow":["public"]},"https://api.example":{"allow":["secret"]}}}"#,
            r#"{"version":1,"extra":true}"#,
            r#"{"version":1,"cache_purity":{"trusted_applets":["bad/name"]}}"#,
            r#"{"version":1,"cache_purity":{"trusted_applets":["cut","cut"]}}"#,
        ] {
            assert!(DataFlowPolicy::from_json(input.as_bytes()).is_err());
        }
        for input in [
            r#"{"version":1,"secrets":{"environment":{"bad/name":["TOKEN"]}}}"#,
            r#"{"version":1,"secrets":{"environment":{"text.normalize":["1TOKEN"]}}}"#,
            r#"{"version":1,"secrets":{"environment":{"text.normalize":["TOKEN","TOKEN"]}}}"#,
            r#"{"version":1,"secrets":{"extra":true}}"#,
        ] {
            assert!(DataFlowPolicy::from_json(input.as_bytes()).is_err());
        }
    }

    #[test]
    fn environment_secret_capture_is_applet_scoped_and_uses_only_effective_values() {
        let registry = Registry::default();
        registry.set_host_state(policy(
            r#"{"version":1,"secrets":{"environment":{"remote.call":["API_TOKEN"]}}}"#,
        ));
        assert!(applet_has_declared_secret_environment(
            &registry,
            "remote.call"
        ));
        assert!(!applet_has_declared_secret_environment(
            &registry,
            "other.applet"
        ));
        let environment = vec![
            (
                std::ffi::OsString::from("API_TOKEN"),
                std::ffi::OsString::from("sentinel-token-value"),
            ),
            (
                std::ffi::OsString::from("UNDECLARED"),
                std::ffi::OsString::from("other-secret-value"),
            ),
        ];
        capture_declared_environment_secrets(&registry, "other.applet", &environment).unwrap();
        assert_eq!(
            secret_redactor(&registry).redact_text("sentinel-token-value other-secret-value"),
            "sentinel-token-value other-secret-value"
        );
        capture_declared_environment_secrets(&registry, "remote.call", &environment).unwrap();
        assert_eq!(
            secret_redactor(&registry).redact_text("sentinel-token-value other-secret-value"),
            "[REDACTED] other-secret-value"
        );
        assert!(!format!("{:?}", secret_redactor(&registry)).contains("sentinel-token-value"));
        assert!(!format!("{:?}", registry.host_state::<DataFlowPolicy>())
            .contains("sentinel-token-value"));
    }

    #[test]
    fn explicit_redactor_uses_exact_values_longest_first_and_denies_overflow() {
        let redactor = SecretRedactor::default();
        redactor.register(b"sword").unwrap();
        redactor.register(b"swordfish").unwrap();
        assert_eq!(
            redactor.redact_bytes(b"swordfish sword"),
            b"[REDACTED] [REDACTED]"
        );
        assert!(!format!("{redactor:?}").contains("sword"));

        let bounded = SecretRedactor::default();
        for index in 0..MAX_REDACTION_SECRETS {
            bounded
                .register(format!("known-secret-{index:04}").as_bytes())
                .unwrap();
        }
        assert_eq!(
            bounded.register(b"one-more-secret"),
            Err(PolicyError::RedactionLimitExceeded)
        );
        assert_eq!(
            SecretRedactor::default().register(&vec![b'x'; MAX_REDACTION_SECRET_BYTES_EACH + 1]),
            Err(PolicyError::RedactionLimitExceeded)
        );

        let aggregate = SecretRedactor::default();
        for marker in 0..4u8 {
            let mut value = vec![b'a'; MAX_REDACTION_SECRET_BYTES_EACH];
            value[0] = marker;
            aggregate.register(&value).unwrap();
        }
        let mut over_limit = vec![b'b'; MAX_REDACTION_SECRET_BYTES_EACH];
        over_limit[0] = 9;
        assert_eq!(
            aggregate.register(&over_limit),
            Err(PolicyError::RedactionLimitExceeded)
        );
    }

    fn test_manifest(name: &str, binary: &str, contract: &str) -> Manifest {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("applet.json"),
            format!(
                r#"{{"name":{name:?},"binary":{binary:?},"accepts_stdin":false,"cacheable":true,"parameters":{{}},"arguments":[],"contract":{contract}}}"#
            ),
        )
        .unwrap();
        let registry = Registry::load(temp.path()).unwrap();
        registry.resolve(name).unwrap().clone()
    }

    fn legacy_cacheable_manifest() -> Manifest {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("applet.json"),
            r#"{"name":"cut","binary":"cut","accepts_stdin":true,"cacheable":true,"parameters":{},"arguments":[]}"#,
        )
        .unwrap();
        let registry = Registry::load(temp.path()).unwrap();
        registry.resolve("cut").unwrap().clone()
    }

    #[test]
    fn cache_purity_gate_preserves_legacy_mode_and_rejects_unverified_commands() {
        let legacy = Registry::default();
        let external = legacy_cacheable_manifest();
        assert!(authorize_cache_purity(&legacy, &external).is_ok());
        // Legacy mode stays permissive but records an explicit
        // `NoPolicyConfigured` approval so the audit never goes silent.
        let legacy_decisions = policy_decisions(&legacy);
        assert_eq!(legacy_decisions.len(), 1);
        assert_eq!(
            legacy_decisions[0].reason,
            PolicyDecisionReason::NoPolicyConfigured
        );
        assert_eq!(legacy_decisions[0].outcome, PolicyDecisionOutcome::Approved);

        let registry = Registry::default();
        registry.set_host_state(policy(r#"{"version":1}"#));
        assert_eq!(
            authorize_cache_purity(&registry, &external),
            Err(PolicyError::CachePurityDenied)
        );
        let decisions = policy_decisions(&registry);
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions.last().unwrap().reason,
            PolicyDecisionReason::CachePurityDenied
        );
        assert_eq!(
            decisions.last().unwrap().outcome,
            PolicyDecisionOutcome::Denied
        );
        // Each registry owns its audit log: the denial above must not leak
        // into the legacy registry's log and vice versa.
        assert_eq!(policy_decisions(&legacy).len(), 1);
    }

    #[test]
    fn cache_purity_accepts_only_reviewed_pure_in_process_contracts_by_default() {
        let registry = Registry::default();
        registry.set_host_state(policy(r#"{"version":1}"#));
        let pure = test_manifest(
            "text.concat",
            "@execute-applet",
            r#"{"version":1,"backend":"in_process","capabilities":["text.transform"],"effects":[]}"#,
        );
        assert!(authorize_cache_purity(&registry, &pure).is_ok());

        let unknown_capability = test_manifest(
            "text.concat",
            "@execute-applet",
            r#"{"version":1,"backend":"in_process","capabilities":["future.side_effect"],"effects":[]}"#,
        );
        assert_eq!(
            authorize_cache_purity(&registry, &unknown_capability),
            Err(PolicyError::CachePurityDenied)
        );

        let declared_effect = test_manifest(
            "text.concat",
            "@execute-applet",
            r#"{"version":1,"backend":"in_process","capabilities":["text.transform","clock.read"],"effects":["clock.read"]}"#,
        );
        assert_eq!(
            authorize_cache_purity(&registry, &declared_effect),
            Err(PolicyError::CachePurityDenied)
        );

        let filesystem_read = test_manifest(
            "text.concat",
            "@execute-applet",
            r#"{"version":1,"backend":"in_process","capabilities":["filesystem.read"],"effects":[]}"#,
        );
        assert_eq!(
            authorize_cache_purity(&registry, &filesystem_read),
            Err(PolicyError::CachePurityDenied)
        );
    }

    #[test]
    fn explicit_private_policy_can_trust_a_named_cacheable_applet() {
        let registry = Registry::default();
        registry.set_host_state(policy(
            r#"{"version":1,"cache_purity":{"trusted_applets":["cut"]}}"#,
        ));
        let external = test_manifest("cut", "cut", r#"{"version":1}"#);
        assert!(authorize_cache_purity(&registry, &external).is_ok());
        assert_eq!(
            policy_decisions(&registry).last().unwrap().reason,
            PolicyDecisionReason::CachePurityGranted
        );
        assert_eq!(
            authorize_cache_purity(&registry, &legacy_cacheable_manifest()),
            Err(PolicyError::CachePurityDenied)
        );
        assert_eq!(
            policy_decisions(&registry).last().unwrap().reason,
            PolicyDecisionReason::CachePurityDenied
        );
    }

    #[test]
    fn replaced_helper_with_a_different_hash_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("execute-applet");
        std::fs::write(&helper, b"genuine helper bytes").unwrap();
        let expected = bundled_helper_sha256(&helper).unwrap();
        assert!(verify_bundled_helper(&helper, Some(&expected)).is_ok());
        // Replacing the helper changes its hash and must fail closed.
        std::fs::write(&helper, b"attacker-controlled replacement").unwrap();
        let error = verify_bundled_helper(&helper, Some(&expected)).unwrap_err();
        assert!(error.to_string().contains("hash mismatch"), "{error}");
    }

    #[test]
    fn recorded_helper_hash_sidecar_is_read_and_malformed_sidecar_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("execute-applet");
        std::fs::write(&helper, b"genuine helper bytes").unwrap();
        assert_eq!(recorded_bundled_helper_sha256(&helper).unwrap(), None);
        let expected = bundled_helper_sha256(&helper).unwrap();
        std::fs::write(
            temp.path().join("execute-applet.sha256"),
            format!("{expected}  execute-applet\n"),
        )
        .unwrap();
        assert_eq!(
            recorded_bundled_helper_sha256(&helper).unwrap(),
            Some(expected)
        );
        std::fs::write(temp.path().join("execute-applet.sha256"), "not-a-hash\n").unwrap();
        assert!(recorded_bundled_helper_sha256(&helper).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_helper_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("execute-applet");
        std::fs::write(&helper, b"genuine helper bytes").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_bundled_helper(&helper, None).is_ok());
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = verify_bundled_helper(&helper, None).unwrap_err();
        assert!(
            error.to_string().contains("group/world-writable"),
            "{error}"
        );
        // A group/world-writable helper file is refused for the same reason.
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(verify_bundled_helper(&helper, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn policy_loader_rejects_group_readable_files() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("policy.json");
        std::fs::write(&path, r#"{"version":1}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(DataFlowPolicy::load(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(DataFlowPolicy::load(&path).is_err());
    }

    /// The joint snapshot/total read must stay consistent while other threads
    /// are still recording. Separate reads can observe a push in between and
    /// claim a total past the snapshot's last sequence; the joint read holds
    /// one lock so the last sequence always equals the total and the window
    /// stays contiguous.
    #[test]
    fn decision_snapshot_with_total_stays_consistent_under_concurrency() {
        const WRITERS: usize = 8;
        const PER_WRITER: usize = 200;
        const READERS: usize = 2;
        const READS_PER_READER: usize = 500;

        fn assert_consistent(snapshot: &[PolicyDecision], total: u64) {
            assert!(
                snapshot.len() <= MAX_DECISION_RECORDS,
                "window exceeded its bound: {}",
                snapshot.len()
            );
            assert!(
                snapshot.len() as u64 <= total,
                "window {} runs past total {total}",
                snapshot.len()
            );
            if snapshot.is_empty() {
                assert_eq!(total, 0, "empty window must mean an empty log");
                return;
            }
            assert_eq!(
                snapshot.last().expect("non-empty window").sequence,
                total,
                "joint snapshot/total disagreed under concurrency"
            );
            assert_eq!(
                snapshot.first().expect("non-empty window").sequence,
                total - snapshot.len() as u64 + 1,
                "window prefix was dropped without lifetime accounting"
            );
            for window in snapshot.windows(2) {
                assert_eq!(
                    window[1].sequence,
                    window[0].sequence + 1,
                    "concurrent snapshot left a sequence gap"
                );
            }
        }

        let registry = Registry::default();
        std::thread::scope(|scope| {
            for _ in 0..WRITERS {
                scope.spawn(|| {
                    for _ in 0..PER_WRITER {
                        record_policy_decision(
                            &registry,
                            "audit.concurrent",
                            "effect:text.transform",
                            None,
                            PolicyRisk::Low,
                            PolicyDecisionOutcome::Approved,
                            PolicyDecisionReason::PolicyMatched,
                        );
                    }
                });
            }
            for _ in 0..READERS {
                scope.spawn(|| {
                    for _ in 0..READS_PER_READER {
                        let (snapshot, total) = policy_decisions_with_total(&registry);
                        assert_consistent(&snapshot, total);
                    }
                });
            }
        });

        let expected = (WRITERS * PER_WRITER) as u64;
        let (snapshot, total) = policy_decisions_with_total(&registry);
        assert_eq!(total, expected, "every concurrent push must be counted");
        assert_eq!(
            snapshot.len(),
            std::cmp::min(total as usize, MAX_DECISION_RECORDS),
            "the bounded window must keep the newest decisions"
        );
        assert_consistent(&snapshot, total);
    }

    /// The Windows allow-ACE gate must refuse write-level grants while letting
    /// read-level grants through, on every host: this is pure mask logic with
    /// no platform calls. Regression cover for the `0x8` (`FILE_READ_EA`)
    /// misclassification that refused `C:\Users` — standard read-and-execute
    /// grants carry read-EA, so the ancestor walk failed every manifest load
    /// under a Windows user profile and `check --json` degraded to
    /// `check_failed`.
    #[test]
    fn windows_write_mask_keeps_read_grants_trusted() {
        const FILE_READ_DATA: u32 = 0x1;
        const FILE_READ_EA: u32 = 0x8;
        const FILE_EXECUTE: u32 = 0x20;
        const FILE_READ_ATTRIBUTES: u32 = 0x80;
        const READ_CONTROL: u32 = 0x20000;
        const SYNCHRONIZE: u32 = 0x100000;
        const GENERIC_READ: u32 = 0x80000000;
        const GENERIC_EXECUTE: u32 = 0x20000000;
        // A standard read-and-execute grant, as `C:\Users` carries for
        // `BUILTIN\Users`: read data/EA/attributes, execute, and the generic
        // read-control/synchronize rights. None of it lets another principal
        // replace or remove a trusted input.
        const STANDARD_READ_EXECUTE: u32 = FILE_READ_DATA
            | FILE_READ_EA
            | FILE_EXECUTE
            | FILE_READ_ATTRIBUTES
            | READ_CONTROL
            | SYNCHRONIZE;
        assert!(!windows_allow_ace_grants_write(0));
        assert!(!windows_allow_ace_grants_write(STANDARD_READ_EXECUTE));
        for read in [
            FILE_READ_DATA,
            FILE_READ_EA,
            FILE_EXECUTE,
            FILE_READ_ATTRIBUTES,
            READ_CONTROL,
            SYNCHRONIZE,
            GENERIC_READ,
            GENERIC_EXECUTE,
        ] {
            assert!(
                !windows_allow_ace_grants_write(read),
                "read-level right {read:#x} must stay trusted"
            );
        }
        // Every write-level right the gate documents still fails closed, both
        // alone and combined with a read grant.
        for write in [
            0x2,        // FILE_WRITE_DATA
            0x4,        // FILE_APPEND_DATA
            0x10,       // FILE_WRITE_EA
            0x40,       // FILE_DELETE_CHILD
            0x100,      // FILE_WRITE_ATTRIBUTES
            0x10000,    // DELETE
            0x40000,    // WRITE_DAC
            0x80000,    // WRITE_OWNER
            0x40000000, // GENERIC_WRITE
            0x10000000, // GENERIC_ALL
        ] {
            assert!(
                windows_allow_ace_grants_write(write),
                "write-level right {write:#x} must fail closed"
            );
            assert!(
                windows_allow_ace_grants_write(STANDARD_READ_EXECUTE | write),
                "write-level right {write:#x} must fail closed beside reads"
            );
        }
    }

    /// The ancestor half of the Windows gate must pass create-only grants
    /// while refusing replacement/removal/takeover grants, on every host:
    /// this is pure mask logic with no platform calls. Regression cover for
    /// the factory-install drive root (`C:\`), which grants
    /// `BUILTIN\Users` a create/append right (`0x4`): the full leaf mask
    /// refused the root and `check --json` degraded to `check_failed` for
    /// every manifest load. The leaf keeps the strict mask; only ancestors
    /// narrow it.
    #[test]
    fn windows_ancestor_mask_passes_create_only_grants() {
        const FILE_READ_DATA: u32 = 0x1;
        const FILE_READ_EA: u32 = 0x8;
        const FILE_EXECUTE: u32 = 0x20;
        const FILE_READ_ATTRIBUTES: u32 = 0x80;
        const READ_CONTROL: u32 = 0x20000;
        const SYNCHRONIZE: u32 = 0x100000;
        const STANDARD_READ_EXECUTE: u32 = FILE_READ_DATA
            | FILE_READ_EA
            | FILE_EXECUTE
            | FILE_READ_ATTRIBUTES
            | READ_CONTROL
            | SYNCHRONIZE;
        // The ancestor mask must stay a strict subset of the leaf mask: the
        // leaf refuses everything the ancestor refuses, plus create-only
        // rights.
        assert_eq!(
            TRUSTED_WINDOWS_ANCESTOR_WRITE_MASK & !TRUSTED_WINDOWS_WRITE_MASK,
            0,
            "every ancestor-disqualifying right must also fail the leaf"
        );
        assert_eq!(
            TRUSTED_WINDOWS_WRITE_MASK & !TRUSTED_WINDOWS_ANCESTOR_WRITE_MASK,
            0x2 | 0x4 | 0x10 | 0x100,
            "only the create-only rights may pass on ancestors"
        );
        // Create-only rights pass on ancestors, alone and beside reads: they
        // let a principal create new entries or alter attributes but cannot
        // rename, delete or replace an existing verified entry.
        for create_only in [
            0x2,   // FILE_ADD_FILE
            0x4,   // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
            0x10,  // FILE_WRITE_EA
            0x100, // FILE_WRITE_ATTRIBUTES
        ] {
            assert!(
                !windows_ancestor_mask_is_disqualifying(create_only),
                "create-only right {create_only:#x} must pass on ancestors"
            );
            assert!(
                !windows_ancestor_mask_is_disqualifying(STANDARD_READ_EXECUTE | create_only),
                "create-only right {create_only:#x} must pass on ancestors beside reads"
            );
            // The leaf keeps refusing them: narrowing applies to ancestors only.
            assert!(
                windows_allow_ace_grants_write(create_only),
                "create-only right {create_only:#x} must still fail the leaf"
            );
        }
        // The factory-`C:\` shape passes: a create/append grant beside reads.
        assert!(!windows_ancestor_mask_is_disqualifying(
            STANDARD_READ_EXECUTE | 0x4
        ));
        // Replacement/removal/takeover rights refuse on ancestors, both alone
        // and combined with a read grant.
        for disqualifying in [
            0x40,       // FILE_DELETE_CHILD
            0x10000,    // DELETE
            0x40000,    // WRITE_DAC
            0x80000,    // WRITE_OWNER
            0x40000000, // GENERIC_WRITE
            0x10000000, // GENERIC_ALL
        ] {
            assert!(
                windows_ancestor_mask_is_disqualifying(disqualifying),
                "replacement/removal right {disqualifying:#x} must fail ancestors"
            );
            assert!(
                windows_ancestor_mask_is_disqualifying(STANDARD_READ_EXECUTE | disqualifying),
                "replacement/removal right {disqualifying:#x} must fail ancestors beside reads"
            );
        }
        assert!(!windows_ancestor_mask_is_disqualifying(0));
        assert!(!windows_ancestor_mask_is_disqualifying(
            STANDARD_READ_EXECUTE
        ));
    }

    /// The Windows owner gate must accept the current user, SYSTEM,
    /// Administrators and TrustedInstaller while refusing anyone else, on
    /// every host: this is pure string logic with no platform calls.
    /// Regression cover for the factory-install drive root (`C:\`), which is
    /// owned by `NT SERVICE\TrustedInstaller` rather than the runner: the
    /// ancestor walk refused the root and `check --json` degraded to
    /// `check_failed` for every manifest load.
    #[test]
    fn windows_trusted_owner_sid_accepts_platform_principals() {
        const CURRENT_USER: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
        const OTHER_USER: &str = "S-1-5-21-9999999999-8888888888-7777777777-1002";
        const BUILTIN_USERS: &str = "S-1-5-32-545";
        assert!(windows_owner_sid_is_trusted(CURRENT_USER, CURRENT_USER));
        for trusted in [
            TRUSTED_WINDOWS_SYSTEM_SID,
            TRUSTED_WINDOWS_ADMINISTRATORS_SID,
            TRUSTED_WINDOWS_TRUSTED_INSTALLER_SID,
        ] {
            assert!(
                windows_owner_sid_is_trusted(trusted, CURRENT_USER),
                "{trusted} must stay trusted"
            );
        }
        assert!(
            !windows_owner_sid_is_trusted(OTHER_USER, CURRENT_USER),
            "a random user SID must fail closed"
        );
        assert!(
            !windows_owner_sid_is_trusted(BUILTIN_USERS, CURRENT_USER),
            "the Users-group SID must fail closed"
        );
        assert!(
            !windows_owner_sid_is_trusted("", CURRENT_USER),
            "an empty owner SID must fail closed"
        );
        assert!(
            !windows_owner_sid_is_trusted(CURRENT_USER, ""),
            "an empty current-user SID must not self-trust"
        );
    }
}
