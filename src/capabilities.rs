//! Opt-in applet grants and whole-program capability preflight.
//!
//! Without a configured policy Execute retains its historical trusted-process
//! behavior. Once a policy is installed, every invoked applet must have an
//! explicit canonical-name grant and a supported version-1 manifest contract.
//! These declaration checks do not isolate a process. OS isolation is applied
//! only to grants with `sandbox_required: true` by `platform::sandbox`.

use crate::ast::{Program, Step, StepKind, Trigger, Value};
use crate::contracts::AppletBackend;
use crate::manifest::Registry;
use crate::security::{
    record_policy_decision, PolicyDecisionOutcome, PolicyDecisionReason, PolicyRisk,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::Command;
use url::Url;

pub const CAPABILITY_POLICY_VERSION: u8 = 1;
const MAX_CAPABILITY_POLICY_BYTES: usize = 64 * 1024;

/// Strict opt-in allowlist. A missing policy is represented by
/// `Registry::host_state::<Option<CapabilityPolicy>>() == None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityPolicy {
    pub version: u8,
    #[serde(default)]
    pub applets: BTreeMap<String, AppletGrant>,
    /// Refuse privileged steps before any effect: every privilege elevator, and every
    /// wrapper or shell that is not named in `allow_wrappers`. See `privilege`.
    #[serde(default)]
    pub deny_privileged: bool,
    /// Wrapper and shell names (for example `env`) that may run under
    /// `deny_privileged`. Elevators cannot be allowed.
    #[serde(default)]
    pub allow_wrappers: BTreeSet<String>,
}

/// Refuse a command launch that the policy's `deny_privileged` forbids. This runs in
/// `command_for`, before any `Command` is built, so every spawn path that builds its
/// process through it is covered. The executable is classified by each manifest binary
/// name in `binary_names` and by the path it would actually run, both as written and
/// canonicalized, so a link to an elevator is still an elevator. Fails closed: a name
/// that cannot be resolved is still classified by its name.
pub fn check_privileged_launch(
    registry: &Registry,
    binary_names: &[&str],
    resolved: &Path,
) -> Result<()> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        return Ok(());
    };
    if !policy.deny_privileged {
        return Ok(());
    }
    let shown = binary_names
        .first()
        .map(|name| (*name).to_owned())
        .unwrap_or_else(|| resolved.display().to_string());
    let mut names: Vec<String> = binary_names.iter().map(|name| (*name).to_owned()).collect();
    names.push(resolved.display().to_string());
    if let Ok(canonical) = std::fs::canonicalize(resolved) {
        names.push(canonical.display().to_string());
    }
    let binary = shown.as_str();
    for name in &names {
        match crate::privilege::classify_names([name.as_str()]) {
            crate::privilege::Privilege::Elevator => {
                record_policy_decision(
                    registry,
                    binary,
                    "privilege.elevate",
                    None,
                    PolicyRisk::High,
                    PolicyDecisionOutcome::Denied,
                    PolicyDecisionReason::MissingGrant,
                );
                bail!(
                    "capability policy denies privileged steps: {binary:?} runs through the privilege elevator {name:?}"
                );
            }
            crate::privilege::Privilege::Wrapper => {
                let allowed = crate::privilege::normalized_name(name)
                    .is_some_and(|normalized| policy.allow_wrappers.contains(&normalized));
                if !allowed {
                    record_policy_decision(
                        registry,
                        binary,
                        "privilege.wrapper",
                        None,
                        PolicyRisk::High,
                        PolicyDecisionOutcome::Denied,
                        PolicyDecisionReason::MissingGrant,
                    );
                    bail!(
                        "capability policy denies privileged steps: {binary:?} is a command launcher that can run another program ({name:?}); list it in allow_wrappers to permit it"
                    );
                }
            }
            crate::privilege::Privilege::Standard => {}
        }
    }
    Ok(())
}

/// Per-applet permissions. Child filesystem/network access is OS-enforced only
/// when `sandbox_required` is true. Path grants also gate host-managed file
/// operations and remote-origin grants are checked by remote dispatch.
/// Environment filtering is applied whenever a policy is active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppletGrant {
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    #[serde(default)]
    pub binary_sha256: Option<String>,
    #[serde(default)]
    pub environment: BTreeSet<String>,
    #[serde(default)]
    pub filesystem: FilesystemGrant,
    #[serde(default)]
    pub network: bool,
    /// Exact canonical HTTP(S) origins for host-managed remote applets.
    /// An empty set with `network: true` is an explicit broad network grant.
    #[serde(default)]
    pub network_origins: BTreeSet<String>,
    #[serde(default)]
    pub sandbox_required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemGrant {
    #[serde(default)]
    pub read: Vec<PathBuf>,
    #[serde(default)]
    pub write: Vec<PathBuf>,
}

