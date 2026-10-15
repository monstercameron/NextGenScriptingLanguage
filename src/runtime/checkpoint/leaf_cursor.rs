//! Isolated format prototype for possible fine-grained checkpoint continuation.
//! The current journal and execution loop do not persist or consume this type;
//! it does not provide per-leaf resume behavior or close the recovery gap.
//!
//! The cursor models a leaf as one invocation pipeline group or one runtime
//! control-flow step. A stable source version pins the whole parsed program
//! and preloaded module fingerprints; dynamic frame paths distinguish repeated
//! call and loop executions. This format makes no automatic source migration
//! claim and never claims exactly-once external effects.

use super::EncodedPath;
use crate::ast::Value;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const FORMAT_VERSION: u32 = 1;
const MAX_FRAME_DEPTH: usize = 512;
const MAX_BINDINGS: usize = 4096;
const MAX_MODULES: usize = 256;
const MAX_SOURCE_STEPS: usize = 1_000_000;
const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;

/// Proposed whole-program identity to capture before the first effect. An
/// integration must reject source/module changes rather than remap indices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceVersion {
    pub semantic_sha256: String,
    pub step_count: usize,
    pub module_fingerprints: BTreeMap<String, String>,
}

/// Source location plus the semantic identity of the node at that location.
/// The index is diagnostic and bounds checking; the digest is the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SiteIdentity {
    pub step_index: usize,
    pub semantic_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DynamicFrameKind {
    Call,
    Loop,
    Branch,
    Try,
}

/// A dynamic occurrence in the active execution path. `ordinal` is the loop
/// iteration, call occurrence, or selected branch ordinal at this site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DynamicFrameIdentity {
    pub kind: DynamicFrameKind,
    pub site: SiteIdentity,
    pub ordinal: u64,
}

