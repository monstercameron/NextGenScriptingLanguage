//! Opt-in applet grants and whole-program capability preflight.
//!
//! Without a configured policy Execute retains its historical trusted-process
//! behavior. Once a policy is installed, every invoked applet must have an
//! explicit canonical-name grant and a supported version-1 manifest contract.
//! A policy defaults native children to untrusted execution: an unavailable or
//! insufficient OS sandbox is a hard refusal. Trusted hosts can explicitly
//! select the historical declaration-only mode.

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

pub const CAPABILITY_POLICY_VERSION: u8 = 1;
const MAX_CAPABILITY_POLICY_BYTES: usize = 64 * 1024;

/// Host-selected execution mode. Policies select `Untrusted` by default;
/// an explicit `Trusted` selection never overrides `sandbox_required` grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Trusted,
    Untrusted,
}

impl ExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }
}

pub fn execution_mode(registry: &Registry) -> ExecutionMode {
    registry
        .host_state::<Option<ExecutionMode>>()
        .unwrap_or_else(|| {
            if capability_policy_active(registry) {
                ExecutionMode::Untrusted
            } else {
                ExecutionMode::Trusted
            }
        })
}

/// Whether a capability policy was installed for this registry.
pub fn capability_policy_active(registry: &Registry) -> bool {
    registry.host_state::<Option<CapabilityPolicy>>().is_some()
}

/// Prospective enforcement for graph, screen and stream advisory renderers.
pub fn invocation_enforcement(
    registry: &Registry,
    call: &crate::ast::Invocation,
) -> Result<&'static str> {
    let manifest = registry.resolve_call(call)?;
    if manifest.builtin.is_some()
        || !matches!(
            manifest.execution_backend(),
            AppletBackend::Command | AppletBackend::JsonLinesWorker
        )
    {
        return Ok("host_managed");
    }
    let policy = registry.host_state::<Option<CapabilityPolicy>>();
    let grant = policy
        .as_ref()
        .as_ref()
        .and_then(|policy| policy.applets.get(&manifest.name));
    let required = execution_mode(registry) == ExecutionMode::Untrusted
        || grant.is_some_and(|grant| grant.sandbox_required);
    if !required {
        return Ok("declared_only");
    }
    if !cfg!(any(target_os = "linux", target_os = "macos"))
        || grant.is_none_or(|grant| {
            manifest.execution_contract().is_none_or(|contract| {
                validate_native_enforcement(grant, &contract.effects).is_err()
            })
        })
    {
        Ok("refused")
    } else {
        Ok("os_sandbox_required")
    }
}

/// A launch was withheld because OS confinement could not be guaranteed.
#[derive(Debug)]
pub struct SandboxRefused(pub String);

impl std::fmt::Display for SandboxRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "required OS sandbox refused execution: {}",
            self.0
        )
    }
}

impl std::error::Error for SandboxRefused {}

fn sandbox_required(registry: &Registry, grant: &AppletGrant) -> bool {
    grant.sandbox_required || execution_mode(registry) == ExecutionMode::Untrusted
}

fn require_execution_policy(registry: &Registry) -> Result<()> {
    if registry.host_state::<Option<CapabilityPolicy>>().is_none()
        && execution_mode(registry) == ExecutionMode::Untrusted
    {
        return Err(
            SandboxRefused("untrusted execution requires a capability policy".into()).into(),
        );
    }
    Ok(())
}

fn validate_native_enforcement(grant: &AppletGrant, effects: &[String]) -> Result<()> {
    if !grant.network_origins.is_empty() {
        return Err(SandboxRefused(
            "cannot enforce network origin restrictions for native children".into(),
        )
        .into());
    }
    for effect in effects {
        if !matches!(
            effect.as_str(),
            "filesystem.read" | "filesystem.write" | "network.connect" | "process.exec"
        ) {
            return Err(SandboxRefused(format!(
                "cannot enforce declared native effect {effect:?}"
            ))
            .into());
        }
    }
    Ok(())
}

fn preflight_native_sandbox(grant: &AppletGrant, binary: &Path, cwd: &Path) -> Result<()> {
    crate::platform::sandbox::preflight(grant, binary, cwd)
        .map_err(|error| SandboxRefused(format!("{error:#}")).into())
}