impl CapabilityPolicy {
    /// Read and validate a policy file with a fixed size limit.
    pub fn load(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)
            .with_context(|| format!("reading capability policy {}", path.display()))?
            .take((MAX_CAPABILITY_POLICY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .context("reading capability policy")?;
        if bytes.len() > MAX_CAPABILITY_POLICY_BYTES {
            bail!("capability policy exceeds the size limit");
        }
        let source = std::str::from_utf8(&bytes).context("capability policy is not UTF-8")?;
        Self::from_json(source)
            .with_context(|| format!("loading capability policy {}", path.display()))
    }

    /// Parse a strict versioned JSON policy. Unknown fields and settings fail
    /// closed through `deny_unknown_fields` and validation below.
    pub fn from_json(source: &str) -> Result<Self> {
        if source.len() > MAX_CAPABILITY_POLICY_BYTES {
            bail!("capability policy exceeds the size limit");
        }
        let mut deserializer = serde_json::Deserializer::from_str(source);
        let value = StrictPolicyValue::deserialize(&mut deserializer)
            .context("parsing capability policy")?;
        deserializer.end().context("parsing capability policy")?;
        let policy: Self =
            serde_json::from_value(value.into_json()).context("decoding capability policy")?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != CAPABILITY_POLICY_VERSION {
            bail!(
                "unsupported capability policy version {}; expected {}",
                self.version,
                CAPABILITY_POLICY_VERSION
            );
        }
        for (name, grant) in &self.applets {
            if !validate_applet_name(name) {
                bail!("invalid canonical applet name in capability policy");
            }
            if let Some(digest) = &grant.binary_sha256 {
                validate_digest(digest)
                    .with_context(|| format!("applet {name:?} binary_sha256"))?;
            }
            for capability in &grant.capabilities {
                validate_capability_id(capability)
                    .with_context(|| format!("applet {name:?} capability"))?;
            }
            for variable in &grant.environment {
                validate_environment_name(variable)
                    .with_context(|| format!("applet {name:?} environment allowlist"))?;
            }
            if grant.network && !grant.capabilities.contains("network.connect") {
                bail!("applet {name:?} grants network access without network.connect");
            }
            if !grant.network && grant.capabilities.contains("network.connect") {
                bail!("applet {name:?} declares network.connect without a network grant");
            }
            if !grant.network_origins.is_empty() && !grant.network {
                bail!("applet {name:?} grants network origins without network access");
            }
            for origin in &grant.network_origins {
                validate_network_origin(origin)
                    .with_context(|| format!("applet {name:?} network origin"))?;
            }
            if !grant.filesystem.read.is_empty() && !grant.capabilities.contains("filesystem.read")
            {
                bail!("applet {name:?} grants filesystem reads without filesystem.read");
            }
            if !grant.filesystem.write.is_empty()
                && !grant.capabilities.contains("filesystem.write")
            {
                bail!("applet {name:?} grants filesystem writes without filesystem.write");
            }
            for path in grant
                .filesystem
                .read
                .iter()
                .chain(grant.filesystem.write.iter())
            {
                validate_policy_path(path)
                    .with_context(|| format!("applet {name:?} filesystem grant"))?;
            }
        }
        for name in &self.allow_wrappers {
            // Only a listed wrapper or shell can be allowed, by its plain name.
            let plain = crate::privilege::normalized_name(name).as_deref() == Some(name.as_str());
            if !plain || crate::privilege::classify(name) != crate::privilege::Privilege::Wrapper {
                bail!(
                    "allow_wrappers names {name:?}, which is not a known command launcher; elevators cannot be allowed"
                );
            }
        }
        Ok(())
    }

    /// Compare one applet's declared contract with the policy and verify its
    /// executable digest when the grant pins one.
    pub fn authorize(
        &self,
        name: &str,
        direct: bool,
        binary: &Path,
        contract: Option<ContractView<'_>>,
    ) -> Result<()> {
        self.validate()?;
        self.authorize_validated(name, direct, binary, contract, None)
    }

    fn authorize_validated(
        &self,
        name: &str,
        direct: bool,
        binary: &Path,
        contract: Option<ContractView<'_>>,
        registry: Option<&Registry>,
    ) -> Result<()> {
        let result = self.authorize_inner(name, direct, binary, contract);
        if let Some(registry) = registry {
            record_contract_authorization(self, registry, name, direct, binary, contract, &result);
        }
        result
    }

    fn authorize_inner(
        &self,
        name: &str,
        direct: bool,
        binary: &Path,
        contract: Option<ContractView<'_>>,
    ) -> Result<()> {
        if direct {
            bail!("capability policy denies direct quoted commands");
        }
        let grant = self.applets.get(name).ok_or_else(|| {
            anyhow!("capability policy has no allowlist entry for applet {name:?}")
        })?;
        let contract = contract.ok_or_else(|| {
            anyhow!("capability policy requires a version-1 contract for applet {name:?}")
        })?;
        if contract.version != 1 {
            bail!(
                "applet {name:?} has unsupported contract version {}",
                contract.version
            );
        }
        if let Some(capability) = contract
            .capabilities
            .iter()
            .find(|capability| !grant.capabilities.contains(*capability))
        {
            bail!("capability policy does not grant {capability:?} to applet {name:?}");
        }
        if let Some(effect) = contract
            .effects
            .iter()
            .find(|effect| !contract.capabilities.contains(*effect))
        {
            bail!("applet {name:?} declares effect {effect:?} without the matching capability");
        }
        if let Some(expected) = &grant.binary_sha256 {
            let actual = sha256_file(binary)
                .with_context(|| format!("hashing executable for applet {name:?}"))?;
            if !actual.eq_ignore_ascii_case(expected) {
                bail!("capability policy binary hash mismatch for applet {name:?}");
            }
        }
        Ok(())
    }

    /// Recheck a pinned binary immediately before spawn. This narrows, but
    /// cannot eliminate, replacement races between hashing and `exec`.
    pub fn verify_binary(&self, name: &str, binary: &Path) -> Result<()> {
        let Some(expected) = self
            .applets
            .get(name)
            .and_then(|grant| grant.binary_sha256.as_deref())
        else {
            return Ok(());
        };
        let actual = sha256_file(binary)
            .with_context(|| format!("hashing executable for applet {name:?}"))?;
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("capability policy binary hash mismatch for applet {name:?}");
        }
        Ok(())
    }
}

fn record_contract_authorization(
    policy: &CapabilityPolicy,
    registry: &Registry,
    name: &str,
    direct: bool,
    binary: &Path,
    contract: Option<ContractView<'_>>,
    result: &Result<()>,
) {
    let (action, risk, reason) = if direct {
        (
            "command.execute".to_owned(),
            PolicyRisk::High,
            PolicyDecisionReason::MissingGrant,
        )
    } else if let Some(grant) = policy.applets.get(name) {
        match contract {
            None => (
                "applet.invoke".to_owned(),
                PolicyRisk::Elevated,
                PolicyDecisionReason::MissingGrant,
            ),
            Some(contract) if contract.version != 1 => (
                "applet.invoke".to_owned(),
                PolicyRisk::Elevated,
                PolicyDecisionReason::MissingGrant,
            ),
            Some(contract) => {
                if let Some(capability) = contract
                    .capabilities
                    .iter()
                    .find(|capability| !grant.capabilities.contains(*capability))
                {
                    (
                        format!("capability:{capability}"),
                        capability_risk(capability),
                        PolicyDecisionReason::CapabilityNotGranted,
                    )
                } else if let Some(effect) = contract
                    .effects
                    .iter()
                    .find(|effect| !contract.capabilities.contains(*effect))
                {
                    (
                        format!("effect:{effect}"),
                        capability_risk(effect),
                        PolicyDecisionReason::EffectNotGranted,
                    )
                } else if let Some(expected) = &grant.binary_sha256 {
                    match sha256_file(binary) {
                        Ok(actual) if actual.eq_ignore_ascii_case(expected) => (
                            "applet.invoke".to_owned(),
                            PolicyRisk::Low,
                            PolicyDecisionReason::PolicyMatched,
                        ),
                        _ => (
                            "binary.identity".to_owned(),
                            PolicyRisk::High,
                            PolicyDecisionReason::BinaryIdentityMismatch,
                        ),
                    }
                } else {
                    (
                        "applet.invoke".to_owned(),
                        PolicyRisk::Low,
                        PolicyDecisionReason::PolicyMatched,
                    )
                }
            }
        }
    } else {
        (
            "applet.invoke".to_owned(),
            PolicyRisk::Elevated,
            PolicyDecisionReason::MissingGrant,
        )
    };

    if result.is_err() {
        record_policy_decision(
            registry,
            name,
            &action,
            None,
            risk,
            PolicyDecisionOutcome::Denied,
            reason,
        );
        return;
    }

    let Some(grant) = policy.applets.get(name) else {
        return;
    };
    let Some(contract) = contract else {
        return;
    };
    if contract.capabilities.is_empty() && contract.effects.is_empty() {
        record_policy_decision(
            registry,
            name,
            "applet.invoke",
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::PolicyMatched,
        );
        return;
    }
    for capability in contract.capabilities {
        if grant.capabilities.contains(capability) {
            record_policy_decision(
                registry,
                name,
                &format!("capability:{capability}"),
                None,
                capability_risk(capability),
                PolicyDecisionOutcome::Approved,
                PolicyDecisionReason::PolicyMatched,
            );
        }
    }
    for effect in contract.effects {
        record_policy_decision(
            registry,
            name,
            &format!("effect:{effect}"),
            None,
            capability_risk(effect),
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::PolicyMatched,
        );
    }
}

fn capability_risk(capability: &str) -> PolicyRisk {
    match capability {
        "network.connect" | "process.exec" | "filesystem.write" | "filesystem.lock" => {
            PolicyRisk::High
        }
        "filesystem.read" => PolicyRisk::Elevated,
        _ => PolicyRisk::Low,
    }
}