/// Proposed identity for one resumable leaf. A pipeline invocation group is
/// kept as one leaf so a crash between native stages remains uncertain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LeafIdentity {
    pub site: SiteIdentity,
    pub end_step: usize,
    pub dynamic_path: Vec<DynamicFrameIdentity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoopIterationState {
    Repeat {
        remaining: u64,
    },
    ForEach {
        binding: String,
        items: Vec<Value>,
        next_index: usize,
    },
    While {
        iteration: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoopFrameState {
    pub site: SiteIdentity,
    pub iteration: u64,
    pub outer_binding_names: BTreeSet<String>,
    pub state: LoopIterationState,
}

/// Values needed when a call returns to its caller. These are private local
/// checkpoint data, not an encrypted secret store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopeSnapshot {
    pub cwd: EncodedPath,
    pub bindings: BTreeMap<String, Value>,
    pub pipeline_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CallFrameState {
    pub site: SiteIdentity,
    pub return_step: usize,
    pub result_binding: Option<String>,
    pub caller: ScopeSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TryPhase {
    Body,
    Catch,
    Finally,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransferKind {
    Break,
    Continue,
    Return,
}

/// `anyhow::Error` is not serializable. The continuation stores its rendered
/// cause chain and rehydrates that text as an error if cleanup resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PendingExitState {
    Error {
        rendered_error: String,
        cancellation: bool,
    },
    Transfer {
        transfer: TransferKind,
        target_step: usize,
        call_depth: usize,
        loop_depth: usize,
        drop_bindings: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TryFrameState {
    pub site: SiteIdentity,
    pub catch_step: Option<usize>,
    pub finally_step: Option<usize>,
    pub end_step: usize,
    pub phase: TryPhase,
    pub pending_exit: Option<PendingExitState>,
    pub catch_binding: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "frame", rename_all = "snake_case")]
pub(crate) enum ExecutionFrame {
    Loop(LoopFrameState),
    Call(CallFrameState),
    Try(TryFrameState),
}

/// Required marker for a future integration's authorization to keep active
/// bindings and call snapshots as private, unencrypted local checkpoint data.
/// This prototype does not itself obtain or enforce that authorization, and
/// makes no claim that permissions protect against same-user or privileged
/// readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SensitiveStatePolicy {
    PrivatePlaintextAuthorized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum EffectContract {
    Pure,
    DeclaredIdempotent { key_sha256: String },
    CompensationRequired { journal_sha256: String },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InFlightLeaf {
    pub leaf: LeafIdentity,
    pub effect: EffectContract,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletedPrefix {
    pub leaves: u64,
    pub chain_sha256: String,
}

impl CompletedPrefix {
    pub fn empty() -> Self {
        Self {
            leaves: 0,
            chain_sha256: digest(b"execute-checkpoint-leaf-prefix-v1"),
        }
    }

    fn append(&mut self, leaf: &LeafIdentity) -> Result<()> {
        let bytes = serde_json::to_vec(leaf)?;
        let mut hash = Sha256::new();
        hash.update(b"execute-checkpoint-leaf-prefix-step-v1\0");
        hash.update(self.chain_sha256.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
        self.chain_sha256 = hex(&hash.finalize());
        self.leaves = self
            .leaves
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("completed leaf counter is exhausted"))?;
        Ok(())
    }
}

/// Proposed serializable continuation payload. This prototype only models the
/// data and validates transitions in memory; the current runtime does not
/// persist or restore it. A future integration would have to commit each
/// transition atomically with its execution snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LeafContinuation {
    pub format_version: u32,
    pub source: SourceVersion,
    pub sensitivity: SensitiveStatePolicy,
    pub completed_prefix: CompletedPrefix,
    pub next_step: usize,
    pub in_flight: Option<InFlightLeaf>,
    pub frames: Vec<ExecutionFrame>,
    pub cwd: EncodedPath,
    pub bindings: BTreeMap<String, Value>,
    /// Runtime must materialize replayable input at leaf boundaries. Stdin and
    /// arbitrary live streams cannot be reconstructed from this byte buffer.
    pub pipeline_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeDisposition {
    ContinueAtNextLeaf,
    RetryPureLeaf,
    RetryWithDeclaredIdempotencyKey { key_sha256: String },
    RecoverCompensationFirst { journal_sha256: String },
    RetryUncertainOnlyWithExplicitAuthorization,
}

impl LeafContinuation {
    pub fn validate(&self) -> Result<()> {
        if self.format_version != FORMAT_VERSION {
            bail!("unsupported leaf cursor version {}", self.format_version);
        }
        validate_digest(&self.source.semantic_sha256, "program fingerprint")?;
        if self.source.step_count == 0
            || self.source.step_count > MAX_SOURCE_STEPS
            || self.next_step > self.source.step_count
        {
            bail!("leaf cursor has an invalid source step position");
        }
        if self.source.module_fingerprints.len() > MAX_MODULES {
            bail!("leaf cursor exceeds the module fingerprint limit");
        }
        for (name, fingerprint) in &self.source.module_fingerprints {
            if name.is_empty() || name.len() > 4096 {
                bail!("leaf cursor has an empty module fingerprint name");
            }
            validate_digest(fingerprint, "module fingerprint")?;
        }
        validate_digest(
            &self.completed_prefix.chain_sha256,
            "completed-prefix digest",
        )?;
        if self.completed_prefix.leaves == 0
            && self.completed_prefix.chain_sha256 != CompletedPrefix::empty().chain_sha256
        {
            bail!("empty completed prefix has a nonempty digest");
        }
        validate_snapshot(&ScopeSnapshot {
            cwd: self.cwd.clone(),
            bindings: self.bindings.clone(),
            pipeline_bytes: self.pipeline_bytes.clone(),
        })?;
        if self.frames.len() > MAX_FRAME_DEPTH {
            bail!("leaf cursor exceeds the active frame depth limit");
        }
        for frame in &self.frames {
            validate_frame(frame, self.source.step_count)?;
        }
        if let Some(in_flight) = &self.in_flight {
            validate_leaf(&in_flight.leaf, self.source.step_count)?;
            if in_flight.leaf.site.step_index != self.next_step {
                bail!("in-flight leaf does not match the cursor's next step");
            }
            match &in_flight.effect {
                EffectContract::DeclaredIdempotent { key_sha256 } => {
                    validate_digest(key_sha256, "idempotency-key fingerprint")?;
                }
                EffectContract::CompensationRequired { journal_sha256 } => {
                    validate_digest(journal_sha256, "compensation-journal fingerprint")?;
                }
                EffectContract::Pure | EffectContract::Unknown => {}
            }
        }
        // Vec<u8> is JSON-encoded as an integer array. Bound the complete
        // durable representation, not only the raw byte and binding payloads.
        if serde_json::to_vec(self)?.len() > MAX_STATE_BYTES {
            bail!("leaf cursor serialized state exceeds 16 MiB");
        }
        Ok(())
    }

    pub fn begin_leaf(&mut self, leaf: LeafIdentity, effect: EffectContract) -> Result<()> {
        self.validate()?;
        if self.in_flight.is_some() {
            bail!("a checkpoint leaf is already in flight");
        }
        validate_leaf(&leaf, self.source.step_count)?;
        if leaf.site.step_index != self.next_step {
            bail!("leaf does not match the cursor's next source step");
        }
        self.in_flight = Some(InFlightLeaf { leaf, effect });
        self.validate()
    }

    /// Prototype transition. A future integration must call it only after the
    /// result snapshot is durably stored and must persist this update with that
    /// snapshot in the same journal replacement.
    pub fn complete_leaf(&mut self, leaf: &LeafIdentity, next_step: usize) -> Result<()> {
        if next_step > self.source.step_count {
            bail!("next leaf position is outside the source");
        }
        let Some(in_flight) = self.in_flight.as_ref() else {
            bail!("there is no checkpoint leaf in flight");
        };
        if &in_flight.leaf != leaf {
            bail!("completed leaf identity differs from the in-flight leaf");
        }
        let mut completed_prefix = self.completed_prefix.clone();
        completed_prefix.append(leaf)?;
        self.completed_prefix = completed_prefix;
        self.in_flight = None;
        self.next_step = next_step;
        self.validate()
    }

    /// Classify recovery without silently replaying an uncertain effect. A
    /// integrating runtime must perform compensation explicitly or opt into
    /// an uncertain retry before running the pending leaf again.
    pub fn resume_disposition(&self, explicit_uncertain_retry: bool) -> Result<ResumeDisposition> {
        self.validate()?;
        let Some(in_flight) = &self.in_flight else {
            return Ok(ResumeDisposition::ContinueAtNextLeaf);
        };
        Ok(match &in_flight.effect {
            EffectContract::Pure => ResumeDisposition::RetryPureLeaf,
            EffectContract::DeclaredIdempotent { key_sha256 } => {
                ResumeDisposition::RetryWithDeclaredIdempotencyKey {
                    key_sha256: key_sha256.clone(),
                }
            }
            EffectContract::CompensationRequired { journal_sha256 } => {
                ResumeDisposition::RecoverCompensationFirst {
                    journal_sha256: journal_sha256.clone(),
                }
            }
            EffectContract::Unknown if explicit_uncertain_retry => {
                ResumeDisposition::RetryUncertainOnlyWithExplicitAuthorization
            }
            EffectContract::Unknown => {
                bail!(
                    "in-flight leaf has uncertain external effects; explicit retry authorization is required"
                )
            }
        })
    }
}

fn validate_frame(frame: &ExecutionFrame, step_count: usize) -> Result<()> {
    match frame {
        ExecutionFrame::Loop(frame) => {
            validate_site(&frame.site, step_count)?;
            for name in &frame.outer_binding_names {
                validate_binding_name(name)?;
            }
            if let LoopIterationState::ForEach {
                binding,
                items,
                next_index,
            } = &frame.state
            {
                validate_binding_name(binding)?;
                if *next_index > items.len() {
                    bail!("foreach cursor index exceeds its saved item list");
                }
                validate_value_size(items)?;
            }
        }
        ExecutionFrame::Call(frame) => {
            validate_site(&frame.site, step_count)?;
            if frame.return_step > step_count {
                bail!("call frame return step is outside the source");
            }
            if let Some(binding) = &frame.result_binding {
                validate_binding_name(binding)?;
            }
            validate_snapshot(&frame.caller)?;
        }
        ExecutionFrame::Try(frame) => {
            validate_site(&frame.site, step_count)?;
            if frame.end_step > step_count
                || frame.catch_step.is_some_and(|step| step > step_count)
                || frame.finally_step.is_some_and(|step| step > step_count)
            {
                bail!("try frame marker is outside the source");
            }
            if let Some(binding) = &frame.catch_binding {
                validate_binding_name(binding)?;
            }
            if let Some(PendingExitState::Transfer {
                target_step,
                drop_bindings,
                ..
            }) = &frame.pending_exit
            {
                if *target_step > step_count {
                    bail!("pending control transfer target is outside the source");
                }
                for binding in drop_bindings {
                    validate_binding_name(binding)?;
                }
            }
        }
    }
    Ok(())
}

fn validate_leaf(leaf: &LeafIdentity, step_count: usize) -> Result<()> {
    validate_site(&leaf.site, step_count)?;
    if leaf.end_step <= leaf.site.step_index || leaf.end_step > step_count {
        bail!("leaf cursor has an invalid leaf range");
    }
    if leaf.dynamic_path.len() > MAX_FRAME_DEPTH {
        bail!("leaf identity exceeds the dynamic path depth limit");
    }
    for frame in &leaf.dynamic_path {
        validate_site(&frame.site, step_count)?;
    }
    Ok(())
}

fn validate_site(site: &SiteIdentity, step_count: usize) -> Result<()> {
    if site.step_index >= step_count {
        bail!("leaf cursor site is outside the source");
    }
    validate_digest(&site.semantic_sha256, "leaf semantic fingerprint")
}

fn validate_snapshot(snapshot: &ScopeSnapshot) -> Result<()> {
    if snapshot.bindings.len() > MAX_BINDINGS {
        bail!("leaf cursor has too many bindings");
    }
    for name in snapshot.bindings.keys() {
        validate_binding_name(name)?;
    }
    let bytes = serde_json::to_vec(&snapshot.bindings)?;
    if bytes.len().saturating_add(snapshot.pipeline_bytes.len()) > MAX_STATE_BYTES {
        bail!("leaf cursor active state exceeds 16 MiB");
    }
    let path = snapshot.cwd.to_path();
    if !path.is_absolute() {
        bail!("leaf cursor working directory must be absolute");
    }
    Ok(())
}

fn validate_value_size(value: &impl Serialize) -> Result<()> {
    if serde_json::to_vec(value)?.len() > MAX_STATE_BYTES {
        bail!("leaf cursor active values exceed 16 MiB");
    }
    Ok(())
}

fn validate_binding_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('\0') || name.len() > 4096 {
        bail!("leaf cursor has an invalid binding name");
    }
    Ok(())
}

fn validate_digest(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("leaf cursor has an invalid {label}");
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn site(step_index: usize, seed: &str) -> SiteIdentity {
        SiteIdentity {
            step_index,
            semantic_sha256: digest(seed.as_bytes()),
        }
    }

    fn cursor() -> LeafContinuation {
        LeafContinuation {
            format_version: FORMAT_VERSION,
            source: SourceVersion {
                semantic_sha256: digest(b"whole source version"),
                step_count: 8,
                module_fingerprints: BTreeMap::from([(
                    "colors".to_owned(),
                    digest(b"module body"),
                )]),
            },
            sensitivity: SensitiveStatePolicy::PrivatePlaintextAuthorized,
            completed_prefix: CompletedPrefix::empty(),
            next_step: 2,
            in_flight: None,
            frames: vec![ExecutionFrame::Loop(LoopFrameState {
                site: site(1, "repeat two times"),
                iteration: 1,
                outer_binding_names: BTreeSet::from(["theme".to_owned()]),
                state: LoopIterationState::Repeat { remaining: 1 },
            })],
            cwd: EncodedPath::from_path(Path::new("/tmp/checkpoint-cursor")),
            bindings: BTreeMap::from([("theme".to_owned(), Value::String("dark".to_owned()))]),
            pipeline_bytes: b"input to next leaf".to_vec(),
        }
    }

    #[test]
    fn leaf_identity_includes_loop_and_call_occurrence_path() {
        let base = LeafIdentity {
            site: site(2, "invoke set-theme"),
            end_step: 3,
            dynamic_path: vec![DynamicFrameIdentity {
                kind: DynamicFrameKind::Loop,
                site: site(1, "repeat two times"),
                ordinal: 0,
            }],
        };
        let mut second_iteration = base.clone();
        second_iteration.dynamic_path[0].ordinal = 1;
        assert_ne!(base, second_iteration);
        assert_eq!(base.site, second_iteration.site);
    }

    #[test]
    fn cursor_roundtrips_active_loop_call_state_and_module_identity() {
        let mut saved = cursor();
        saved.frames.push(ExecutionFrame::Call(CallFrameState {
            site: site(0, "call colors"),
            return_step: 7,
            result_binding: Some("hex".to_owned()),
            caller: ScopeSnapshot {
                cwd: EncodedPath::from_path(Path::new("/tmp/caller")),
                bindings: BTreeMap::from([("old".to_owned(), Value::Bool(true))]),
                pipeline_bytes: b"caller pipeline".to_vec(),
            },
        }));
        saved.validate().unwrap();
        let encoded = serde_json::to_vec(&saved).unwrap();
        let decoded: LeafContinuation = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, saved);
    }

    #[test]
    fn in_flight_unknown_effect_requires_explicit_retry_and_compensation_stays_separate() {
        let mut cursor = cursor();
        let leaf = LeafIdentity {
            site: site(2, "invoke mutate config"),
            end_step: 3,
            dynamic_path: vec![DynamicFrameIdentity {
                kind: DynamicFrameKind::Loop,
                site: site(1, "repeat two times"),
                ordinal: 0,
            }],
        };
        cursor
            .begin_leaf(leaf.clone(), EffectContract::Unknown)
            .unwrap();
        assert!(cursor.resume_disposition(false).is_err());
        assert_eq!(
            cursor.resume_disposition(true).unwrap(),
            ResumeDisposition::RetryUncertainOnlyWithExplicitAuthorization
        );
        cursor.in_flight.as_mut().unwrap().effect = EffectContract::CompensationRequired {
            journal_sha256: digest(b"transaction journal identity"),
        };
        assert_eq!(
            cursor.resume_disposition(false).unwrap(),
            ResumeDisposition::RecoverCompensationFirst {
                journal_sha256: digest(b"transaction journal identity")
            }
        );
    }

    #[test]
    fn completed_prefix_advances_only_for_the_matching_in_flight_leaf() {
        let mut cursor = cursor();
        let leaf = LeafIdentity {
            site: site(2, "pure transform"),
            end_step: 3,
            dynamic_path: vec![],
        };
        cursor
            .begin_leaf(leaf.clone(), EffectContract::Pure)
            .unwrap();
        let before = cursor.completed_prefix.clone();
        cursor.complete_leaf(&leaf, 3).unwrap();
        assert_eq!(cursor.completed_prefix.leaves, 1);
        assert_ne!(cursor.completed_prefix.chain_sha256, before.chain_sha256);
        assert_eq!(cursor.next_step, 3);
        assert!(cursor.in_flight.is_none());
    }

    #[test]
    fn cursor_rejects_source_drift_bad_ranges_and_missing_sensitive_opt_in() {
        let mut saved = cursor();
        saved.validate().unwrap();
        saved.source.semantic_sha256 = "not-a-digest".to_owned();
        assert!(saved.validate().is_err());

        let saved = cursor();
        let mut bad_leaf = LeafIdentity {
            site: site(2, "leaf"),
            end_step: 3,
            dynamic_path: vec![],
        };
        bad_leaf.end_step = 2;
        assert!(validate_leaf(&bad_leaf, 8).is_err());

        let mut json = serde_json::to_value(saved).unwrap();
        json.as_object_mut().unwrap().remove("sensitivity");
        assert!(serde_json::from_value::<LeafContinuation>(json).is_err());
    }

    #[test]
    fn cursor_bounds_the_full_serialized_active_state() {
        let mut cursor = cursor();
        // The raw input is below the 16 MiB payload bound, but its JSON array
        // representation exceeds the durable journal limit.
        cursor.pipeline_bytes = vec![255; 5 * 1024 * 1024];
        assert!(cursor.validate().is_err());
    }
}