fn declared_sandbox_grant(grant: &AppletGrant, effects: &[String]) -> AppletGrant {
    let mut enforced = grant.clone();
    if !effects.iter().any(|effect| effect == "filesystem.read") {
        enforced.filesystem.read.clear();
    }
    if !effects.iter().any(|effect| effect == "filesystem.write") {
        enforced.filesystem.write.clear();
    }
    enforced.network &= effects.iter().any(|effect| effect == "network.connect");
    enforced
}

#[cfg(test)]
mod execution_tests {
    use super::*;

    fn grant() -> AppletGrant {
        AppletGrant {
            capabilities: BTreeSet::from([
                "process.exec".into(),
                "filesystem.read".into(),
                "filesystem.write".into(),
                "network.connect".into(),
            ]),
            binary_sha256: None,
            environment: BTreeSet::new(),
            filesystem: FilesystemGrant {
                read: vec!["read".into()],
                write: vec!["write".into()],
            },
            network: true,
            network_origins: BTreeSet::new(),
            sandbox_required: false,
        }
    }

    #[test]
    fn sandbox_authority_is_intersection_of_policy_and_declared_effects() {
        let original = grant();
        let enforced = declared_sandbox_grant(&original, &[]);
        assert!(enforced.filesystem.read.is_empty());
        assert!(enforced.filesystem.write.is_empty());
        assert!(!enforced.network);
        let enforced = declared_sandbox_grant(&original, &["filesystem.read".into()]);
        assert_eq!(enforced.filesystem.read, original.filesystem.read);
        assert!(enforced.filesystem.write.is_empty());
        assert!(!enforced.network);
        assert!(original.network, "the policy itself must not be changed");
    }

    #[test]
    fn native_effects_without_an_enforcement_mechanism_are_typed_refusals() {
        let error = validate_native_enforcement(&grant(), &["account.delete".into()]).unwrap_err();
        assert!(error.downcast_ref::<SandboxRefused>().is_some());
        assert!(error.to_string().contains("account.delete"));
    }

    #[test]
    fn native_origin_restrictions_are_typed_refusals_even_with_broad_network_access() {
        let mut restricted = grant();
        restricted
            .network_origins
            .insert("https://example.test".into());
        let error =
            validate_native_enforcement(&restricted, &["network.connect".into()]).unwrap_err();
        assert!(error.downcast_ref::<SandboxRefused>().is_some());
        assert!(error.to_string().contains("network origin"));
    }
}

/// Add truthful prospective enforcement to a static plan, without probing or
/// launching a child. `os_sandbox_required` is a requirement, not a claim that
/// an OS facility has already been installed by explain/what-if.
pub fn annotate_enforcement(plan: &mut serde_json::Value, registry: &Registry) {
    plan["execution_mode"] = execution_mode(registry).as_str().into();
    let Some(steps) = plan["steps"].as_array_mut() else {
        return;
    };
    let policy = registry.host_state::<Option<CapabilityPolicy>>();
    for step in steps {
        let native = step["process"] == true;
        let grant = step["applet"]
            .as_str()
            .and_then(|name| policy.as_ref().as_ref()?.applets.get(name));
        let effects: Vec<String> = step["effects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|effect| effect.as_str().map(str::to_owned))
            .collect();
        let required = native
            && (execution_mode(registry) == ExecutionMode::Untrusted
                || grant.is_some_and(|grant| grant.sandbox_required));
        step["enforcement"] = if required {
            if !cfg!(any(target_os = "linux", target_os = "macos"))
                || grant.is_none_or(|grant| validate_native_enforcement(grant, &effects).is_err())
            {
                "refused"
            } else {
                "os_sandbox_required"
            }
        } else if native {
            "declared_only"
        } else {
            "host_managed"
        }
        .into();
    }
}

/// Strict opt-in allowlist. A missing policy is represented by
/// `Registry::host_state::<Option<CapabilityPolicy>>() == None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityPolicy {
    pub version: u8,
    #[serde(default)]
    pub applets: BTreeMap<String, AppletGrant>,
    /// Permissions for host-language file operations such as `Read`, `Save`,
    /// and `UseFolder`. These actions have no applet subject and must not
    /// inherit authority from an unrelated applet.
    #[serde(default)]
    pub host: Option<HostGrant>,
    /// Host environment variables workflow inputs may read with from environment.
    /// Empty by default so programs cannot inspect ambient secrets under a policy.
    #[serde(default)]
    pub host_environment: BTreeSet<String>,
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
        // Without a policy the launch is allowed, and the audit says so
        // explicitly instead of staying silent about the unchecked launch.
        let shown = binary_names.first().copied().unwrap_or("host");
        record_policy_decision(
            registry,
            shown,
            "privilege.elevate",
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::NoPolicyConfigured,
        );
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

/// Per-applet permissions. Native filesystem/network access requires OS
/// confinement in untrusted mode or when `sandbox_required` is true.
/// Path grants also gate host-managed file
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

/// Filesystem capabilities granted to host-language workflow actions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostGrant {
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    #[serde(default)]
    pub filesystem: FilesystemGrant,
}