fn authorize_backend_capability(
    registry: &Registry,
    applet_name: &str,
    grant: &AppletGrant,
    capability: &str,
) -> Result<()> {
    let allowed = grant.capabilities.contains(capability);
    record_policy_decision(
        registry,
        applet_name,
        capability,
        None,
        PolicyRisk::High,
        if allowed {
            PolicyDecisionOutcome::Approved
        } else {
            PolicyDecisionOutcome::Denied
        },
        if allowed {
            PolicyDecisionReason::PolicyMatched
        } else {
            PolicyDecisionReason::CapabilityNotGranted
        },
    );
    if allowed {
        Ok(())
    } else {
        bail!(
            "capability policy does not grant backend requirement {capability:?} to applet {applet_name:?}"
        );
    }
}

/// Preserve JSON object uniqueness while decoding security configuration.
/// `serde_json::Value` alone overwrites duplicate map keys, which would make
/// a policy's meaning depend on parser duplicate-key behavior.
enum StrictPolicyValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

impl StrictPolicyValue {
    fn into_json(self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(value) => serde_json::Value::Bool(value),
            Self::Number(value) => serde_json::Value::Number(value),
            Self::String(value) => serde_json::Value::String(value),
            Self::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(Self::into_json).collect())
            }
            Self::Object(values) => serde_json::Value::Object(
                values
                    .into_iter()
                    .map(|(name, value)| (name, value.into_json()))
                    .collect(),
            ),
        }
    }
}

impl<'de> Deserialize<'de> for StrictPolicyValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictVisitor;

        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictPolicyValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value with unique object keys")
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictPolicyValue::Null)
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictPolicyValue::Null)
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(StrictPolicyValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictPolicyValue::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictPolicyValue::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(StrictPolicyValue::Number)
                    .ok_or_else(|| E::custom("non-finite numbers are not valid JSON"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(StrictPolicyValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
                Ok(StrictPolicyValue::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(StrictPolicyValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((name, value)) = map.next_entry::<String, StrictPolicyValue>()? {
                    if values.insert(name.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(
                            "duplicate object key in capability policy",
                        ));
                    }
                }
                Ok(StrictPolicyValue::Object(values))
            }
        }

        deserializer.deserialize_any(StrictVisitor)
    }
}

/// A small view of the manifest contract used by policy authorization.
#[derive(Debug, Clone, Copy)]
pub struct ContractView<'a> {
    pub version: u32,
    pub capabilities: &'a [String],
    pub effects: &'a [String],
}

/// SHA-256 of the exact executable file contents, in lowercase hexadecimal.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).context("opening executable for identity check")?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .context("reading executable for identity check")?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Validate every applet reachable from program source before any workflow
/// effect. The scan includes all reusable definitions and listen triggers so
/// a later edit/branch cannot expose a command that escaped the initial check.
pub fn preflight(program: &Program, registry: &Registry, cwd: &Path) -> Result<()> {
    preflight_with_cache_directory(program, registry, cwd, None)
}

/// Like [`preflight`], with the cache directory that the runtime will use.
/// Cache reads and writes are host-process file operations, so strict policy
/// checks both permissions before any earlier workflow step can have effects.
/// `None` selects the runtime default, `cwd/.flow-cache`.
pub fn preflight_with_cache_directory(
    program: &Program,
    registry: &Registry,
    cwd: &Path,
    cache_directory: Option<&Path>,
) -> Result<()> {
    let dynamic_cwd = contains_cwd_change(program, registry)?;
    let mut calls = Vec::new();
    if let Some(Trigger::Listen {
        source: crate::ast::ListenSource::Applet(call),
        ..
    }) = &program.trigger
    {
        calls.push(call);
    }
    collect_invocations(&program.steps, &mut calls);
    for definition in program.definitions.values() {
        collect_invocations(&definition.steps, &mut calls);
    }

    // Environment overlays are applied to child processes only. Do not
    // silently ignore them on backends that execute inside this process or
    // communicate with a remote service.
    for call in &calls {
        let manifest = registry.resolve_call(call)?;
        validate_environment_backend(manifest)?;
    }

    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        return Ok(());
    };
    policy.validate()?;

    if dynamic_cwd
        && calls.iter().any(|call| call.cache)
        && !cache_directory.is_some_and(Path::is_absolute)
    {
        bail!(
            "capability policy requires an absolute cache directory when the program changes working directories"
        );
    }

    for name in policy.applets.keys() {
        let manifest = registry
            .resolve(name)
            .with_context(|| format!("capability policy names unknown applet {name:?}"))?;
        if manifest.name != *name {
            bail!("capability policy applet names must use canonical manifest names");
        }
        let contract = manifest.execution_contract().ok_or_else(|| {
            anyhow!("capability policy grant for applet {name:?} has no version-1 contract")
        })?;
        if contract.version != 1 {
            bail!(
                "applet {name:?} has unsupported contract version {}",
                contract.version
            );
        }
    }

    preflight_invocations_with_policy(calls.iter().copied(), registry, cwd, dynamic_cwd, policy)?;

    preflight_host_paths(program, registry, policy, cwd, dynamic_cwd)?;
    preflight_cache_paths(&calls, registry, cwd, cache_directory)
}

fn validate_environment_backend(manifest: &crate::manifest::Manifest) -> Result<()> {
    let Some(environment) = manifest.environment() else {
        return Ok(());
    };
    if environment.set.is_empty() && environment.unset.is_empty() {
        return Ok(());
    }
    if !matches!(
        manifest.execution_backend(),
        AppletBackend::Command | AppletBackend::JsonLinesWorker
    ) {
        bail!(
            "applet environment overlays are supported only for command and json_lines_worker backends (applet {:?})",
            manifest.name
        );
    }
    Ok(())
}

/// Preflight a graph or other execution plan that owns an already extracted
/// list of applet calls. Direct commands are denied and every call must resolve
/// through the same strict registry grants used by `preflight`.
pub fn preflight_invocations<'a>(
    calls: impl IntoIterator<Item = &'a crate::ast::Invocation>,
    registry: &Registry,
    cwd: &Path,
) -> Result<()> {
    let calls = calls.into_iter().collect::<Vec<_>>();
    for call in &calls {
        validate_environment_backend(registry.resolve_call(call)?)?;
    }
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        return Ok(());
    };
    policy.validate()?;
    validate_policy_names(policy, registry)?;
    preflight_invocations_with_policy(calls.iter().copied(), registry, cwd, false, policy)?;
    preflight_cache_paths(&calls, registry, cwd, None)
}

fn preflight_cache_paths(
    calls: &[&crate::ast::Invocation],
    registry: &Registry,
    cwd: &Path,
    cache_directory: Option<&Path>,
) -> Result<()> {
    if !calls.iter().any(|call| call.cache) {
        return Ok(());
    }
    let directory = match cache_directory {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => cwd.join(path),
        None => cwd.join(".flow-cache"),
    };
    for call in calls.iter().filter(|call| call.cache) {
        let manifest = registry.resolve_call(call)?;
        if call.direct {
            bail!("capability policy denies direct quoted commands");
        }
        // A cache entry is opened for reading and may be atomically replaced
        // after the applet runs. Keep that authority applet-scoped so another
        // applet's path grant cannot authorize this cache operation.
        authorize_applet_host_path(registry, &manifest.name, &directory, false, cwd)?;
        authorize_applet_host_path(registry, &manifest.name, &directory, true, cwd)?;
    }
    Ok(())
}

