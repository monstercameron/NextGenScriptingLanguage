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
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use url::Url;

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

const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_ORIGINS: usize = 256;
const MAX_TRUSTED_CACHE_APPLETS: usize = 1024;
const MAX_DECISION_RECORDS: usize = 1024;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceKind {
    Cache,
    Checkpoint,
    Save,
}

impl PersistenceKind {
    fn action(self) -> &'static str {
        match self {
            Self::Cache => "cache.persist",
            Self::Checkpoint => "checkpoint.persist",
            Self::Save => "filesystem.persist",
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
#[derive(Clone, Default, PartialEq, Eq)]
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

impl DataFlowPolicy {
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
            || document.persistence.0.len() > 3
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRisk {
    Low,
    Elevated,
    High,
}

/// Whether a policy-approved action was allowed to proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionOutcome {
    Approved,
    Denied,
}

/// A bounded reason code for an approval or denial record. These are enums so
/// callers cannot accidentally attach payload data, credentials, or paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
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
        decision.sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        if records.len() == MAX_DECISION_RECORDS {
            records.pop_front();
        }
        records.push_back(decision.clone());
        decision
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

fn canonical_origin(value: &str) -> Result<String, ()> {
    let url = Url::parse(value).map_err(|_| ())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(());
    }
    Ok(url.origin().ascii_serialization())
}

fn canonical_policy_origin(value: &str) -> Result<String, ()> {
    let url = Url::parse(value).map_err(|_| ())?;
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(());
    }
    canonical_origin(value)
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

fn open_private_file(path: &Path) -> std::io::Result<File> {
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
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
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
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Registry;

    fn policy(text: &str) -> DataFlowPolicy {
        DataFlowPolicy::from_json(text.as_bytes()).unwrap()
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
        assert!(policy_decisions(&legacy).is_empty());

        let registry = Registry::default();
        registry.set_host_state(policy(r#"{"version":1}"#));
        assert_eq!(
            authorize_cache_purity(&registry, &external),
            Err(PolicyError::CachePurityDenied)
        );
        assert_eq!(
            policy_decisions(&registry).last().unwrap().reason,
            PolicyDecisionReason::CachePurityDenied
        );
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
}