impl CapabilityPolicy {
    /// Read and validate a policy file with a fixed size limit.
    pub fn load(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        crate::security::open_private_file(path)
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
        if let Some(grant) = &self.host {
            for capability in &grant.capabilities {
                validate_capability_id(capability).context("host capability")?;
            }
            if !grant.filesystem.read.is_empty() && !grant.capabilities.contains("filesystem.read")
            {
                bail!("host grants filesystem reads without filesystem.read");
            }
            if !grant.filesystem.write.is_empty()
                && !grant.capabilities.contains("filesystem.write")
            {
                bail!("host grants filesystem writes without filesystem.write");
            }
            for path in grant
                .filesystem
                .read
                .iter()
                .chain(grant.filesystem.write.iter())
            {
                validate_policy_path(path).context("host filesystem grant")?;
            }
        }
        for variable in &self.host_environment {
            validate_environment_name(variable)
                .context("host_environment contains an invalid environment name")?;
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
/// Whether a program changes its working directory during a run. Relative host
/// paths are checked when the step runs, not before, if it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CwdMode {
    Fixed,
    Dynamic,
}

impl CwdMode {
    fn is_dynamic(self) -> bool {
        self == CwdMode::Dynamic
    }
}

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
    preflight_program(program, registry, cwd, cache_directory, true)
}

/// Perform all declaration and host-path authorization for advisory output,
/// without starting an OS facility probe. A real run always uses `preflight`.
pub fn preflight_plan(program: &Program, registry: &Registry, cwd: &Path) -> Result<()> {
    preflight_program(program, registry, cwd, None, false)
}

fn preflight_program(
    program: &Program,
    registry: &Registry,
    cwd: &Path,
    cache_directory: Option<&Path>,
    probe_sandbox: bool,
) -> Result<()> {
    require_execution_policy(registry)?;
    let cwd_mode = if contains_cwd_change(program, registry)? {
        CwdMode::Dynamic
    } else {
        CwdMode::Fixed
    };
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

    if cwd_mode.is_dynamic()
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

    preflight_invocations_with_policy(
        calls.iter().copied(),
        registry,
        cwd,
        cwd_mode,
        policy,
        probe_sandbox,
    )?;

    preflight_host_paths(program, registry, policy, cwd, cwd_mode)?;
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
    preflight_invocation_plan(calls, registry, cwd, true)
}

pub fn preflight_invocations_plan<'a>(
    calls: impl IntoIterator<Item = &'a crate::ast::Invocation>,
    registry: &Registry,
    cwd: &Path,
) -> Result<()> {
    preflight_invocation_plan(calls, registry, cwd, false)
}