fn validate_policy_names(policy: &CapabilityPolicy, registry: &Registry) -> Result<()> {
    for name in policy.applets.keys() {
        let manifest = registry
            .resolve(name)
            .with_context(|| format!("capability policy names unknown applet {name:?}"))?;
        if manifest.name != *name {
            bail!("capability policy applet names must use canonical manifest names");
        }
        let contract = manifest.execution_contract().ok_or_else(|| {
            anyhow!("capability policy grant for applet {name:?} has no version-1 contract")
        })?;
        if contract.version != 1 {
            bail!(
                "applet {name:?} has unsupported contract version {}",
                contract.version
            );
        }
    }
    Ok(())
}

fn preflight_invocations_with_policy<'a>(
    calls: impl IntoIterator<Item = &'a crate::ast::Invocation>,
    registry: &Registry,
    cwd: &Path,
    dynamic_cwd: bool,
    policy: &CapabilityPolicy,
) -> Result<()> {
    // Privilege is checked for every call before any grant is, so a privileged step is
    // reported as such wherever it sits in the program.
    let calls: Vec<&crate::ast::Invocation> = calls.into_iter().collect();
    for call in &calls {
        let manifest = registry.resolve_call(call)?;
        let binary = manifest.binary_for(call);
        let resolved =
            crate::runtime::binary_path(binary, cwd).unwrap_or_else(|_| PathBuf::from(binary));
        check_privileged_launch(registry, &[binary], &resolved)?;
    }
    for call in calls {
        if call.direct {
            record_policy_decision(
                registry,
                "direct.command",
                "command.execute",
                None,
                PolicyRisk::High,
                PolicyDecisionOutcome::Denied,
                PolicyDecisionReason::MissingGrant,
            );
            bail!("capability policy denies direct quoted commands");
        }
        let manifest = registry.resolve_call(call)?;
        if manifest.builtin == Some(crate::manifest::Builtin::Cd) {
            let value = call.parameters.get("path").ok_or_else(|| {
                anyhow!("capability policy requires a literal path for the cd builtin")
            })?;
            let Value::String(path) = value else {
                bail!(
                    "capability policy requires literal host filesystem paths that can be checked before effects"
                );
            };
            if dynamic_cwd && !Path::new(path).is_absolute() {
                bail!(
                    "capability policy cannot preflight a relative host path after a working-directory change"
                );
            }
            authorize_host_path(registry, Path::new(path), false, cwd)?;
            continue;
        }
        let name = &manifest.name;
        let Some(grant) = policy.applets.get(name) else {
            record_policy_decision(
                registry,
                name,
                "applet.invoke",
                None,
                PolicyRisk::Elevated,
                PolicyDecisionOutcome::Denied,
                PolicyDecisionReason::MissingGrant,
            );
            bail!("capability policy has no allowlist entry for applet {name:?}");
        };
        let contract = manifest.execution_contract().ok_or_else(|| {
            anyhow!("capability policy requires a version-1 contract for applet {name:?}")
        })?;
        if let Some(environment) = &contract.environment {
            for variable in environment.set.keys() {
                authorize_environment_override(registry, name, variable)?;
            }
        }
        let backend = manifest.execution_backend();
        match backend {
            AppletBackend::InProcess => {
                if grant.sandbox_required {
                    bail!("required OS sandbox cannot isolate in-process applet {name:?}");
                }
                if grant.binary_sha256.is_some() {
                    bail!("binary_sha256 cannot pin an in-process applet implementation");
                }
                if !is_policy_safe_in_process(name) || !contract.effects.is_empty() {
                    bail!(
                        "capability policy denies effectful or unreviewed in-process applet {name:?}"
                    );
                }
            }
            AppletBackend::ProgramModule => {
                bail!(
                    "capability policy cannot isolate or fully preflight program module applet {name:?}"
                );
            }
            AppletBackend::Remote => {
                if grant.sandbox_required {
                    bail!("required OS sandbox cannot isolate remote applet {name:?}");
                }
                if grant.binary_sha256.is_some() {
                    bail!("binary_sha256 cannot pin a remote applet implementation");
                }
                authorize_backend_capability(registry, name, grant, "network.connect")?;
            }
            AppletBackend::Command | AppletBackend::JsonLinesWorker => {
                authorize_backend_capability(registry, name, grant, "process.exec")?;
            }
        }
        if grant.sandbox_required && !grant.network_origins.is_empty() {
            bail!(
                "required OS sandbox cannot enforce network origin restrictions for applet {name:?}"
            );
        }
        if backend != AppletBackend::Remote && !grant.network_origins.is_empty() {
            bail!("network_origins is supported only for host-managed remote applets");
        }
        if backend == AppletBackend::Remote {
            let remote = contract
                .remote
                .as_ref()
                .ok_or_else(|| anyhow!("remote applet {name:?} has no endpoint configuration"))?;
            authorize_network_endpoint(registry, name, &remote.endpoint)?;
            if let Some(auth) = &remote.auth {
                match auth {
                    crate::contracts::RemoteAuthSource::BearerEnv { name: variable } => {
                        authorize_environment_variable(registry, name, variable)?;
                    }
                    crate::contracts::RemoteAuthSource::BearerFile { path } => {
                        if !path.is_absolute() {
                            bail!("strict policy requires remote bearer-file paths to be absolute");
                        }
                        authorize_applet_host_path(registry, name, path, false, cwd)?;
                    }
                }
            }
        }
        let binary = if backend != AppletBackend::InProcess
            && (grant.binary_sha256.is_some() || grant.sandbox_required)
        {
            let requested = manifest.binary_for(call);
            if dynamic_cwd && !requested.starts_with('@') && !Path::new(requested).is_absolute() {
                bail!(
                    "capability policy cannot resolve a relative or PATH executable after a dynamic working-directory change; use an absolute executable path"
                );
            }
            let resolved = crate::runtime::binary_path(requested, cwd)
                .with_context(|| format!("resolving executable for applet {name:?}"))?;
            Some(resolved)
        } else {
            None
        };
        policy.authorize_validated(
            name,
            false,
            binary.as_deref().unwrap_or_else(|| Path::new("")),
            Some(ContractView {
                version: contract.version,
                capabilities: &contract.capabilities,
                effects: &contract.effects,
            }),
            Some(registry),
        )?;
        if grant.sandbox_required {
            if dynamic_cwd {
                bail!(
                    "required sandbox cannot preflight a program with dynamic working-directory changes"
                );
            }
            let binary = binary
                .as_deref()
                .expect("sandbox preflight resolved binary");
            crate::platform::sandbox::preflight(grant, binary, cwd)
                .with_context(|| format!("preflighting required sandbox for applet {name:?}"))?;
        }
    }
    Ok(())
}

/// Build the child command, applying policy environment filtering and OS
/// sandboxing. Call this at every launch site, including persistent workers.
pub fn command_for<I, S>(
    binary: &Path,
    arguments: I,
    registry: &Registry,
    applet_name: &str,
    cwd: &crate::working_dir::WorkingDirectory,
) -> Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // The one check every spawn path reaches before a Command exists. It classifies the
    // manifest binary name, when the applet is registered, and the resolved path.
    let manifest_binary = registry
        .resolve(applet_name)
        .ok()
        .map(|manifest| manifest.binary.clone());
    let names: Vec<&str> = manifest_binary.as_deref().into_iter().collect();
    check_privileged_launch(registry, &names, binary)?;
    let environment = effective_environment(registry, applet_name)?;
    let environment = apply_command_overlay(registry, applet_name, environment)?;
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let manifest = match registry.resolve(applet_name) {
        Ok(manifest) => manifest,
        Err(_) if configured.as_ref().is_none() => {
            // Direct quoted commands use a synthetic manifest that is not in
            // the registry. Keep their legacy launch path when no strict
            // capability policy is active.
            crate::security::capture_declared_environment_secrets(
                registry,
                applet_name,
                &environment,
            )
            .map_err(|_| anyhow!("could not register declared child environment secrets"))?;
            let mut command = Command::new(binary);
            command.args(arguments).env_clear().envs(environment);
            cwd.apply(&mut command);
            return Ok(command);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolving applet {applet_name:?} for launch"));
        }
    };
    let backend = manifest.execution_backend();
    if !matches!(
        backend,
        AppletBackend::Command | AppletBackend::JsonLinesWorker
    ) {
        bail!("command launch requested for non-process applet {applet_name:?}");
    }
    validate_environment_backend(manifest)?;
    let Some(policy) = configured.as_ref() else {
        crate::security::capture_declared_environment_secrets(registry, applet_name, &environment)
            .map_err(|_| anyhow!("could not register declared child environment secrets"))?;
        let mut command = Command::new(binary);
        command.args(arguments).env_clear().envs(environment);
        cwd.apply(&mut command);
        return Ok(command);
    };
    policy.validate()?;
    let grant = policy.applets.get(applet_name).ok_or_else(|| {
        anyhow!("capability policy has no allowlist entry for applet {applet_name:?}")
    })?;
    authorize_backend_capability(registry, applet_name, grant, "process.exec")?;
    if manifest.name != applet_name {
        bail!("capability policy applet names must use canonical manifest names");
    }
    let contract = manifest.execution_contract().ok_or_else(|| {
        anyhow!("capability policy requires a version-1 contract for applet {applet_name:?}")
    })?;
    policy.authorize_validated(
        applet_name,
        false,
        binary,
        Some(ContractView {
            version: contract.version,
            capabilities: &contract.capabilities,
            effects: &contract.effects,
        }),
        Some(registry),
    )?;
    if grant.sandbox_required {
        if !grant.network_origins.is_empty() {
            bail!(
                "required OS sandbox cannot enforce network origin restrictions for applet {applet_name:?}"
            );
        }
        crate::platform::sandbox::preflight(grant, binary, cwd)
            .with_context(|| format!("preflighting required sandbox for applet {applet_name:?}"))?;
    }
    crate::security::capture_declared_environment_secrets(registry, applet_name, &environment)
        .map_err(|_| anyhow!("could not register declared child environment secrets"))?;
    let mut command = if grant.sandbox_required {
        crate::platform::sandbox::command(binary, arguments, grant, cwd)?
    } else {
        let mut command = Command::new(binary);
        command.args(arguments);
        command
    };
    command.env_clear().envs(environment);
    cwd.apply(&mut command);
    Ok(command)
}

pub use crate::home::expand_home_path;

/// Per-command environment changes from `and environment {...}` and
/// `and without environment [...]`. Set values are secrets for redaction: they
/// are registered as they are applied, and explain and trace show names only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandEnvOverlay {
    pub set: BTreeMap<String, String>,
    pub unset: Vec<String>,
}

thread_local! {
    static COMMAND_ENV_OVERLAY: std::cell::RefCell<Option<CommandEnvOverlay>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs one command's overlay for the current thread. The previous overlay
/// is restored when the guard drops, even on error.
pub struct CommandEnvScope(Option<CommandEnvOverlay>);

impl CommandEnvScope {
    pub fn enter(overlay: Option<CommandEnvOverlay>) -> Self {
        Self(COMMAND_ENV_OVERLAY.with(|cell| cell.replace(overlay)))
    }
}

impl Drop for CommandEnvScope {
    fn drop(&mut self) {
        let previous = self.0.take();
        COMMAND_ENV_OVERLAY.with(|cell| {
            cell.replace(previous);
        });
    }
}

/// Apply the active overlay to a launch environment. Unsets remove names first.
/// Each set name must pass the applet's environment grant, and its value is
/// registered with the redactor before the child can receive it.
fn apply_command_overlay(
    registry: &Registry,
    applet_name: &str,
    mut environment: Vec<(OsString, OsString)>,
) -> Result<Vec<(OsString, OsString)>> {
    let Some(overlay) = COMMAND_ENV_OVERLAY.with(|cell| cell.borrow().clone()) else {
        return Ok(environment);
    };
    for name in &overlay.unset {
        environment.retain(|(existing, _)| !same_environment_name(existing, name));
    }
    for (name, value) in &overlay.set {
        authorize_environment_override(registry, applet_name, name)?;
        crate::security::secret_redactor(registry)
            .register(value.as_bytes())
            .map_err(|_| anyhow!("could not register a command environment value as a secret"))?;
        environment.retain(|(existing, _)| !same_environment_name(existing, name));
        environment.push((OsString::from(name), OsString::from(value)));
    }
    environment.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(environment)
}

/// Resolve the complete per-applet child environment. The caller applies the
/// returned snapshot to one child only; this function never mutates the host
/// process environment. Values are intentionally not included in diagnostics.
pub fn effective_environment(
    registry: &Registry,
    applet_name: &str,
) -> Result<Vec<(OsString, OsString)>> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let manifest = match registry.resolve(applet_name) {
        Ok(manifest) => Some(manifest),
        Err(error) if configured.as_ref().is_none() => {
            // Legacy direct quoted commands use a synthetic manifest name
            // which is not registered. With no strict policy, preserve their
            // historical ambient environment and apply no manifest overlay.
            let _ = error;
            let mut environment = std::env::vars_os().collect::<Vec<_>>();
            environment.sort_by(|left, right| left.0.cmp(&right.0));
            return Ok(environment);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolving applet {applet_name:?} environment"));
        }
    };
    if let Some(manifest) = manifest {
        validate_environment_backend(manifest)?;
    }

    let mut environment = std::env::vars_os()
        .filter(|(name, _)| {
            environment_name_allowed(configured.as_ref().as_ref(), applet_name, name)
        })
        .collect::<Vec<_>>();
    if let Some(policy) = configured.as_ref() {
        policy.validate()?;
        if !policy.applets.contains_key(applet_name) {
            bail!("capability policy has no allowlist entry for applet {applet_name:?}");
        }
    }

    if let Some(overlay) = manifest.and_then(|manifest| manifest.environment()) {
        for name in &overlay.unset {
            environment.retain(|(existing, _)| !same_environment_name(existing, name));
        }
        for (name, value) in &overlay.set {
            authorize_environment_override(registry, applet_name, name)?;
            environment.retain(|(existing, _)| !same_environment_name(existing, name));
            environment.push((OsString::from(name), OsString::from(value)));
        }
    }
    environment.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(environment)
}

fn environment_name_allowed(
    policy: Option<&CapabilityPolicy>,
    applet_name: &str,
    variable: &OsStr,
) -> bool {
    let Some(policy) = policy else {
        return true;
    };
    let Some(variable) = variable.to_str() else {
        return false;
    };
    policy
        .applets
        .get(applet_name)
        .is_some_and(|grant| environment_name_granted(grant, variable))
}

/// Variables the dynamic loader reads before the program runs.
fn is_loader_variable(name: &str) -> bool {
    name.starts_with("LD_") || name.starts_with("DYLD_")
}

fn environment_name_granted(grant: &AppletGrant, variable: &str) -> bool {
    grant.environment.iter().any(|allowed| {
        if cfg!(windows) {
            allowed.eq_ignore_ascii_case(variable)
        } else {
            allowed == variable
        }
    })
}