fn preflight_invocation_plan<'a>(
    calls: impl IntoIterator<Item = &'a crate::ast::Invocation>,
    registry: &Registry,
    cwd: &Path,
    probe_sandbox: bool,
) -> Result<()> {
    require_execution_policy(registry)?;
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
    preflight_invocations_with_policy(
        calls.iter().copied(),
        registry,
        cwd,
        CwdMode::Fixed,
        policy,
        probe_sandbox,
    )?;
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
        authorize_applet_host_path(registry, &manifest.name, &directory, HostAccess::Read, cwd)?;
        authorize_applet_host_path(registry, &manifest.name, &directory, HostAccess::Write, cwd)?;
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
    cwd_mode: CwdMode,
    policy: &CapabilityPolicy,
    probe_sandbox: bool,
) -> Result<()> {
    // Privilege is checked for every call before any grant is, so a privileged step is
    // reported as such wherever it sits in the program.
    let calls: Vec<&crate::ast::Invocation> = calls.into_iter().collect();
    let mut sandboxes = Vec::new();
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
            // A relative target after a working-directory change is authorized when the
            // step runs, joined onto the directory it actually names (`enter_directory`).
            if !cwd_mode.is_dynamic() || Path::new(path).is_absolute() {
                authorize_host_path(registry, Path::new(path), HostAccess::Read, cwd)?;
            }
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
        let native_sandbox = matches!(
            backend,
            AppletBackend::Command | AppletBackend::JsonLinesWorker
        ) && sandbox_required(registry, grant);
        if native_sandbox {
            validate_native_enforcement(grant, &contract.effects)?;
        }
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
                        authorize_applet_host_path(registry, name, path, HostAccess::Read, cwd)?;
                    }
                }
            }
        }
        let binary = if backend != AppletBackend::InProcess
            && (grant.binary_sha256.is_some() || native_sandbox)
        {
            let requested = manifest.binary_for(call);
            if cwd_mode.is_dynamic()
                && !requested.starts_with('@')
                && !Path::new(requested).is_absolute()
            {
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
        if native_sandbox {
            if cwd_mode.is_dynamic() {
                bail!(
                    "required sandbox cannot preflight a program with dynamic working-directory changes"
                );
            }
            let binary = binary
                .as_deref()
                .expect("sandbox preflight resolved binary");
            sandboxes.push((
                declared_sandbox_grant(grant, &contract.effects),
                binary.to_path_buf(),
                name,
            ));
        }
    }
    // Complete declaration authorization for the whole program before probing
    // any sandbox facility, even for an earlier, otherwise authorized applet.
    for (grant, binary, name) in sandboxes {
        if probe_sandbox {
            preflight_native_sandbox(&grant, &binary, cwd)
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
    require_execution_policy(registry)?;
    #[cfg(feature = "failpoints")]
    if std::env::var("EXECUTE_TEST_PANIC_APPLET").is_ok_and(|name| name == applet_name) {
        panic!("injected native launch panic");
    }
    // The one check every spawn path reaches before a Command exists. It classifies the
    // manifest binary name, when the applet is registered, and the resolved path.
    let manifest_binary = registry
        .resolve(applet_name)
        .ok()
        .map(|manifest| manifest.binary.clone());
    let names: Vec<&str> = manifest_binary.as_deref().into_iter().collect();
    check_privileged_launch(registry, &names, binary)?;
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    // With no capability policy, command overlay, manifest environment section or declared
    // secret variable, the child environment is exactly the inherited one. The command then
    // carries no environment changes, so std passes `environ` through instead of copying
    // and rebuilding the whole environment for every child.
    let inherits_environment = configured.as_ref().is_none()
        && child_environment_allowlist()?.is_none()
        && !command_overlay_active()
        && registry
            .resolve(applet_name)
            .map_or(true, |manifest| manifest.environment().is_none())
        && !crate::security::declares_secret_environment(registry, applet_name);
    let environment = if inherits_environment {
        None
    } else {
        let environment = effective_environment(registry, applet_name)?;
        Some(apply_command_overlay(registry, applet_name, environment)?)
    };
    let manifest = match registry.resolve(applet_name) {
        Ok(manifest) => manifest,
        Err(_) if configured.as_ref().is_none() => {
            // Direct quoted commands use a synthetic manifest that is not in
            // the registry. Keep their legacy launch path when no strict
            // capability policy is active.
            let mut command = Command::new(binary);
            crate::platform::add_arguments(&mut command, arguments);
            apply_child_environment(registry, applet_name, &mut command, environment)?;
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
        let mut command = Command::new(binary);
        crate::platform::add_arguments(&mut command, arguments);
        apply_child_environment(registry, applet_name, &mut command, environment)?;
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
    let native_sandbox = sandbox_required(registry, grant);
    let enforced;
    let grant = if native_sandbox {
        enforced = declared_sandbox_grant(grant, &contract.effects);
        &enforced
    } else {
        grant
    };
    if native_sandbox {
        validate_native_enforcement(grant, &contract.effects)?;
        preflight_native_sandbox(grant, binary, cwd)
            .with_context(|| format!("preflighting required sandbox for applet {applet_name:?}"))?;
    }
    let mut command = if native_sandbox {
        crate::platform::sandbox::command(binary, arguments, grant, cwd)
            .map_err(|error| SandboxRefused(format!("{error:#}")))?
    } else {
        let mut command = Command::new(binary);
        crate::platform::add_arguments(&mut command, arguments);
        command
    };
    apply_child_environment(registry, applet_name, &mut command, environment)?;
    cwd.apply(&mut command);
    Ok(command)
}

/// Give a child its computed environment, registering declared secret values first. `None`
/// means the child inherits the process environment unchanged.
fn apply_child_environment(
    registry: &Registry,
    applet_name: &str,
    command: &mut Command,
    environment: Option<Vec<(OsString, OsString)>>,
) -> Result<()> {
    let Some(environment) = environment else {
        return Ok(());
    };
    crate::security::capture_declared_environment_secrets(registry, applet_name, &environment)
        .map_err(|_| anyhow!("could not register declared child environment secrets"))?;
    command.env_clear().envs(environment);
    Ok(())
}

/// Whether an `and environment` / `and without environment` overlay is active for the
/// command being launched on this thread.
fn command_overlay_active() -> bool {
    COMMAND_ENV_OVERLAY.with(|cell| cell.borrow().is_some())
}

/// PATH supplied by the active command overlay, when it changes PATH. An unset
/// PATH is represented by an empty value so lookup cannot silently fall back to
/// the interpreter's inherited PATH.
pub(crate) fn command_overlay_path() -> Option<OsString> {
    COMMAND_ENV_OVERLAY.with(|cell| {
        let overlay = cell.borrow();
        let overlay = overlay.as_ref()?;
        if overlay
            .unset
            .iter()
            .any(|name| same_environment_name(OsStr::new(name), "PATH"))
        {
            return Some(OsString::new());
        }
        overlay
            .set
            .iter()
            .find(|(name, _)| same_environment_name(OsStr::new(name), "PATH"))
            .map(|(_, value)| OsString::from(value))
    })
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
            // Direct quoted commands have no registered manifest, but still use
            // the same ambient environment restrictions as named applets.
            let _ = error;
            None
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolving applet {applet_name:?} environment"));
        }
    };
    if let Some(manifest) = manifest {
        validate_environment_backend(manifest)?;
    }

    let allowlist = child_environment_allowlist()?;
    let mut environment = std::env::vars_os()
        .filter(|(name, _)| {
            environment_name_allowed(configured.as_ref().as_ref(), applet_name, name)
                && allowlist.as_ref().is_none_or(|list| {
                    list.iter()
                        .any(|allowed| same_environment_name(name, allowed))
                })
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

/// Optional ambient child-environment allowlist, selected by the host. An empty
/// value clears the ambient environment. A malformed value fails closed.
fn child_environment_allowlist() -> Result<Option<Vec<String>>> {
    let Some(value) = std::env::var_os("EXECUTE_CHILD_ENV_ALLOW") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .ok_or_else(|| anyhow!("EXECUTE_CHILD_ENV_ALLOW must be UTF-8"))?;
    parse_child_environment_allowlist(value).map(Some)
}

/// Parse and validate the host's comma-separated native child environment allowlist.
/// The empty string means clear the environment; command-line callers use
/// `--clear-env` for that case.
pub fn parse_child_environment_allowlist(value: &str) -> Result<Vec<String>> {
    if value.len() > 8192 {
        bail!("EXECUTE_CHILD_ENV_ALLOW exceeds the 8192-byte limit");
    }
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for name in value.split(',') {
        let mut bytes = name.bytes();
        let valid = bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid || is_loader_variable(name) {
            bail!("EXECUTE_CHILD_ENV_ALLOW contains an invalid or loader-sensitive name");
        }
        if names.len() == 256 {
            bail!("EXECUTE_CHILD_ENV_ALLOW exceeds the 256-name limit");
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

/// Apply the host's optional ambient allowlist to a directly launched helper.
/// Unlike applet launches, these helpers have no per-applet policy or overlays.
pub fn apply_host_child_environment(command: &mut Command) -> Result<()> {
    if let Some(allowlist) = child_environment_allowlist()? {
        let environment = std::env::vars_os().filter(|(name, _)| {
            allowlist
                .iter()
                .any(|allowed| same_environment_name(name, allowed))
        });
        command.env_clear().envs(environment);
    }
    Ok(())
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
    if let Some(allowlist) = child_environment_allowlist()? {
        if !allowlist
            .iter()
            .any(|name| same_environment_name(OsStr::new(name), variable_name))
        {
            bail!("environment override {variable_name:?} is not in EXECUTE_CHILD_ENV_ALLOW");
        }
    }
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        // Without a policy, every other name is the author's choice (the Bash trust
        // model). The dynamic loader's variables change code that runs before the
        // program starts, so a binding or a manifest cannot set them. Both the
        // denial and the allowance are audit entries, so the log states what
        // happened instead of staying silent about it.
        if is_loader_variable(variable_name) {
            record_policy_decision(
                registry,
                applet_name,
                "environment.set",
                None,
                PolicyRisk::High,
                PolicyDecisionOutcome::Denied,
                PolicyDecisionReason::MissingGrant,
            );
            bail!(
                "{variable_name} is read by the dynamic loader before the program starts; a program cannot set it"
            );
        }
        record_policy_decision(
            registry,
            applet_name,
            "environment.set",
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::NoPolicyConfigured,
        );
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

impl CapabilityPolicy {
    /// Canonical content digest of the parsed policy: the SHA-256 of its
    /// canonical JSON encoding. Run records store this digest so a reviewer
    /// can tell which policy produced an allow/deny log. A digest is one-way:
    /// it names the policy without recording any grant, path, or other
    /// policy content.
    pub fn canonical_digest(&self) -> String {
        let encoded =
            serde_json::to_vec(self).expect("a validated capability policy is serializable");
        Sha256::digest(encoded)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
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
        record_policy_decision(
            registry,
            applet_name,
            "environment.read",
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::NoPolicyConfigured,
        );
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
        record_policy_decision(
            registry,
            applet_name,
            "network.connect",
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::NoPolicyConfigured,
        );
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
    let parsed_endpoint = match parse_http_url(endpoint, "remote endpoint") {
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
    if parsed_endpoint.fragment {
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
    let origin = canonical_http_origin(endpoint)?;
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

/// Whether a host path is being read or written, for filesystem policy checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostAccess {
    Read,
    Write,
}

/// Guard host-language file operations that otherwise bypass a child sandbox.
/// Host read/save can happen only inside a matching direction in the explicit
/// host grant; applet grants never authorize host-language file operations.
pub fn authorize_host_path(
    registry: &Registry,
    path: &Path,
    access: HostAccess,
    cwd: &Path,
) -> Result<()> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        record_policy_decision(
            registry,
            "host",
            if access == HostAccess::Write {
                "filesystem.write"
            } else {
                "filesystem.read"
            },
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::NoPolicyConfigured,
        );
        return Ok(());
    };
    policy.validate()?;
    let host = policy.host.as_ref();
    let capability = if access == HostAccess::Write {
        "filesystem.write"
    } else {
        "filesystem.read"
    };
    let paths = host.map(|grant| {
        if access == HostAccess::Write {
            &grant.filesystem.write
        } else {
            &grant.filesystem.read
        }
    });
    let allowed = host.is_some_and(|grant| grant.capabilities.contains(capability))
        && paths.into_iter().flatten().any(|root| {
            canonicalize_for_grant(path, cwd)
                .ok()
                .zip(canonicalize_for_grant(root, cwd).ok())
                .is_some_and(|(target, root)| grant_contains(&root, &target))
        });
    let (action, risk) = if access == HostAccess::Write {
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
    access: HostAccess,
    cwd: &Path,
) -> Result<()> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        record_policy_decision(
            registry,
            applet_name,
            if access == HostAccess::Write {
                "filesystem.write"
            } else {
                "filesystem.read"
            },
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::NoPolicyConfigured,
        );
        return Ok(());
    };
    policy.validate()?;
    let Some(grant) = policy.applets.get(applet_name) else {
        record_policy_decision(
            registry,
            applet_name,
            if access == HostAccess::Write {
                "filesystem.write"
            } else {
                "filesystem.read"
            },
            None,
            if access == HostAccess::Write {
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
    let paths = if access == HostAccess::Write {
        &grant.filesystem.write
    } else {
        &grant.filesystem.read
    };
    let allowed = target.as_ref().is_some_and(|target| {
        paths.iter().any(|path| {
            canonicalize_for_grant(path, cwd).is_ok_and(|root| grant_contains(&root, target))
        })
    });
    let (action, risk) = if access == HostAccess::Write {
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
    cwd_mode: CwdMode,
) -> Result<()> {
    authorize_environment_inputs(&program.inputs, registry)?;
    fn check_steps(
        steps: &[Step],
        registry: &Registry,
        policy: &CapabilityPolicy,
        cwd: &Path,
        cwd_mode: CwdMode,
    ) -> Result<()> {
        for step in steps {
            // A temporary scope creates its path under the system temporary directory,
            // so the policy must allow writing there before any scope is created.
            if matches!(step.kind, StepKind::TempScope { .. }) {
                authorize_host_path(registry, &std::env::temp_dir(), HostAccess::Write, cwd)?;
                continue;
            }
            let (path, access) = match &step.kind {
                StepKind::UseFolder { path } | StepKind::Read { path, .. } => {
                    (path, HostAccess::Read)
                }
                StepKind::Save { path, .. } => (path, HostAccess::Write),
                StepKind::Append { path } => (path, HostAccess::Write),
                StepKind::Redirect {
                    path: Some(path), ..
                } => (path, HostAccess::Write),
                StepKind::LaunchDetached {
                    output: Some(path), ..
                } => (path, HostAccess::Write),
                StepKind::Invoke { call, .. }
                    if !call.direct
                        && registry.resolve_call(call)?.builtin
                            == Some(crate::manifest::Builtin::Cd) =>
                {
                    let path = call.parameters.get("path").ok_or_else(|| {
                        anyhow!("capability policy requires a literal path for the cd builtin")
                    })?;
                    (path, HostAccess::Read)
                }
                _ => continue,
            };
            let Value::String(path) = path else {
                bail!("capability policy requires literal host filesystem paths that can be checked before effects");
            };
            let path = expand_home_path(path)?;
            if !cwd_mode.is_dynamic() || path.is_absolute() {
                authorize_host_path(registry, &path, access, cwd)?;
            }
        }
        let _ = policy;
        Ok(())
    }
    check_steps(&program.steps, registry, policy, cwd, cwd_mode)?;
    for definition in program.definitions.values() {
        check_steps(&definition.steps, registry, policy, cwd, cwd_mode)?;
    }
    Ok(())
}

/// Check environment-backed entry inputs against the configured capability policy.
/// Graph and module entrypoints call this before resolving inputs.
pub fn authorize_environment_inputs(
    declarations: &[crate::ast::InputDeclaration],
    registry: &Registry,
) -> Result<()> {
    let configured = registry.host_state::<Option<CapabilityPolicy>>();
    let Some(policy) = configured.as_ref() else {
        // Without a policy every declared environment input is accepted, and
        // the audit carries that baseline instead of implying a check ran.
        record_policy_decision(
            registry,
            "<workflow>",
            "environment.read",
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::NoPolicyConfigured,
        );
        return Ok(());
    };
    policy.validate()?;
    for declaration in declarations {
        if let Some(crate::ast::InputSource::Environment(name)) = &declaration.source {
            let allowed = policy.host_environment.contains(name);
            record_policy_decision(
                registry,
                "<workflow>",
                "environment.read",
                None,
                PolicyRisk::Elevated,
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
            if !allowed {
                bail!("capability policy denies reading environment variable {name:?}");
            }
        }
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

struct ParsedHttpUrl {
    origin: String,
    path: String,
    query: bool,
    fragment: bool,
}

pub(crate) fn canonical_http_origin(value: &str) -> Result<String> {
    let parsed = parse_http_url(value, "HTTP(S) URL")?;
    if parsed.fragment {
        bail!("HTTP(S) URL must not contain a fragment");
    }
    Ok(parsed.origin)
}

pub(crate) fn canonical_http_policy_origin(value: &str) -> Result<String> {
    let parsed = parse_http_url(value, "HTTP(S) origin")?;
    if parsed.path != "/" || parsed.query || parsed.fragment {
        bail!("HTTP(S) origin must not contain a path, query, or fragment");
    }
    Ok(parsed.origin)
}

fn parse_http_url(value: &str, context: &str) -> Result<ParsedHttpUrl> {
    let invalid = || anyhow!("invalid {context}");
    if value
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b'\\')
    {
        return Err(invalid());
    }
    let (scheme, remainder) = value.split_once("://").ok_or_else(invalid)?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        bail!("{context} must be an HTTP(S) URL without embedded credentials");
    }
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if !authority.is_ascii() {
        bail!("{context} host must use ASCII spelling");
    }
    if authority.is_empty() || authority.contains('@') {
        bail!("{context} must be an HTTP(S) URL without embedded credentials");
    }
    let (host, port, is_ipv6) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (address, tail) = bracketed.split_once(']').ok_or_else(invalid)?;
        let address = address
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| invalid())?;
        let port = if tail.is_empty() {
            None
        } else {
            Some(parse_url_port(tail.strip_prefix(':').ok_or_else(invalid)?)?)
        };
        (address.to_string(), port, true)
    } else {
        if authority.matches(':').count() > 1 {
            return Err(invalid());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(parse_url_port(port)?)),
            None => (authority, None),
        };
        let labels = host.split('.').collect::<Vec<_>>();
        let has_trailing_dot = host.ends_with('.');
        if host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
            || labels.iter().enumerate().any(|(index, label)| {
                (label.is_empty() && !(has_trailing_dot && index + 1 == labels.len()))
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || label.len() > 63
            })
        {
            return Err(invalid());
        }
        let final_label = labels.last().copied().unwrap_or_default();
        let looks_like_ipv4 = !final_label.is_empty()
            && (final_label.bytes().all(|byte| byte.is_ascii_digit())
                || final_label
                    .get(..2)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("0x"))
                    && final_label[2..]
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit()));
        if looks_like_ipv4
            && host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|address| address.to_string() != host)
        {
            return Err(invalid());
        }
        if looks_like_ipv4 && host.parse::<std::net::Ipv4Addr>().is_err() {
            return Err(invalid());
        }
        (host.to_ascii_lowercase(), port, false)
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    let rendered_host = if is_ipv6 { format!("[{host}]") } else { host };
    let origin = match port {
        Some(port) if port != default_port => format!("{scheme}://{rendered_host}:{port}"),
        _ => format!("{scheme}://{rendered_host}"),
    };
    let tail = &remainder[authority_end..];
    let fragment = tail.contains('#');
    let before_fragment = tail.split('#').next().unwrap_or_default();
    let query = before_fragment.contains('?');
    let path = before_fragment.split('?').next().unwrap_or_default();
    Ok(ParsedHttpUrl {
        origin,
        path: if path.is_empty() {
            "/".to_owned()
        } else {
            path.to_owned()
        },
        query,
        fragment,
    })
}

fn parse_url_port(value: &str) -> Result<u16> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid URL port");
    }
    value.parse::<u16>().context("invalid URL port")
}

fn validate_network_origin(value: &str) -> Result<()> {
    if canonical_http_policy_origin(value)? != value {
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