#[cfg(windows)]
fn same_environment_name(left: &OsStr, right: &str) -> bool {
    left.to_str()
        .is_some_and(|left| left.eq_ignore_ascii_case(right))
}

#[cfg(not(windows))]
fn same_environment_name(left: &OsStr, right: &str) -> bool {
    left == OsStr::new(right)
}

fn authorize_environment_override(
    registry: &Registry,
    applet_name: &str,
    variable_name: &str,
) -> Result<()> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        // Without a policy, every other name is the author's choice (the Bash trust
        // model). The dynamic loader's variables change code that runs before the
        // program starts, so a binding or a manifest cannot set them.
        if is_loader_variable(variable_name) {
            bail!(
                "{variable_name} is read by the dynamic loader before the program starts; a program cannot set it"
            );
        }
        return Ok(());
    };
    policy.validate()?;
    let allowed = policy
        .applets
        .get(applet_name)
        .is_some_and(|grant| environment_name_granted(grant, variable_name));
    record_policy_decision(
        registry,
        applet_name,
        "environment.set",
        None,
        PolicyRisk::High,
        if allowed {
            PolicyDecisionOutcome::Approved
        } else {
            PolicyDecisionOutcome::Denied
        },
        if allowed {
            PolicyDecisionReason::PolicyMatched
        } else {
            PolicyDecisionReason::CapabilityNotGranted
        },
    );
    if allowed {
        Ok(())
    } else {
        bail!(
            "capability policy denies setting an environment variable for applet {applet_name:?}"
        );
    }
}

/// Stable per-applet policy identity for persistent-worker cache keys. A
/// replaced registry grant must never reuse a worker spawned under old rights.
pub fn policy_fingerprint(registry: &Registry, applet_name: &str) -> Result<Option<[u8; 32]>> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        return Ok(None);
    };
    policy.validate()?;
    let grant = policy.applets.get(applet_name).ok_or_else(|| {
        anyhow!("capability policy has no allowlist entry for applet {applet_name:?}")
    })?;
    let encoded = serde_json::to_vec(grant).context("encoding capability grant identity")?;
    let digest = Sha256::digest(encoded);
    Ok(Some(digest.into()))
}

/// Filter a captured worker environment with the active applet grant. `None`
/// means no policy is active and preserves legacy full inheritance.
pub fn filter_environment<I>(
    registry: &Registry,
    applet_name: &str,
    environment: I,
) -> Result<Option<Vec<(OsString, OsString)>>>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        return Ok(None);
    };
    policy.validate()?;
    let grant = policy.applets.get(applet_name).ok_or_else(|| {
        anyhow!("capability policy has no allowlist entry for applet {applet_name:?}")
    })?;
    let mut filtered = Vec::new();
    for (name, value) in environment {
        if name
            .to_str()
            .is_some_and(|name| environment_name_granted(grant, name))
        {
            filtered.push((name, value));
        }
    }
    Ok(Some(filtered))
}

/// Check permission before host code reads one environment variable for an
/// applet, including explicit remote bearer credentials. Values are never
/// included in diagnostics.
pub fn authorize_environment_variable(
    registry: &Registry,
    applet_name: &str,
    variable_name: &str,
) -> Result<()> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        return Ok(());
    };
    policy.validate()?;
    let allowed = policy
        .applets
        .get(applet_name)
        .is_some_and(|grant| environment_name_granted(grant, variable_name));
    record_policy_decision(
        registry,
        applet_name,
        "environment.read",
        None,
        PolicyRisk::High,
        if allowed {
            PolicyDecisionOutcome::Approved
        } else {
            PolicyDecisionOutcome::Denied
        },
        if allowed {
            PolicyDecisionReason::PolicyMatched
        } else {
            PolicyDecisionReason::CapabilityNotGranted
        },
    );
    if allowed {
        Ok(())
    } else {
        bail!("capability policy denies environment access for applet {applet_name:?}");
    }
}

/// Authorize the scheme/host/effective-port origin used by a host-managed
/// remote applet. Redirects and proxy behavior must remain disabled by the
/// transport layer.
pub fn authorize_network_endpoint(
    registry: &Registry,
    applet_name: &str,
    endpoint: &str,
) -> Result<()> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        return Ok(());
    };
    policy.validate()?;
    let Some(grant) = policy.applets.get(applet_name) else {
        record_policy_decision(
            registry,
            applet_name,
            "network.connect",
            None,
            PolicyRisk::High,
            PolicyDecisionOutcome::Denied,
            PolicyDecisionReason::MissingGrant,
        );
        bail!("capability policy has no allowlist entry for applet {applet_name:?}");
    };
    let manifest = registry
        .resolve(applet_name)
        .with_context(|| format!("resolving applet {applet_name:?} for network access"))?;
    if manifest.name != applet_name || manifest.execution_backend() != AppletBackend::Remote {
        bail!("network endpoint authorization applies only to canonical remote applets");
    }
    let contract = manifest.execution_contract().ok_or_else(|| {
        anyhow!("capability policy requires a version-1 contract for applet {applet_name:?}")
    })?;
    if contract.version != 1 || !grant.capabilities.contains("network.connect") || !grant.network {
        record_policy_decision(
            registry,
            applet_name,
            "network.connect",
            None,
            PolicyRisk::High,
            PolicyDecisionOutcome::Denied,
            PolicyDecisionReason::CapabilityNotGranted,
        );
        bail!("capability policy denies network access for applet {applet_name:?}");
    }
    let endpoint = match parse_http_url(endpoint, "remote endpoint") {
        Ok(endpoint) => endpoint,
        Err(error) => {
            record_policy_decision(
                registry,
                applet_name,
                "network.connect",
                None,
                PolicyRisk::High,
                PolicyDecisionOutcome::Denied,
                PolicyDecisionReason::NetworkOriginNotGranted,
            );
            return Err(error);
        }
    };
    if endpoint.fragment().is_some() {
        record_policy_decision(
            registry,
            applet_name,
            "network.connect",
            None,
            PolicyRisk::High,
            PolicyDecisionOutcome::Denied,
            PolicyDecisionReason::NetworkOriginNotGranted,
        );
        bail!("remote endpoint must not contain a fragment");
    }
    let origin = endpoint.origin().ascii_serialization();
    let allowed = grant.network_origins.is_empty() || grant.network_origins.contains(&origin);
    record_policy_decision(
        registry,
        applet_name,
        "network.connect",
        Some(&origin),
        PolicyRisk::High,
        if allowed {
            PolicyDecisionOutcome::Approved
        } else {
            PolicyDecisionOutcome::Denied
        },
        if allowed {
            PolicyDecisionReason::PolicyMatched
        } else {
            PolicyDecisionReason::NetworkOriginNotGranted
        },
    );
    if !allowed {
        bail!("capability policy denies the remote endpoint origin");
    }
    Ok(())
}

/// Guard host-language file operations that otherwise bypass a child sandbox.
/// Explicit path grants are shared as the policy's host file authority, so a
/// host read/save can happen only inside at least one declared read/write root.
pub fn authorize_host_path(
    registry: &Registry,
    path: &Path,
    write: bool,
    cwd: &Path,
) -> Result<()> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        return Ok(());
    };
    policy.validate()?;
    let allowed = canonicalize_for_grant(path, cwd)
        .ok()
        .is_some_and(|target| {
            policy.applets.values().any(|grant| {
                let paths = if write {
                    &grant.filesystem.write
                } else {
                    &grant.filesystem.read
                };
                paths.iter().any(|path| {
                    canonicalize_for_grant(path, cwd)
                        .is_ok_and(|root| grant_contains(&root, &target))
                })
            })
        });
    let (action, risk) = if write {
        ("filesystem.write", PolicyRisk::High)
    } else {
        ("filesystem.read", PolicyRisk::Elevated)
    };
    record_policy_decision(
        registry,
        "host",
        action,
        None,
        risk,
        if allowed {
            PolicyDecisionOutcome::Approved
        } else {
            PolicyDecisionOutcome::Denied
        },
        if allowed {
            PolicyDecisionReason::PolicyMatched
        } else {
            PolicyDecisionReason::CapabilityNotGranted
        },
    );
    if allowed {
        Ok(())
    } else {
        bail!("capability policy denies host filesystem access");
    }
}

/// Check a file operation against one applet's own path grant. This is used
/// for applet-owned host reads such as a remote bearer-file credential; the
/// generic workflow `Read`/`Save` guard instead uses the policy-wide host
/// grants because those effects have no applet subject.
pub fn authorize_applet_host_path(
    registry: &Registry,
    applet_name: &str,
    path: &Path,
    write: bool,
    cwd: &Path,
) -> Result<()> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        return Ok(());
    };
    policy.validate()?;
    let Some(grant) = policy.applets.get(applet_name) else {
        record_policy_decision(
            registry,
            applet_name,
            if write {
                "filesystem.write"
            } else {
                "filesystem.read"
            },
            None,
            if write {
                PolicyRisk::High
            } else {
                PolicyRisk::Elevated
            },
            PolicyDecisionOutcome::Denied,
            PolicyDecisionReason::MissingGrant,
        );
        bail!("capability policy has no allowlist entry for applet {applet_name:?}");
    };
    let target = canonicalize_for_grant(path, cwd).ok();
    let paths = if write {
        &grant.filesystem.write
    } else {
        &grant.filesystem.read
    };
    let allowed = target.as_ref().is_some_and(|target| {
        paths.iter().any(|path| {
            canonicalize_for_grant(path, cwd).is_ok_and(|root| grant_contains(&root, target))
        })
    });
    let (action, risk) = if write {
        ("filesystem.write", PolicyRisk::High)
    } else {
        ("filesystem.read", PolicyRisk::Elevated)
    };
    record_policy_decision(
        registry,
        applet_name,
        action,
        None,
        risk,
        if allowed {
            PolicyDecisionOutcome::Approved
        } else {
            PolicyDecisionOutcome::Denied
        },
        if allowed {
            PolicyDecisionReason::PolicyMatched
        } else {
            PolicyDecisionReason::CapabilityNotGranted
        },
    );
    if allowed {
        Ok(())
    } else {
        bail!("capability policy denies applet filesystem access");
    }
}

fn collect_invocations<'a>(steps: &'a [Step], calls: &mut Vec<&'a crate::ast::Invocation>) {
    for step in steps {
        match &step.kind {
            StepKind::Invoke { call, .. } => calls.push(call),
            StepKind::TaskGroup { tasks, .. } => calls.extend(tasks),
            StepKind::SpawnJob { stages, .. } => {
                calls.extend(stages.iter().map(|stage| &stage.call))
            }
            StepKind::IfCommand { call, .. }
            | StepKind::Loop {
                kind: crate::ast::LoopKind::WhileCommand { call, .. },
                ..
            } => calls.push(call),
            _ => {}
        }
    }
}

fn contains_cwd_change(program: &Program, registry: &Registry) -> Result<bool> {
    fn changed(steps: &[Step], registry: &Registry) -> Result<bool> {
        for step in steps {
            match &step.kind {
                StepKind::UseFolder { .. } => return Ok(true),
                StepKind::Invoke { call, .. }
                    if !call.direct
                        && registry.resolve_call(call)?.builtin
                            == Some(crate::manifest::Builtin::Cd) =>
                {
                    return Ok(true);
                }
                StepKind::TaskGroup { tasks, .. }
                    if tasks.iter().any(|call| {
                        !call.direct
                            && registry.resolve_call(call).is_ok_and(|manifest| {
                                manifest.builtin == Some(crate::manifest::Builtin::Cd)
                            })
                    }) =>
                {
                    return Ok(true);
                }
                _ => {}
            }
        }
        Ok(false)
    }
    if changed(&program.steps, registry)? {
        return Ok(true);
    }
    for definition in program.definitions.values() {
        if changed(&definition.steps, registry)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn preflight_host_paths(
    program: &Program,
    registry: &Registry,
    policy: &CapabilityPolicy,
    cwd: &Path,
    dynamic_cwd: bool,
) -> Result<()> {
    fn check_steps(
        steps: &[Step],
        registry: &Registry,
        policy: &CapabilityPolicy,
        cwd: &Path,
        dynamic_cwd: bool,
    ) -> Result<()> {
        for step in steps {
            // A temporary scope creates its path under the system temporary directory,
            // so the policy must allow writing there before any scope is created.
            if matches!(step.kind, StepKind::TempScope { .. }) {
                authorize_host_path(registry, &std::env::temp_dir(), true, cwd)?;
                continue;
            }
            let (path, write) = match &step.kind {
                StepKind::UseFolder { path } | StepKind::Read { path, .. } => (path, false),
                StepKind::Save { path, .. } => (path, true),
                StepKind::Append { path } => (path, true),
                StepKind::Redirect {
                    path: Some(path), ..
                } => (path, true),
                StepKind::LaunchDetached {
                    output: Some(path), ..
                } => (path, true),
                StepKind::Invoke { call, .. }
                    if !call.direct
                        && registry.resolve_call(call)?.builtin
                            == Some(crate::manifest::Builtin::Cd) =>
                {
                    let path = call.parameters.get("path").ok_or_else(|| {
                        anyhow!("capability policy requires a literal path for the cd builtin")
                    })?;
                    (path, false)
                }
                _ => continue,
            };
            let Value::String(path) = path else {
                bail!(
                    "capability policy requires literal host filesystem paths that can be checked before effects"
                );
            };
            let path = expand_home_path(path)?;
            if dynamic_cwd && !path.is_absolute() {
                bail!(
                    "capability policy cannot preflight a relative host path after a working-directory change"
                );
            }
            authorize_host_path(registry, &path, write, cwd)?;
        }
        // `policy` is passed explicitly to make it clear this scan is active
        // only after a policy was installed; it is also used by future path
        // schema extensions without changing the caller contract.
        let _ = policy;
        Ok(())
    }
    check_steps(&program.steps, registry, policy, cwd, dynamic_cwd)?;
    for definition in program.definitions.values() {
        check_steps(&definition.steps, registry, policy, cwd, dynamic_cwd)?;
    }
    Ok(())
}

pub(crate) fn canonicalize_for_grant(path: &Path, cwd: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.as_os_str().as_encoded_bytes().contains(&0) {
        bail!("filesystem path grant is empty or invalid");
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return Ok(canonical);
    }
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| anyhow!("filesystem path cannot be safely resolved"))?;
        suffix.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow!("filesystem path cannot be safely resolved"))?;
    }
    let mut resolved =
        std::fs::canonicalize(ancestor).context("canonicalizing filesystem path parent")?;
    for component in suffix.iter().rev() {
        match component.as_os_str().to_str() {
            Some(".") => {}
            Some("..") => {
                if resolved != Path::new("/") {
                    resolved.pop();
                }
            }
            _ => resolved.push(component),
        }
    }
    Ok(resolved)
}

fn validate_policy_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.as_os_str().as_encoded_bytes().contains(&0) {
        bail!("path must be nonempty and contain no NUL byte");
    }
    // A drive prefix must be followed by the root, so `C:data` is refused.
    if let Some(Component::Prefix(prefix)) = path.components().next() {
        if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
            && !matches!(path.components().nth(1), Some(Component::RootDir))
        {
            bail!("drive-relative paths are not supported; use an absolute drive path");
        }
    }
    Ok(())
}

/// Whether `target` lies at or under the canonical grant `root`. Matching is
/// exact: components compare case-sensitively, so a directory whose on-disk
/// name differs from the grant is a different directory. The only folding is
/// the verbatim `\\?\` prefix, the drive letter, and `/` versus `\`.
#[cfg(windows)]
fn grant_contains(root: &Path, target: &Path) -> bool {
    let root = windows_grant_key(root);
    let target = windows_grant_key(target);
    if target == root {
        return true;
    }
    let separator = u16::from(b'\\');
    target.starts_with(&root)
        && (root.last() == Some(&separator) || target[root.len()] == separator)
}

#[cfg(not(windows))]
fn grant_contains(root: &Path, target: &Path) -> bool {
    target == root || target.starts_with(root)
}

#[cfg(windows)]
fn windows_grant_key(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let separator = u16::from(b'\\');
    let mut units = path.as_os_str().encode_wide().collect::<Vec<u16>>();
    // `\\?\C:` is the verbatim spelling of `C:`; canonicalize returns the former.
    let verbatim = [separator, separator, u16::from(b'?'), separator];
    if units.len() >= 6 && units[..4] == verbatim && units[5] == u16::from(b':') {
        units.drain(..4);
    }
    for unit in &mut units {
        if *unit == u16::from(b'/') {
            *unit = separator;
        }
    }
    // Only the drive letter folds; every other component keeps its exact spelling.
    if units.len() >= 2
        && units[1] == u16::from(b':')
        && (u16::from(b'a')..=u16::from(b'z')).contains(&units[0])
    {
        units[0] -= u16::from(b'a' - b'A');
    }
    units
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("expected exactly 64 hexadecimal characters");
    }
    Ok(())
}

fn parse_http_url(value: &str, context: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("invalid {context}"))?;
    let has_userinfo = url
        .as_str()
        .split_once("://")
        .and_then(|(_, remainder)| remainder.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'));
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || has_userinfo
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("{context} must be an HTTP(S) URL without embedded credentials");
    }
    Ok(url)
}

fn validate_network_origin(value: &str) -> Result<()> {
    let url = parse_http_url(value, "network origin")?;
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        bail!("network origin must not contain a path, query, or fragment");
    }
    let canonical = url.origin().ascii_serialization();
    if canonical != value {
        bail!("network origin must use canonical scheme, host, and port spelling");
    }
    Ok(())
}

fn validate_applet_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn validate_capability_id(value: &str) -> Result<()> {
    let mut segments = value.split('.');
    let first = segments.next().unwrap_or_default();
    let mut namespaced = false;
    if first.is_empty()
        || !first.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
        || !segments.all(|segment| {
            namespaced = true;
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
        })
        || !namespaced
    {
        bail!("expected a namespaced lowercase identifier");
    }
    Ok(())
}

fn validate_environment_name(value: &str) -> Result<()> {
    let mut bytes = value.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        bail!("expected an environment variable name using A-Z, 0-9, and underscore");
    }
    Ok(())
}

/// Only host-owned, side-effect-free transformations may run in process when a
/// strict policy is active. This exact-name list prevents a custom manifest
/// from labelling `fs.read` or another effectful dispatcher branch as pure.
fn is_policy_safe_in_process(name: &str) -> bool {
    matches!(
        name,
        "color.parse_rgb"
            | "color.parse_rgb.result"
            | "color.rgb_to_hex"
            | "number.add"
            | "number.subtract"
            | "number.multiply"
            | "number.divide"
            | "number.compare"
            | "number.parse_integer"
            | "number.parse_integer.result"
            | "number.to_text"
            | "path.basename"
            | "path.dirname"
            | "path.remove_suffix"
            | "text.concat"
            | "text.default_if_empty"
            | "text.join"
            | "text.lines"
            | "text.lower"
            | "text.matches"
            | "text.replace"
            | "text.split"
            | "text.strip_prefix"
            | "text.strip_suffix"
            | "text.trim_final_lf"
            | "text.upper"
            | "path.join"
            | "yaml.parse"
            | "yaml.stringify"
            | "toml.parse"
            | "toml.stringify"
            | "ini.parse"
            | "ini.stringify"
            | "env.load"
            | "json.parse"
            | "json.stringify"
            | "json.get"
            | "json.set"
            | "json.map"
            | "json.filter"
            | "jsonc.edit_content"
    )
}

#[cfg(all(test, windows))]
mod windows_grant_path_tests {
    use super::*;

    #[test]
    fn drive_grants_are_accepted_and_drive_relative_paths_are_refused() {
        assert!(validate_policy_path(Path::new(r"C:\data")).is_ok());
        assert!(validate_policy_path(Path::new(r"\\?\C:\data")).is_ok());
        assert!(validate_policy_path(Path::new(r"C:data")).is_err());
    }

    #[test]
    fn a_drive_grant_allows_its_tree_in_verbatim_and_plain_spelling() {
        let grant = Path::new(r"C:\Data");
        assert!(grant_contains(grant, Path::new(r"C:\Data\x")));
        assert!(grant_contains(grant, Path::new(r"\\?\C:\Data\x")));
        assert!(grant_contains(grant, Path::new(r"C:\Data")));
    }

    #[test]
    fn a_drive_grant_refuses_siblings_that_share_a_prefix_or_differ() {
        let grant = Path::new(r"C:\Data");
        assert!(!grant_contains(grant, Path::new(r"C:\DataX")));
        assert!(!grant_contains(grant, Path::new(r"C:\Other")));
        assert!(!grant_contains(grant, Path::new(r"\\?\C:\DataX\x")));
    }

    #[test]
    fn components_match_exactly_but_the_drive_letter_and_separators_do_not() {
        // A different-case component is a different directory unless canonicalize produced it.
        assert!(!grant_contains(
            Path::new(r"C:\Data"),
            Path::new(r"C:\data\x")
        ));
        assert!(grant_contains(
            Path::new(r"c:\Data"),
            Path::new(r"C:\Data\x")
        ));
        assert!(grant_contains(
            Path::new(r"C:/Data"),
            Path::new(r"C:\Data\x")
        ));
        assert!(grant_contains(Path::new(r"C:\"), Path::new(r"C:\anything")));
    }

    #[test]
    fn unc_and_device_targets_need_an_exact_unc_or_device_grant() {
        assert!(validate_policy_path(Path::new(r"\\server\share\data")).is_ok());
        assert!(grant_contains(
            Path::new(r"\\server\share\Data"),
            Path::new(r"\\server\share\Data\x")
        ));
        assert!(!grant_contains(
            Path::new(r"C:\Data"),
            Path::new(r"\\server\share\Data\x")
        ));
        assert!(!grant_contains(
            Path::new(r"\\server\share\Data"),
            Path::new(r"\\server\share\data\x")
        ));
    }
}
