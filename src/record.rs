//! Opt-in run records: `execute run --record DIR` keeps a bounded, redacted account
//! of one finite run. `execute show-run DIR` summarises it, and `execute replay DIR`
//! reruns the program against it (see `replay`).
//!
//! Layout of a record directory:
//!
//! - `run.json`: protocol `execute.record/1`. Program hash, plan (explain text) and
//!   its hash, redacted inputs, timing, the final result and output hash, the
//!   limits that applied, the policy in force (execution mode and each
//!   configured policy's version and content digest, never policy contents),
//!   and the bounded policy-decision audit: what effects were allowed or
//!   denied and why. Each kept decision carries a link in a hash chain headed
//!   by `audit_head`, and the head plus the truncation accounting carry a
//!   keyed seal (`audit_mac`, an HMAC under the per-user key, versioned by
//!   `audit_seal_version`). The v2 seal also binds the record identity
//!   (`record_id`, the program and plan hashes) and the policy context, so
//!   copying an audit into another record — or editing the recorded policy,
//!   program or plan — breaks the seal. The retired v1 seal bound only the
//!   total, the flag and the head: it is still checked so old records stay
//!   readable, but a match is shown as an explicitly weak legacy seal and
//!   never as verified. `show-run` is warn-open: it always presents the audit
//!   and labels anything short of a fully verified chain and seal as unsealed
//!   or untrusted, so a tampered record stays reviewable instead of hiding
//!   its own evidence. `audit_verified` covers the policy audit only — never
//!   the result, steps, trace or blobs, which are shown as recorded. The
//!   chain evidences edits made without recomputing it; the seal raises the
//!   bar to key holders, but a same-user forger with key access can still
//!   rewrite the audit and reseal it, and key rotation is indistinguishable
//!   from tampering. Whole-record tamper evidence belongs to the
//!   record-integrity work.
//! - `program.program`: a copy of the program as it ran.
//! - `trace.jsonl`: one JSON object per applet group, in execution order, with the
//!   resolved argv, status, declared outcome, wall time, and the stdout and stderr
//!   byte counts, SHA-256 hashes and blob names.
//! - `blobs/<sha256>`: the stdout and stderr bytes, content-addressed. Blobs are
//!   written after redaction, so the hash matches the stored bytes.
//!
//! Nothing is written unless a record directory is requested. The directory is
//! created private (mode 0700 on Unix, files 0600). Redaction covers every value
//! that the data-flow policy declares secret (see `security`). It is exact-value
//! redaction, not detection, so an undeclared secret is recorded as it was.

use crate::manifest::Registry;
use crate::runtime::observe::{Observer, StepRequest, StepResult};
use crate::security::{self, PersistenceKind, SecretRedactor, SensitivitySet};
pub use crate::security::{register_sensitive_inputs, register_sensitive_program};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(any(not(unix), test))]
use std::fs;
use std::fs::File;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const PROTOCOL: &str = "execute.record/1";
pub const PROGRAM_FILE: &str = "program.program";
pub const RUN_FILE: &str = "run.json";
pub const TRACE_FILE: &str = "trace.jsonl";
pub const BLOB_DIR: &str = "blobs";
/// Total bytes of captured output kept across all blobs. Steps beyond this keep
/// their hashes and counts but no bytes, and the record is marked truncated.
pub const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;
/// Longest program copy a record will keep.
pub const MAX_PROGRAM_BYTES: usize = 1024 * 1024;
/// Longest explain text stored in the record.
pub const MAX_PLAN_BYTES: usize = 64 * 1024;
/// Stderr kept inline with each step, from its end.
pub const STDERR_TAIL_BYTES: usize = 2048;
/// Maximum trace size. Further steps are counted but not written after this bound.
pub const MAX_TRACE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RUN_BYTES: usize = 1024 * 1024;
#[cfg(unix)]
static RECORD_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The final state of a recorded run, written to `run.json`.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub ok: bool,
    pub exit_status: Option<i32>,
    pub error: Option<String>,
    pub output: Vec<u8>,
}

/// What a record needs to know about a run before it starts.
pub struct RecordStart<'a> {
    pub directory: &'a Path,
    pub program_source: &'a [u8],
    pub plan: &'a str,
    /// Inputs as JSON; string values are redacted before they are stored.
    pub inputs: Json,
    pub registry: &'a Registry,
    /// The per-user key that seals the policy-decision audit (`audit_mac`).
    /// `None` writes an unsealed record: the hash chain is still checked, but
    /// `show-run` warns that the seal is missing. A key holder can always
    /// rewrite the audit and reseal it, so the seal evidences naive edits and
    /// corruption, not a same-user forger with key access.
    pub audit_key: Option<[u8; crate::agent::what_if_key::KEY_BYTES]>,
}

/// Writes one finite run's record. Install it with `runtime::observe::install`.
pub struct Recorder {
    #[cfg(not(unix))]
    directory: PathBuf,
    #[cfg(unix)]
    directory_handle: File,
    #[cfg(unix)]
    blobs_handle: File,
    redactor: Arc<SecretRedactor>,
    /// Consulted for the data-flow persistence policy before any output is kept.
    registry: Registry,
    /// Key for the audit seal, or `None` for an unsealed record.
    audit_key: Option<[u8; crate::agent::what_if_key::KEY_BYTES]>,
    /// Random per-record identity, covered by the audit seal so a copied
    /// audit does not verify in another record.
    record_id: String,
    started: Instant,
    started_unix_ms: u128,
    plan_sha256: String,
    plan_text: String,
    inputs: Json,
    declared_secret_names: Vec<String>,
    state: Mutex<RecorderState>,
}

struct RecorderState {
    loop_limit: Option<usize>,
    loop_scope: Option<crate::runtime::LoopBudgetScope>,
    steps: usize,
    blob_bytes: usize,
    truncated: bool,
    trace: BufWriter<File>,
    trace_bytes: usize,
    trace_truncated: bool,
    blobs_written: BTreeSet<String>,
    /// Raw streamed stdout, in order, bounded by `MAX_BLOB_BYTES`. It is redacted
    /// as a whole when the record is finished, so a secret split across two
    /// streamed writes is still found.
    terminal: Vec<u8>,
    /// Union of the sensitivity labels of every observed step.
    labels: SensitivitySet,
    /// Whether any observed applet received declared secret environment.
    secret_environment: bool,
}

/// The plan as it is stored: redacted in full, then truncated, and hashed after
/// redaction. Redacting before truncation keeps a secret that straddles the limit
/// from surviving as a fragment, and hashing the redacted plan means the hash cannot
/// confirm a guess at a secret.
fn prepare_plan(redactor: &SecretRedactor, plan: &str) -> (String, String) {
    let redacted = redactor.redact_text(plan);
    let sha256 = sha256_hex(redacted.as_bytes());
    (truncate_text(&redacted, MAX_PLAN_BYTES), sha256)
}

/// How many of the in-memory policy decisions the run record keeps. Decisions
/// are safe by construction (bounded identifiers and canonical origins only, no
/// payloads or secrets), and the tail is kept so the most recent decision --
/// usually the one that failed the run -- survives. The lifetime total is
/// stored alongside, so a truncated audit is explicit rather than silent.
pub const MAX_AUDIT_DECISIONS: usize = crate::security::MAX_PERSISTED_DECISIONS;

/// Domain string for the audit hash chain. A head is only meaningful next to
/// the algorithm that made it; changing this string retires old heads.
const AUDIT_CHAIN_DOMAIN: &str = "execute.policy-audit/1";

/// Genesis head: the chain over zero persisted decisions. An empty audit
/// stores this head with a zero total.
fn audit_genesis() -> String {
    sha256_hex(AUDIT_CHAIN_DOMAIN.as_bytes())
}

fn push_audit_field(encoding: &mut Vec<u8>, text: &str) {
    encoding.extend_from_slice(&(text.len() as u64).to_le_bytes());
    encoding.extend_from_slice(text.as_bytes());
}

/// Length-prefixed encoding of one audit link: the previous head plus the
/// exact persisted fields of one decision, in a fixed order. Length prefixes
/// (not separators) keep the encoding injective whatever the field contents
/// hold. The link covers only the decision's own fields, never `audit_hash`
/// itself. Everything it covers is public by construction (bounded
/// identifiers, canonical origins, enum codes), so the chain hash cannot
/// confirm a guess at a secret.
fn audit_link_encoding(previous_head_hex: &str, decision: &Json) -> Vec<u8> {
    let mut encoding = Vec::new();
    push_audit_field(&mut encoding, previous_head_hex);
    push_audit_field(
        &mut encoding,
        &decision
            .get("sequence")
            .and_then(Json::as_u64)
            .map(|sequence| sequence.to_string())
            .unwrap_or_default(),
    );
    for name in ["subject", "action", "risk", "outcome", "reason"] {
        push_audit_field(
            &mut encoding,
            decision
                .get(name)
                .and_then(Json::as_str)
                .unwrap_or_default(),
        );
    }
    push_audit_field(
        &mut encoding,
        decision
            .get("scope_origin")
            .and_then(Json::as_str)
            .unwrap_or_default(),
    );
    encoding
}

/// Link one persisted decision onto the chain headed by `previous_head_hex`.
fn audit_link(previous_head_hex: &str, decision: &Json) -> String {
    sha256_hex(&audit_link_encoding(previous_head_hex, decision))
}

/// Domain string for the audit seal. A seal is only meaningful next to the
/// algorithm and fields it covers; changing this string retires old seals.
const AUDIT_SEAL_DOMAIN: &str = "execute.policy-audit-seal/2";

/// The retired v1 seal domain: total, flag and head only, with no record
/// binding. Records sealed before the record identity are still readable and
/// their chain is still checked, but they are never reported as verified: the
/// binding is weaker by construction (a copied audit verifies under it), so a
/// matching v1 seal is shown as an explicitly weak legacy seal, not as a v2
/// verification. New records never use this encoding.
const AUDIT_SEAL_DOMAIN_V1: &str = "execute.policy-audit-seal/1";

/// The values a v2 seal binds an audit to: the identity of this record (a
/// random per-record id plus the program and plan hashes) and the policy
/// context the decisions were made under (execution mode and each policy's
/// presence, version and digest). Everything covered is public by
/// construction (identifiers, hashes, counts, a flag, a chain hash), so the
/// seal cannot confirm a guess at a secret.
struct SealBinding<'a> {
    record_id: &'a str,
    program_sha256: &'a str,
    plan_sha256: &'a str,
    policy: &'a Json,
}

/// Length-prefixed encoding of the sealed values: the record identity, the
/// policy context, the lifetime total, the truncation flag, and the chain
/// head. The head transitively binds every kept decision, so sealing the head
/// seals the audit, and the identity and policy fields bind that audit to the
/// record and policy that produced it. Length prefixes (not separators) keep
/// the encoding injective whatever the field contents hold.
fn audit_seal_encoding(
    binding: &SealBinding,
    total: u64,
    truncated: bool,
    head_hex: &str,
) -> Vec<u8> {
    let mut encoding = Vec::new();
    push_audit_field(&mut encoding, AUDIT_SEAL_DOMAIN);
    push_audit_field(&mut encoding, binding.record_id);
    push_audit_field(&mut encoding, binding.program_sha256);
    push_audit_field(&mut encoding, binding.plan_sha256);
    push_audit_field(
        &mut encoding,
        binding
            .policy
            .get("execution_mode")
            .and_then(Json::as_str)
            .unwrap_or_default(),
    );
    for name in ["capability_policy", "data_flow_policy"] {
        let identity = binding.policy.get(name);
        push_audit_field(
            &mut encoding,
            identity
                .and_then(|identity| identity.get("status"))
                .and_then(Json::as_str)
                .unwrap_or_default(),
        );
        push_audit_field(
            &mut encoding,
            &identity
                .and_then(|identity| identity.get("version"))
                .and_then(Json::as_u64)
                .map(|version| version.to_string())
                .unwrap_or_default(),
        );
        push_audit_field(
            &mut encoding,
            identity
                .and_then(|identity| identity.get("sha256"))
                .and_then(Json::as_str)
                .unwrap_or_default(),
        );
    }
    push_audit_field(&mut encoding, &total.to_string());
    push_audit_field(
        &mut encoding,
        if truncated { "truncated" } else { "complete" },
    );
    push_audit_field(&mut encoding, head_hex);
    encoding
}

/// The retired v1 encoding: the lifetime total, the truncation flag, and the
/// chain head, with no record binding. Only the legacy-weak check for records
/// sealed before the record identity uses this; nothing new is sealed with it,
/// and a match never reports the audit as verified.
fn audit_seal_encoding_v1(total: u64, truncated: bool, head_hex: &str) -> Vec<u8> {
    let mut encoding = Vec::new();
    push_audit_field(&mut encoding, AUDIT_SEAL_DOMAIN_V1);
    push_audit_field(&mut encoding, &total.to_string());
    push_audit_field(
        &mut encoding,
        if truncated { "truncated" } else { "complete" },
    );
    push_audit_field(&mut encoding, head_hex);
    encoding
}

/// The keyed seal over a persisted audit: HMAC-SHA256 under the per-user key.
/// A same-user holder of the key can recompute this seal over a rewritten
/// audit, so it evidences edits made without the key (mistakes, corruption,
/// naive tampering), not a forger with key access.
fn audit_seal_hex(
    key: &[u8],
    binding: &SealBinding,
    total: u64,
    truncated: bool,
    head_hex: &str,
) -> String {
    crate::agent::what_if_key::hmac_sha256(
        key,
        &audit_seal_encoding(binding, total, truncated, head_hex),
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// A seal under the retired v1 encoding, for verifying records sealed before
/// the record identity. Its binding is weaker by construction: it covers only
/// the total, the flag and the head, so a copied audit verifies under it.
fn audit_seal_hex_v1(key: &[u8], total: u64, truncated: bool, head_hex: &str) -> String {
    crate::agent::what_if_key::hmac_sha256(key, &audit_seal_encoding_v1(total, truncated, head_hex))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The v2 seal binding read back from a stored record: the values the seal
/// must reproduce. `None` when a bound field is missing, which itself means
/// the record was edited after the run.
fn seal_binding<'a>(run: &'a Json, record_id: &'a str) -> Option<SealBinding<'a>> {
    Some(SealBinding {
        record_id,
        program_sha256: run.get("program")?.get("sha256")?.as_str()?,
        plan_sha256: run.get("plan")?.get("sha256")?.as_str()?,
        policy: run.get("policy")?,
    })
}

/// The policy in force for a run: the execution mode and, for each policy, its
/// presence plus the version and canonical content digest when configured.
/// Policy contents (grants, paths, origins or other policy material) are never
/// recorded; the digests are unsalted hashes, so they name the policy that
/// produced the allow/deny log without recording what it contains, but they
/// still confirm a guessed policy. Absence still tells a reviewer that no
/// confinement applied.
fn policy_context_json(registry: &Registry) -> Json {
    let capability = policy_identity_json(
        crate::capabilities::capability_policy_active(registry),
        registry
            .host_state::<Option<crate::capabilities::CapabilityPolicy>>()
            .as_ref()
            .as_ref()
            .map(|policy| (u64::from(policy.version), policy.canonical_digest())),
    );
    let data_flow_policy = registry.host_state::<security::DataFlowPolicy>();
    let data_flow = policy_identity_json(
        data_flow_policy.is_active(),
        data_flow_policy.is_active().then(|| {
            (
                u64::from(crate::security::DATA_FLOW_POLICY_VERSION),
                data_flow_policy.canonical_digest(),
            )
        }),
    );
    json!({
        "execution_mode": crate::capabilities::execution_mode(registry).as_str(),
        "capability_policy": capability,
        "data_flow_policy": data_flow,
    })
}

/// One policy's recorded identity: whether it was active, and the version and
/// content digest when it was. An absent policy carries nulls, so a reviewer
/// can distinguish "no policy" from "identity missing".
fn policy_identity_json(configured: bool, identity: Option<(u64, String)>) -> Json {
    match (configured, identity) {
        (true, Some((version, sha256))) => json!({
            "status": "configured",
            "version": version,
            "sha256": sha256,
        }),
        (true, None) => json!({
            "status": "configured",
            "version": Json::Null,
            "sha256": Json::Null,
        }),
        (false, _) => json!({
            "status": "absent",
            "version": Json::Null,
            "sha256": Json::Null,
        }),
    }
}

/// The bounded durable audit of policy/effect decisions: the most recent
/// decisions, each linked onto a hash chain, with the lifetime total, whether
/// the tail was cut, the head of the chain, and the keyed seal (`audit_mac`)
/// over the record identity, the policy context, the total, the truncation
/// flag and the head. The total comes from the recorder's counter, not the
/// bounded window, so it stays honest even after the window has dropped the
/// prefix. With no key the seal is JSON null and the record is unsealed.
fn decision_audit_json(
    registry: &Registry,
    audit_key: Option<&[u8]>,
    binding: &SealBinding,
) -> (Json, u64, bool, String, Json) {
    // The snapshot and the lifetime total come from one lock acquisition, so
    // the total can never run past the snapshot's own last sequence, even
    // while other threads are still recording.
    let (mut decisions, total) = security::policy_decisions_with_total(registry);
    // Insertion order is already sequence order (sequences are assigned under
    // the queue lock), but sort defensively: verification checks contiguity
    // in sequence order.
    decisions.sort_by_key(|decision| decision.sequence);
    let start = decisions.len().saturating_sub(MAX_AUDIT_DECISIONS);
    let mut head = audit_genesis();
    let items = decisions[start..]
        .iter()
        .map(|decision| {
            let mut value = serde_json::to_value(decision).unwrap_or(Json::Null);
            head = audit_link(&head, &value);
            value["audit_hash"] = Json::String(head.clone());
            value
        })
        .collect::<Vec<_>>();
    let truncated = total > items.len() as u64;
    let seal = audit_key
        .map(|key| Json::String(audit_seal_hex(key, binding, total, truncated, &head)))
        .unwrap_or(Json::Null);
    (Json::Array(items), total, truncated, head, seal)
}

/// Which key `show-run` verifies the audit seal against.
#[derive(Debug, Clone, Copy)]
pub enum SealKey<'a> {
    /// Verify against this exact key. Tests use a fixed key so the seal check
    /// is deterministic; production code never embeds a key here. A key of
    /// the wrong length is a caller error: verification reports the audit
    /// untrusted instead of silently treating the seal as unchecked.
    Explicit(&'a [u8]),
    /// Verify against the per-user key from its usual location, if one exists.
    /// A missing key is not an error: the audit is shown unsealed instead.
    System,
    /// Never check the seal. The audit is shown unsealed at best.
    Unavailable,
}

impl SealKey<'_> {
    /// Whether this key can never verify anything because it has the wrong
    /// length. A malformed explicit key is reported as untrusted, not as
    /// unsealed: the audit may be fine, but nothing checked it.
    fn is_malformed(&self) -> bool {
        matches!(
            self,
            SealKey::Explicit(key) if key.len() != crate::agent::what_if_key::KEY_BYTES
        )
    }

    /// The key bytes to verify against, or `None` when there is nothing to
    /// check with. Reading the system key never creates one: a missing key
    /// means "cannot verify", not "mint a replacement".
    fn resolve(&self) -> Option<[u8; crate::agent::what_if_key::KEY_BYTES]> {
        match self {
            SealKey::Explicit(key) => {
                let mut fixed = [0u8; crate::agent::what_if_key::KEY_BYTES];
                if key.len() != fixed.len() {
                    return None;
                }
                fixed.copy_from_slice(key);
                Some(fixed)
            }
            SealKey::System => crate::agent::what_if_key::load_existing().ok(),
            SealKey::Unavailable => None,
        }
    }
}

/// What verification of a persisted decision audit concluded. `show-run` is
/// warn-open: every state is presented, and anything short of `Verified`
/// carries a warning plus an `audit_error` in JSON output. Hiding the steps,
/// the result and the policy on tamper would turn `show-run` into an oracle
/// that confirms a record is clean by refusing to show it; showing untrusted
/// data labeled as untrusted keeps the record reviewable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AuditVerification {
    /// No `decisions` key at all: a record written before the audit existed.
    PreAudit,
    /// Decisions with no chain head: written between the bounded audit and the
    /// chained audit. Shown, but explicitly not verified.
    UnchainedLegacy,
    /// The chain checks, but there is no usable seal: either the record
    /// predates the seal or no key is available to check it with. Shown with
    /// an "unsealed" warning.
    Unsealed {
        approved: usize,
        denied: usize,
        reason: &'static str,
    },
    /// A chain or seal check failed. The audit is shown anyway, labeled
    /// untrusted, with the first failing check as the error.
    Untrusted {
        approved: usize,
        denied: usize,
        error: String,
    },
    /// A chained audit whose links, order, count, truncation flag and keyed
    /// seal all check.
    Verified { approved: usize, denied: usize },
}

impl AuditVerification {
    fn counts(decisions: &[Json]) -> (usize, usize) {
        let approved = decisions
            .iter()
            .filter(|decision| decision.get("outcome").and_then(Json::as_str) == Some("approved"))
            .count();
        let denied = decisions
            .iter()
            .filter(|decision| decision.get("outcome").and_then(Json::as_str) == Some("denied"))
            .count();
        (approved, denied)
    }

    fn untrusted(decisions: &[Json], error: String) -> Self {
        let (approved, denied) = Self::counts(decisions);
        AuditVerification::Untrusted {
            approved,
            denied,
            error,
        }
    }

    /// The machine-readable failure, if any, for `audit_error` JSON output.
    /// Every unverified state carries one: JSON consumers check
    /// `audit_verified`, and a null `audit_error` next to a false
    /// `audit_verified` would look like a missing field rather than a reason.
    fn error(&self) -> Option<&str> {
        match self {
            AuditVerification::PreAudit => {
                Some("this record predates the policy audit; no allow/deny log was kept")
            }
            AuditVerification::UnchainedLegacy => Some(
                "this decision audit predates chaining; edits after the run are not detectable",
            ),
            AuditVerification::Unsealed { reason, .. } => Some(reason),
            AuditVerification::Untrusted { error, .. } => Some(error),
            AuditVerification::Verified { .. } => None,
        }
    }
}

/// Verify the persisted decision audit in a `run.json` document.
///
/// The checks catch a decision edited in place (link mismatch), a decision
/// removed from the middle (sequence gap), a reordered audit (same sequences,
/// wrong order), a dropped prefix or suffix (first/last sequence against the
/// total), a truncation flag that disagrees with the total, an audit copied
/// from another record or policy (seal binding mismatch), a downgraded audit
/// with its record identity stripped (legacy-weak, never verified), and a
/// seal that does not match under the given key. Nothing here fails the
/// read: a failed check labels the audit untrusted or unsealed and the audit
/// is still shown, so a tampered record stays reviewable instead of hiding
/// its own evidence. Verification covers the policy audit only: the result,
/// steps, trace and blobs are shown as recorded and are never covered by the
/// seal.
///
/// Trust boundary: the chain is unanchored — it lives in the same file it
/// protects — and the seal is keyed under a per-user key that a same-user
/// editor can also read. Together they evidence edits made without recomputing
/// them (mistakes, corruption, naive tampering), not a same-user editor who
/// rewrites the decisions, the total, the head and the seal together
/// (threat-model A4). The seal raises the bar from "recompute hashes" to
/// "hold the key", and it still does not stop the key holder. Whole-record
/// tamper evidence, which would also cover that forger, belongs to the
/// record-integrity work (issue #414), not to this chain and seal.
fn verify_decision_audit(run: &Json, seal_key: &SealKey) -> AuditVerification {
    let Some(decisions_value) = run.get("decisions") else {
        return AuditVerification::PreAudit;
    };
    let Some(decisions) = decisions_value.as_array() else {
        return AuditVerification::untrusted(
            &[],
            "policy audit is not an array; the record may have been modified after the run"
                .to_owned(),
        );
    };
    let Some(head) = run.get("audit_head").and_then(Json::as_str) else {
        return AuditVerification::UnchainedLegacy;
    };
    let untrusted = |message: String| AuditVerification::untrusted(decisions, message);
    let Some(total) = run.get("decision_count").and_then(Json::as_u64) else {
        return untrusted(
            "policy audit has no decision total; the record may have been modified after the run"
                .to_owned(),
        );
    };
    let Some(truncated) = run.get("decisions_truncated").and_then(Json::as_bool) else {
        return untrusted(
            "policy audit has no truncation flag; the record may have been modified after the run"
                .to_owned(),
        );
    };
    if decisions.len() > MAX_AUDIT_DECISIONS {
        return untrusted(format!(
            "policy audit keeps {} decisions, past the {MAX_AUDIT_DECISIONS}-decision bound; the record may have been modified after the run",
            decisions.len()
        ));
    }
    if truncated != (total > decisions.len() as u64) {
        return untrusted(format!(
            "policy audit truncation flag disagrees with its total ({total}) and kept decisions ({}); the record may have been modified after the run",
            decisions.len()
        ));
    }
    // The chain check below establishes the counts; the seal check after it
    // needs them even when the audit turns out unsealed rather than verified.
    let chained = verify_audit_chain(decisions, head, total);
    let (approved, denied) = AuditVerification::counts(decisions);
    let chain_error = match chained {
        Ok(()) => None,
        Err(error) => Some(error),
    };
    if let Some(error) = chain_error {
        return AuditVerification::Untrusted {
            approved,
            denied,
            error,
        };
    }
    let stored_seal = run.get("audit_mac").and_then(Json::as_str);
    let Some(stored_seal) = stored_seal else {
        return AuditVerification::Unsealed {
            approved,
            denied,
            reason: "this record carries no audit seal; it predates the seal or was recorded without a key",
        };
    };
    if seal_key.is_malformed() {
        return AuditVerification::untrusted(
            decisions,
            "the explicit audit seal key has the wrong length, so the seal was not checked".into(),
        );
    }
    let Some(key) = seal_key.resolve() else {
        return AuditVerification::Unsealed {
            approved,
            denied,
            reason: "the per-user key is unavailable, so the audit seal was not checked",
        };
    };
    // Records sealed before the record identity bind only the total, the
    // flag and the head: a copied audit still matches under that retired
    // encoding, so it is never reported as verified. Records with an identity
    // bind the record and its policy context too, so a transplant or a
    // policy, program or plan edit breaks the seal. A v2 mismatch is either
    // tampering or a different key (rotation has no key identifier, so the
    // two are indistinguishable here); a legacy mismatch is tampering or a
    // different key under the retired encoding.
    let record_id = run.get("record_id").and_then(Json::as_str);
    let Some(record_id) = record_id else {
        let expected_v1 = audit_seal_hex_v1(&key, total, truncated, head);
        if expected_v1 != stored_seal {
            return AuditVerification::Untrusted {
                approved,
                denied,
                error: "policy audit seal does not match its total, truncation flag and head under the retired v1 encoding; the audit was modified after the run or sealed under a different key"
                    .to_owned(),
            };
        }
        return AuditVerification::Unsealed {
            approved,
            denied,
            reason: "this record uses the retired v1 audit seal without record binding; its audit is shown as recorded but could have been copied from another record — re-record for a verifiable audit",
        };
    };
    let Some(binding) = seal_binding(run, record_id) else {
        return AuditVerification::untrusted(
            decisions,
            "policy audit has no program or policy binding to check its seal against; the record may have been modified after the run".into(),
        );
    };
    let expected = audit_seal_hex(&key, &binding, total, truncated, head);
    if expected != stored_seal {
        return AuditVerification::Untrusted {
            approved,
            denied,
            error: "policy audit seal does not match its record identity, policy context, total, truncation flag and head; the audit was transplanted from another record, modified after the run, or sealed under a different key (key rotation is indistinguishable from tampering)"
                .to_owned(),
        };
    }
    AuditVerification::Verified { approved, denied }
}

/// Check the hash chain, the sequence order and the truncation accounting of
/// an audit whose envelope (total, flag, bound) already parsed. Returns the
/// first failing check as an error string.
fn verify_audit_chain(decisions: &[Json], head: &str, total: u64) -> Result<(), String> {
    if decisions.is_empty() {
        if total != 0 {
            return Err(format!(
                "policy audit kept no decisions but claims a total of {total}; the record may have been modified after the run"
            ));
        }
        if head != audit_genesis() {
            return Err(
                "policy audit head does not match an empty audit; the record may have been modified after the run"
                    .to_owned(),
            );
        }
        return Ok(());
    }
    let sequences = decisions
        .iter()
        .map(|decision| decision.get("sequence").and_then(Json::as_u64))
        .collect::<Option<Vec<_>>>();
    let Some(sequences) = sequences else {
        return Err("policy audit has a decision without a sequence number; the record may have been modified after the run".to_owned());
    };
    let first_expected = total
        .checked_sub(decisions.len() as u64)
        .and_then(|first| first.checked_add(1));
    let Some(first_expected) = first_expected else {
        return Err(format!("policy audit total ({total}) is below its kept decisions ({}); the record may have been modified after the run", decisions.len()));
    };
    let expected: Vec<u64> = (first_expected..=total).collect();
    let mut ordered = sequences.clone();
    ordered.sort_unstable();
    if ordered != expected {
        return Err(format!(
            "policy audit sequences run {first_expected}..={total} with a gap; a decision was removed after the run"
        ));
    }
    if sequences != expected {
        return Err(
            "policy audit decisions are out of sequence order; the audit was reordered after the run"
                .to_owned(),
        );
    }
    let mut running = audit_genesis();
    for decision in decisions {
        let sequence = decision
            .get("sequence")
            .and_then(Json::as_u64)
            .unwrap_or(0);
        let Some(stored) = decision.get("audit_hash").and_then(Json::as_str) else {
            return Err(format!("policy audit decision #{sequence} has no chain link; the record may have been modified after the run"));
        };
        running = audit_link(&running, decision);
        if stored != running {
            return Err(format!(
                "policy audit decision #{sequence} does not match its chain link; it was modified after the run"
            ));
        }
    }
    if head != running {
        return Err(
            "policy audit head does not match its decisions; the audit was modified after the run"
                .to_owned(),
        );
    }
    Ok(())
}

/// Whether the data-flow persistence policy lets output with these labels be kept
/// in a record. Output of an applet that receives declared secret environment is
/// never kept, whatever the policy says.
fn persistence_allowed(
    registry: &Registry,
    labels: &SensitivitySet,
    secret_environment: bool,
) -> bool {
    !secret_environment
        && security::authorize_persistence_set(
            registry,
            "workflow.record",
            PersistenceKind::Record,
            labels,
        )
        .is_ok()
}

impl Recorder {
    /// Create the record directory and store the program copy. Fails if the
    /// directory exists and is not empty, so a record is never mixed with another.
    pub fn start(start: RecordStart<'_>) -> Result<Self> {
        if start.program_source.len() > MAX_PROGRAM_BYTES {
            bail!("program is larger than the {MAX_PROGRAM_BYTES}-byte record limit");
        }
        #[cfg(unix)]
        let directory_handle = create_private_directory(start.directory)?;
        #[cfg(not(unix))]
        create_private_directory(start.directory)?;
        // Install all known masks before writing either the source copy or plan.
        let environment = std::env::vars_os().collect::<Vec<_>>();
        security::register_declared_environment_secrets(start.registry, &environment)
            .map_err(|error| anyhow!("registering declared secrets: {error}"))?;
        let redactor = security::secret_redactor(start.registry);
        let program_source = redactor.redact_bytes(start.program_source);
        #[cfg(unix)]
        write_private_at(&directory_handle, PROGRAM_FILE, &program_source)?;
        #[cfg(not(unix))]
        write_private(&start.directory.join(PROGRAM_FILE), &program_source)?;
        #[cfg(unix)]
        let trace = BufWriter::new(create_private_at(&directory_handle, TRACE_FILE)?);
        #[cfg(not(unix))]
        let trace = BufWriter::new(create_private(&start.directory.join(TRACE_FILE))?);
        #[cfg(unix)]
        let blobs_handle = create_private_subdirectory(&directory_handle, BLOB_DIR)?;
        #[cfg(not(unix))]
        create_private_directory(&start.directory.join(BLOB_DIR))?;
        let (plan_text, plan_sha256) = prepare_plan(&redactor, start.plan);
        // The identity is random per record: uniqueness (not secrecy) is what
        // stops a copied audit from verifying elsewhere.
        let mut id_bytes = [0u8; 16];
        getrandom::fill(&mut id_bytes).context("generating the record identity")?;
        let record_id = id_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let recorder = Self {
            #[cfg(not(unix))]
            directory: start.directory.to_path_buf(),
            #[cfg(unix)]
            directory_handle,
            #[cfg(unix)]
            blobs_handle,
            registry: start.registry.clone(),
            audit_key: start.audit_key,
            record_id,
            started: Instant::now(),
            started_unix_ms: unix_ms(),
            plan_sha256,
            plan_text,
            inputs: redact_json(&redactor, &start.inputs),
            declared_secret_names: security::declared_secret_environment_names(start.registry),
            redactor,
            state: Mutex::new(RecorderState {
                loop_limit: None,
                loop_scope: None,
                steps: 0,
                blob_bytes: 0,
                truncated: false,
                trace,
                trace_bytes: 0,
                trace_truncated: false,
                blobs_written: BTreeSet::new(),
                terminal: Vec::new(),
                labels: SensitivitySet::empty(),
                secret_environment: false,
            }),
        };
        recorder.write_run_document(false, None)?;
        Ok(recorder)
    }

    /// Record the execution limit before running so replay can use the same cap.
    pub fn set_loop_budget(
        &self,
        limit: usize,
        scope: crate::runtime::LoopBudgetScope,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.loop_limit = Some(limit);
        state.loop_scope = Some(scope);
        self.write_run_document_locked(&mut state, false, None)
    }

    /// Write `run.json` for a finished run. Call after the run, whatever its result.
    pub fn finish(&self, summary: &RunSummary) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.trace.flush()?;
        self.write_run_document_locked(&mut state, true, Some(summary))
    }

    fn write_run_document(&self, finished: bool, summary: Option<&RunSummary>) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.write_run_document_locked(&mut state, finished, summary)
    }

    fn write_run_document_locked(
        &self,
        state: &mut RecorderState,
        finished: bool,
        summary: Option<&RunSummary>,
    ) -> Result<()> {
        let output = if let Some(summary) = summary {
            let output_bytes = self.redactor.redact_bytes(&summary.output);
            let persist =
                persistence_allowed(&self.registry, &state.labels, state.secret_environment);
            self.blob(state, &output_bytes, persist)?
        } else {
            Json::Null
        };
        let terminal = if let Some(summary) = summary {
            let mut raw_terminal = state.terminal.clone();
            raw_terminal.extend_from_slice(&summary.output);
            let bytes = self.redactor.redact_bytes(&raw_terminal);
            let persist =
                persistence_allowed(&self.registry, &state.labels, state.secret_environment);
            self.blob(state, &bytes, persist)?
        } else {
            Json::Null
        };
        let error = summary.and_then(|summary| {
            summary
                .error
                .as_ref()
                .map(|error| self.redactor.redact_text(error))
        });
        // Snapshot after the persistence checks above, so the record's own
        // persistence decisions are part of the audit too.
        let policy = policy_context_json(&self.registry);
        let program_sha256 = sha256_hex(&self.read_program()?);
        let binding = SealBinding {
            record_id: self.record_id.as_str(),
            program_sha256: program_sha256.as_str(),
            plan_sha256: self.plan_sha256.as_str(),
            policy: &policy,
        };
        let (decisions, decision_count, decisions_truncated, audit_head, audit_mac) =
            decision_audit_json(
                &self.registry,
                self.audit_key.as_ref().map(|key| &key[..]),
                &binding,
            );
        let document = json!({
            "protocol": PROTOCOL,
            "finished": finished,
            "record_id": self.record_id,
            "program": {
                "file": PROGRAM_FILE,
                "sha256": program_sha256,
            },
            "plan": {
                "sha256": self.plan_sha256,
                "text": self.plan_text,
            },
            "inputs": self.inputs,
            "started_unix_ms": self.started_unix_ms,
            "wall_ms": self.started.elapsed().as_millis() as u64,
            "steps": state.steps,
            "result": {
                "ok": summary.map(|summary| summary.ok),
                "exit_status": summary.and_then(|summary| summary.exit_status),
                "error": error,
                "output": output,
            },
            "terminal": terminal,
            "truncated": state.truncated,
            "policy": policy,
            "decisions": decisions,
            "decision_count": decision_count,
            "decisions_truncated": decisions_truncated,
            "audit_head": audit_head,
            "audit_mac": audit_mac,
            "audit_seal_version": 2,
            "limits": {
                "loop_iterations_max": state.loop_limit,
                "loop_iterations_scope": state.loop_scope.map(|scope| match scope {
                    crate::runtime::LoopBudgetScope::PerLoop => "per_loop",
                    crate::runtime::LoopBudgetScope::LegacySharedRun => "legacy_shared_run",
                }),
                "blob_bytes_max": MAX_BLOB_BYTES,
                "blob_bytes_used": state.blob_bytes,
                "stderr_tail_bytes": STDERR_TAIL_BYTES,
            },
            "redaction": {
                "declared_secret_names": self.declared_secret_names,
            },
        });
        let text = serde_json::to_string_pretty(&document)?;
        #[cfg(unix)]
        atomic_write_private_at(&self.directory_handle, RUN_FILE, text.as_bytes())?;
        #[cfg(not(unix))]
        atomic_write_private(&self.directory.join(RUN_FILE), text.as_bytes())?;
        Ok(())
    }

    fn read_program(&self) -> Result<Vec<u8>> {
        #[cfg(unix)]
        {
            read_regular_no_follow_at(&self.directory_handle, PROGRAM_FILE)
                .context("reading recorded program")
        }
        #[cfg(not(unix))]
        {
            fs::read(self.directory.join(PROGRAM_FILE)).context("reading recorded program")
        }
    }

    /// Store bytes as a content-addressed blob and describe them. `bytes` must
    /// already be redacted, so the hash is of the stored content. When persistence is
    /// not allowed, only the count is kept: no bytes and no hash, since a hash of
    /// withheld content would confirm guesses about it. Past the budget, the
    /// description keeps the hash and count but names no blob.
    fn blob(&self, state: &mut RecorderState, bytes: &[u8], persist: bool) -> Result<Json> {
        if !persist {
            return Ok(json!({
                "bytes": bytes.len(),
                "blob": null,
                "withheld": "policy",
                "truncated": false,
            }));
        }
        let hash = sha256_hex(bytes);
        let within_budget = state.blob_bytes.saturating_add(bytes.len()) <= MAX_BLOB_BYTES;
        if !within_budget {
            state.truncated = true;
            return Ok(json!({
                "bytes": bytes.len(),
                "sha256": hash,
                "blob": null,
                "truncated": true,
            }));
        }
        if state.blobs_written.insert(hash.clone()) {
            #[cfg(unix)]
            write_private_at(&self.blobs_handle, &hash, bytes)?;
            #[cfg(not(unix))]
            write_private(&self.directory.join(BLOB_DIR).join(&hash), bytes)?;
            state.blob_bytes += bytes.len();
        }
        Ok(json!({
            "bytes": bytes.len(),
            "sha256": hash,
            "blob": format!("{BLOB_DIR}/{hash}"),
            "truncated": false,
        }))
    }
}

impl Observer for Recorder {
    fn replay(&self, _request: &StepRequest) -> Result<Option<StepResult>> {
        Ok(None)
    }

    fn finished(&self, request: &StepRequest, wall: Duration, result: &StepResult) -> Result<()> {
        // Redact each buffer in full before it is hashed, tailed or stored.
        let stdout = self.redactor.redact_bytes(&result.stdout);
        let stderr = self.redactor.redact_bytes(&result.stderr);
        let argv = request
            .argv
            .iter()
            .map(|argv| {
                argv.iter()
                    .map(|argument| self.redactor.redact_text(argument))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let index = state.steps;
        state.steps += 1;
        state.labels.extend(&result.sensitivity);
        state.secret_environment |= result.secret_environment;
        if state.trace_truncated || state.trace_bytes >= MAX_TRACE_BYTES {
            state.trace_truncated = true;
            state.truncated = true;
            return Ok(());
        }
        let persist = persistence_allowed(
            &self.registry,
            &result.sensitivity,
            result.secret_environment,
        );
        let stdout = self.blob(&mut state, &stdout, persist)?;
        let stderr_tail = if persist {
            String::from_utf8_lossy(&tail(&stderr, STDERR_TAIL_BYTES)).into_owned()
        } else {
            String::new()
        };
        let stderr = self.blob(&mut state, &stderr, persist)?;
        let outcome = match &result.outcome {
            Some(outcome) => json!({
                "name": outcome.name,
                "kind": serde_json::to_value(outcome.kind)?,
                "certainty": serde_json::to_value(outcome.effect_certainty)?,
            }),
            None => Json::Null,
        };
        let entry = json!({
            "event": "step",
            "id": crate::runtime::current_step_id().map(|id| id.to_string()),
            "index": index,
            "t_ms": self.started.elapsed().as_millis() as u64,
            "line": request.line,
            "applets": request.applets,
            "binaries": request.binaries,
            "argv": argv,
            "status": result.status,
            "outcome": outcome,
            "failed": result.failed,
            "wall_us": wall.as_micros() as u64,
            "stdout": stdout,
            "stderr": {
                "bytes": stderr["bytes"],
                "sha256": stderr["sha256"],
                "blob": stderr["blob"],
                "truncated": stderr["truncated"],
                "withheld": stderr.get("withheld").cloned().unwrap_or(Json::Null),
                "tail": stderr_tail,
            },
        });
        let line = serde_json::to_vec(&entry)?;
        let line_len = line.len().saturating_add(1);
        if state.trace_bytes.saturating_add(line_len) <= MAX_TRACE_BYTES {
            let mut line = line;
            line.push(b'\n');
            state.trace.write_all(&line)?;
            state.trace_bytes += line_len;
            state.trace.flush()?;
        } else {
            state.trace_truncated = true;
            state.truncated = true;
        }
        Ok(())
    }

    fn suppresses_writes(&self, _line: usize) -> bool {
        false
    }

    fn terminal_output(&self, bytes: &[u8]) {
        // Kept raw here and redacted as a whole in `finish`.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.terminal.len().saturating_add(bytes.len()) <= MAX_BLOB_BYTES {
            state.terminal.extend_from_slice(bytes);
        } else {
            state.truncated = true;
        }
    }
}

/// A recorded applet group, as read back from `trace.jsonl`.
#[derive(Debug, Clone)]
pub struct StoredStep {
    pub index: usize,
    pub line: usize,
    pub applets: Vec<String>,
    pub binaries: Vec<String>,
    pub argv: Vec<Vec<String>>,
    pub status: Option<i32>,
    pub outcome: Option<Json>,
    /// The group ended as a typed failure, which stops the program unless caught.
    pub failed: bool,
    pub wall_us: u64,
    pub stdout: Json,
    pub stderr: Json,
}

/// A record directory opened for reading.
pub struct StoredRecord {
    pub directory: PathBuf,
    program_directory: PathBuf,
    #[cfg(unix)]
    _directory_handle: File,
    #[cfg(unix)]
    blobs_handle: Option<File>,
    pub run: Json,
    pub steps: Vec<StoredStep>,
}

impl StoredRecord {
    pub fn open(directory: &Path) -> Result<Self> {
        #[cfg(unix)]
        let (directory_handle, program_directory) =
            crate::runtime::checkpoint::open_pinned_directory_with_path(directory)?;
        #[cfg(not(unix))]
        let program_directory = fs::canonicalize(directory)
            .with_context(|| format!("resolving record directory {}", directory.display()))?;
        #[cfg(unix)]
        let blobs_handle = match open_private_subdirectory(&directory_handle, BLOB_DIR) {
            Ok(handle) => Some(handle),
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                }) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        let run_bytes = read_regular_no_follow_at(&directory_handle, RUN_FILE)
            .with_context(|| format!("{} is not a run record", directory.display()))?;
        #[cfg(not(unix))]
        let run_bytes = read_regular_no_follow(&directory.join(RUN_FILE))
            .with_context(|| format!("{} is not a run record", directory.display()))?;
        let mut run: Json =
            serde_json::from_slice(&run_bytes).context("run.json is not valid JSON")?;
        if run["protocol"] != PROTOCOL {
            bail!("unsupported record protocol {}", run["protocol"]);
        }
        #[cfg(unix)]
        let trace_bytes = read_regular_no_follow_at(&directory_handle, TRACE_FILE)
            .context("reading trace.jsonl")?;
        #[cfg(not(unix))]
        let trace_bytes =
            read_regular_no_follow(&directory.join(TRACE_FILE)).context("reading trace.jsonl")?;
        let trace = String::from_utf8(trace_bytes).context("reading trace.jsonl")?;
        let trace_line_count = trace.lines().count();
        let has_torn_final_line = run["finished"] == false && !trace.ends_with('\n');
        let mut steps = Vec::new();
        for (number, line) in trace.lines().enumerate() {
            let value: Json = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) if number + 1 == trace_line_count && has_torn_final_line => break,
                Err(error) => {
                    return Err(error).with_context(|| format!("trace.jsonl line {}", number + 1));
                }
            };
            if value["event"] != "step" {
                continue;
            }
            steps.push(StoredStep {
                index: value["index"].as_u64().unwrap_or(0) as usize,
                line: value["line"].as_u64().unwrap_or(0) as usize,
                applets: strings(&value["applets"]),
                binaries: strings(&value["binaries"]),
                argv: value["argv"]
                    .as_array()
                    .map(|items| items.iter().map(strings).collect())
                    .unwrap_or_default(),
                status: value["status"].as_i64().map(|code| code as i32),
                outcome: (!value["outcome"].is_null()).then(|| value["outcome"].clone()),
                wall_us: value["wall_us"].as_u64().unwrap_or(0),
                stdout: value["stdout"].clone(),
                stderr: value["stderr"].clone(),
                failed: value["failed"].as_bool().unwrap_or(false),
            });
        }
        if run["finished"] == false {
            run["steps"] = json!(steps.len());
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            program_directory,
            #[cfg(unix)]
            _directory_handle: directory_handle,
            #[cfg(unix)]
            blobs_handle,
            run,
            steps,
        })
    }

    /// Return the canonical source identity captured when this record directory
    /// was opened. The path is for parsing and location defaults; source bytes are
    /// always read through the retained directory handle on Unix.
    pub fn program_path(&self) -> PathBuf {
        self.program_directory.join(PROGRAM_FILE)
    }

    /// Read the stored program from the directory handle retained by this record.
    pub fn program_source(&self) -> Result<Vec<u8>> {
        #[cfg(unix)]
        {
            read_regular_no_follow_at(&self._directory_handle, PROGRAM_FILE)
                .context("reading recorded program")
        }
        #[cfg(not(unix))]
        {
            read_regular_no_follow(&self.directory.join(PROGRAM_FILE))
                .context("reading recorded program")
        }
    }

    /// Read a stored blob and check its hash. A missing or truncated blob is an error.
    pub fn blob(&self, description: &Json) -> Result<Vec<u8>> {
        let Some(name) = description["blob"].as_str() else {
            if description["withheld"] == "policy" {
                bail!(
                    "this output was withheld from the record by the data-flow persistence policy"
                );
            }
            bail!("the record kept no bytes for this output (the record was truncated)");
        };
        let expected = description["sha256"].as_str().unwrap_or_default();
        // The name comes from record JSON, which may have been edited. Only the writer's
        // own `blobs/<sha256>` names are read, so no path can leave the record.
        if !valid_blob_name(name) {
            bail!("recorded blob name {name:?} is not a blob of this record");
        }
        #[cfg(unix)]
        let blob_name = name.strip_prefix("blobs/").expect("validated blob name");
        #[cfg(unix)]
        let blobs_handle = self
            .blobs_handle
            .as_ref()
            .context("record blob directory is missing")?;
        #[cfg(unix)]
        let bytes = read_regular_no_follow_at(blobs_handle, blob_name)
            .with_context(|| format!("reading recorded blob {name}"))?;
        #[cfg(not(unix))]
        let bytes = read_regular_no_follow(&self.directory.join(name))
            .with_context(|| format!("reading recorded blob {name}"))?;
        if sha256_hex(&bytes) != expected {
            bail!("recorded blob {name} does not match its hash");
        }
        Ok(bytes)
    }
}

/// Whether a stored blob name is exactly the writer's `blobs/<64 lowercase hex>` form.
fn valid_blob_name(name: &str) -> bool {
    name.strip_prefix(BLOB_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Read a record file without following a link in its final component, and require a
/// regular file. A record is read from its own directory, so a link planted there must
/// not redirect the read elsewhere.
#[cfg(any(not(unix), test))]
fn read_regular_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK keeps a FIFO from stalling the open; the regular-file check rejects it.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "{} is a link, not a record file",
                path.display()
            )));
        }
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let limit = match path.file_name().and_then(|name| name.to_str()) {
        Some(RUN_FILE) => MAX_RUN_BYTES,
        Some(TRACE_FILE) => MAX_TRACE_BYTES,
        Some(PROGRAM_FILE) => MAX_PROGRAM_BYTES,
        _ => MAX_BLOB_BYTES,
    };
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} exceeds the {limit}-byte record file limit",
                path.display()
            ),
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn create_private_at(directory: &File, name: &str) -> Result<File> {
    crate::runtime::checkpoint::open_child_at(
        directory,
        std::ffi::OsStr::new(name),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )
    .with_context(|| format!("creating record file {name}"))
}

#[cfg(unix)]
fn write_private_at(directory: &File, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = create_private_at(directory, name)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn create_private_subdirectory(directory: &File, name: &str) -> Result<File> {
    let name_c = std::ffi::CString::new(name.as_bytes())?;
    // SAFETY: directory is a live descriptor and name is a fixed single component.
    if unsafe {
        libc::mkdirat(
            std::os::fd::AsRawFd::as_raw_fd(directory),
            name_c.as_ptr(),
            0o700,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("creating record blob directory");
    }
    open_private_subdirectory(directory, name)
}

#[cfg(unix)]
fn open_private_subdirectory(directory: &File, name: &str) -> Result<File> {
    crate::runtime::checkpoint::open_child_at(
        directory,
        std::ffi::OsStr::new(name),
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )
    .with_context(|| format!("opening record subdirectory {name}"))
}

#[cfg(unix)]
fn read_regular_no_follow_at(directory: &File, name: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let file = crate::runtime::checkpoint::open_child_at(
        directory,
        std::ffi::OsStr::new(name),
        libc::O_RDONLY | libc::O_NONBLOCK,
        0,
    )?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(format!(
            "{name} is not a regular record file"
        )));
    }
    let limit = match name {
        RUN_FILE => MAX_RUN_BYTES,
        TRACE_FILE => MAX_TRACE_BYTES,
        PROGRAM_FILE => MAX_PROGRAM_BYTES,
        _ => MAX_BLOB_BYTES,
    };
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{name} exceeds the {limit}-byte record file limit"),
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn atomic_write_private_at(directory: &File, name: &str, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_RUN_BYTES {
        bail!("run.json exceeds the {MAX_RUN_BYTES}-byte record file limit");
    }
    for _ in 0..100 {
        let sequence = RECORD_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = format!(".{name}-{}-{sequence}.tmp", std::process::id());
        let mut file = match create_private_at(directory, &temporary) {
            Ok(file) => file,
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
                }) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            drop(file);
            crate::runtime::checkpoint::unlink_at(directory, std::ffi::OsStr::new(&temporary));
            return Err(error).context("writing record temporary file");
        }
        drop(file);
        crate::runtime::checkpoint::rename_at(
            directory,
            std::ffi::OsStr::new(&temporary),
            std::ffi::OsStr::new(name),
        )
        .context("publishing record file")?;
        directory.sync_all()?;
        return Ok(());
    }
    bail!("could not allocate a private record temporary file")
}

fn strings(value: &Json) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A short summary of a record for `show-run`, as text or as JSON. The audit
/// seal is verified against the per-user key from its usual location; a
/// missing key shows the audit unsealed rather than failing the read.
pub fn show_run(directory: &Path, as_json: bool) -> Result<String> {
    show_run_with_key(directory, as_json, SealKey::System)
}

/// [`show_run`] with an explicit seal key. Unit and integration tests pass
/// [`SealKey::Explicit`] so the seal check does not depend on ambient user
/// state; callers without a key pass [`SealKey::Unavailable`].
pub fn show_run_with_key(
    directory: &Path,
    as_json: bool,
    seal_key: SealKey<'_>,
) -> Result<String> {
    let record = StoredRecord::open(directory)?;
    let failed = record
        .steps
        .iter()
        .find(|step| step_failed(step))
        .map(step_summary);
    let mut slowest = record.steps.iter().collect::<Vec<_>>();
    slowest.sort_by(|left, right| {
        right
            .wall_us
            .cmp(&left.wall_us)
            .then(left.index.cmp(&right.index))
    });
    let slowest = slowest
        .into_iter()
        .take(5)
        .map(step_summary)
        .collect::<Vec<_>>();
    let ok = record.run["result"]["ok"].as_bool().unwrap_or(false);
    let decisions = record
        .run
        .get("decisions")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let decision_count = record
        .run
        .get("decision_count")
        .and_then(Json::as_u64)
        .unwrap_or(decisions.len() as u64);
    let decisions_truncated = record
        .run
        .get("decisions_truncated")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    // Verify before presenting. Anything short of verified is still shown,
    // labeled: unsealed or untrusted data with a warning is reviewable, while
    // refusing to show it would hide the record's own evidence of tampering.
    let audit = verify_decision_audit(&record.run, &seal_key);
    let audit_verified = matches!(audit, AuditVerification::Verified { .. });
    let audit_error: Json = audit
        .error()
        .map(|error| Json::String(error.to_owned()))
        .unwrap_or(Json::Null);
    if as_json {
        let document = json!({
            "protocol": "execute.show-run/1",
            "ok": ok,
            "exit_status": record.run["result"]["exit_status"],
            "error": record.run["result"]["error"],
            "record_id": record.run.get("record_id").cloned().unwrap_or(Json::Null),
            "program_sha256": record.run["program"]["sha256"],
            "plan_sha256": record.run["plan"]["sha256"],
            "steps": record.steps.len(),
            "finished": record.run["finished"],
            "truncated": record.run["truncated"],
            "failed_step": failed,
            "slowest_steps": slowest,
            "policy": record.run.get("policy").cloned().unwrap_or(Json::Null),
            "decisions": decisions,
            "decision_count": decision_count,
            "decisions_truncated": decisions_truncated,
            "audit_head": record.run.get("audit_head").cloned().unwrap_or(Json::Null),
            "audit_mac": record.run.get("audit_mac").cloned().unwrap_or(Json::Null),
            "audit_verified": audit_verified,
            "audit_error": audit_error,
            "audit_scope": "policy-decisions-only",
            "record_verified": false,
        });
        return Ok(serde_json::to_string_pretty(&document)? + "\n");
    }
    let mut text = String::new();
    text.push_str(&format!(
        "record {}\nresult: {}\n",
        directory.display(),
        if ok { "ok" } else { "failed" }
    ));
    // The seal covers the policy audit, the record identity and the policy
    // context only. Result, steps, trace and blobs are shown as recorded and
    // are never covered by `audit_verified`: a verified audit still leaves
    // whole-record tamper evidence to the record-integrity work (issue #414).
    text.push_str(
        "trust scope: the audit seal covers policy decisions, record identity and policy context only; result, steps, trace and blobs are shown as recorded\n",
    );
    if let Some(error) = record.run["result"]["error"].as_str() {
        text.push_str(&format!("error: {error}\n"));
    }
    text.push_str(&format!("steps: {}\n", record.steps.len()));
    match &failed {
        Some(step) => text.push_str(&format!(
            "failed step: #{} line {} {} status {}\n  outcome: {}\n  stderr tail: {}\n",
            step["index"],
            step["line"],
            step["applets"],
            step["status"],
            step["outcome"],
            step["stderr_tail"].as_str().unwrap_or("")
        )),
        None => text.push_str("failed step: none\n"),
    }
    text.push_str("slowest steps:\n");
    for step in &slowest {
        text.push_str(&format!(
            "  #{} line {} {} {} us\n",
            step["index"], step["line"], step["applets"], step["wall_us"]
        ));
    }
    text.push_str(&policy_summary_text(&record.run));
    // The audit warning comes before the decisions it qualifies: steps and
    // decisions printed first would read as fact on a tampered record.
    match &audit {
        AuditVerification::PreAudit => {
            text.push_str(
                "warning: this record predates the policy audit; no allow/deny log was kept\n",
            );
        }
        AuditVerification::UnchainedLegacy => {
            text.push_str(
                "warning: this decision audit predates chaining; edits after the run are not detectable (re-record for a verifiable audit)\n",
            );
        }
        AuditVerification::Unsealed { reason, .. } => {
            text.push_str(&format!(
                "warning: the policy audit is unsealed ({reason}); it is shown as recorded, but edits after the run may not be detectable\n",
            ));
        }
        AuditVerification::Untrusted { error, .. } => {
            text.push_str(&format!(
                "warning: the policy audit failed verification ({error}); the decisions below are untrusted\n",
            ));
        }
        AuditVerification::Verified { .. } => {}
    }
    text.push_str(&decisions_summary_text(
        &decisions,
        decision_count,
        decisions_truncated,
    ));
    if record.run["truncated"].as_bool().unwrap_or(false) {
        text.push_str(
            "note: the record reached a size limit; some trace or output data was not kept\n",
        );
    }
    if record.run["finished"] == false {
        text.push_str(
            "note: the recorded run did not finish; the trace may end at the last completed step\n",
        );
    }
    Ok(text)
}

/// One line naming the policy in force. Records written before the audit say so
/// instead of implying confinement that was never checked.
fn policy_summary_text(run: &Json) -> String {
    let Some(policy) = run.get("policy") else {
        return "policy: unknown (record predates policy audit)\n".to_owned();
    };
    let mode = policy
        .get("execution_mode")
        .and_then(Json::as_str)
        .unwrap_or("unknown");
    let (capability, capability_detail) = policy_identity_text(policy.get("capability_policy"));
    let (data_flow, data_flow_detail) = policy_identity_text(policy.get("data_flow_policy"));
    let mut line = format!(
        "policy: execution_mode={mode} capability_policy={capability}{capability_detail} data_flow_policy={data_flow}{data_flow_detail}"
    );
    if mode == "trusted" && capability == "absent" && data_flow == "absent" {
        line.push_str(" (no confinement: no policy was active)");
    }
    line.push('\n');
    line
}

/// The text form of one recorded policy identity: its status plus, when
/// configured, the version and content digest that name which policy ran.
/// Records written before identities carry a bare presence string; those read
/// back unchanged.
fn policy_identity_text(identity: Option<&Json>) -> (&str, String) {
    let Some(identity) = identity else {
        return ("unknown", String::new());
    };
    if let Some(status) = identity.as_str() {
        return (status, String::new());
    }
    let status = identity
        .get("status")
        .and_then(Json::as_str)
        .unwrap_or("unknown");
    if status != "configured" {
        return (status, String::new());
    }
    let version = identity
        .get("version")
        .and_then(Json::as_u64)
        .map(|version| version.to_string())
        .unwrap_or_else(|| "?".to_owned());
    let digest = identity
        .get("sha256")
        .and_then(Json::as_str)
        .unwrap_or("?");
    (status, format!(" version={version} sha256={digest}"))
}

/// The durable policy-decision audit for `show-run`. Decisions carry bounded
/// identifiers and canonical origins only, so they are safe to print.
fn decisions_summary_text(decisions: &[Json], total: u64, truncated: bool) -> String {
    if decisions.is_empty() && total == 0 {
        return "policy decisions: none recorded\n".to_owned();
    }
    let approved = decisions
        .iter()
        .filter(|decision| decision.get("outcome").and_then(Json::as_str) == Some("approved"))
        .count();
    let denied = decisions
        .iter()
        .filter(|decision| decision.get("outcome").and_then(Json::as_str) == Some("denied"))
        .count();
    let mut text = format!(
        "policy decisions: {} recorded ({} approved, {} denied)\n",
        decisions.len(),
        approved,
        denied
    );
    if truncated {
        text.push_str(&format!(
            "note: the audit kept the most recent {} of {total} decisions\n",
            decisions.len()
        ));
    }
    for decision in decisions {
        let sequence = decision
            .get("sequence")
            .and_then(Json::as_u64)
            .map(|sequence| sequence.to_string())
            .unwrap_or_else(|| "?".to_owned());
        let subject = decision
            .get("subject")
            .and_then(Json::as_str)
            .unwrap_or("unknown");
        let action = decision
            .get("action")
            .and_then(Json::as_str)
            .unwrap_or("unknown");
        let outcome = decision
            .get("outcome")
            .and_then(Json::as_str)
            .unwrap_or("unknown");
        let reason = decision
            .get("reason")
            .and_then(Json::as_str)
            .unwrap_or("unknown");
        let risk = decision
            .get("risk")
            .and_then(Json::as_str)
            .unwrap_or("unknown");
        let scope = decision
            .get("scope_origin")
            .and_then(Json::as_str)
            .map(|origin| format!(" scope={origin}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "  #{sequence} {subject} {action}{scope} {outcome} ({reason}) risk={risk}\n"
        ));
    }
    text
}

fn step_failed(step: &StoredStep) -> bool {
    step.failed
        || match &step.outcome {
            Some(outcome) => outcome["kind"] != "success",
            None => step.status != Some(0),
        }
}

fn step_summary(step: &StoredStep) -> Json {
    json!({
        "index": step.index,
        "line": step.line,
        "applets": step.applets,
        "status": step.status,
        "outcome": step.outcome.as_ref().map(|outcome| outcome["name"].clone()),
        "kind": step.outcome.as_ref().map(|outcome| outcome["kind"].clone()),
        "certainty": step.outcome.as_ref().map(|outcome| outcome["certainty"].clone()),
        "wall_us": step.wall_us,
        "stderr_tail": step.stderr["tail"].clone(),
    })
}

fn redact_json(redactor: &SecretRedactor, value: &Json) -> Json {
    match value {
        Json::String(text) => Json::String(redactor.redact_text(text)),
        Json::Array(items) => Json::Array(
            items
                .iter()
                .map(|item| redact_json(redactor, item))
                .collect(),
        ),
        Json::Object(fields) => Json::Object(
            fields
                .iter()
                .map(|(key, item)| (redactor.redact_text(key), redact_json(redactor, item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Input values for a record. A sensitive input (an environment value by default, or
/// a secret prompt answer) is withheld: the record keeps a marker in its place, so the
/// value is never written to disk.
pub fn inputs_json_for(
    inputs: &BTreeMap<String, crate::ast::Value>,
    declarations: &[crate::ast::InputDeclaration],
) -> Json {
    let secret: std::collections::BTreeSet<&str> = declarations
        .iter()
        .filter(|d| crate::ast::is_sensitive(d))
        .map(|d| d.name.as_str())
        .collect();
    let mut json = inputs_json(inputs);
    if let Json::Object(fields) = &mut json {
        for name in secret {
            if fields.contains_key(name) {
                fields.insert(name.to_owned(), serde_json::json!({ "withheld": "secret" }));
            }
        }
    }
    json
}

/// Input values as JSON for a record, from the run's named inputs.
pub fn inputs_json(inputs: &BTreeMap<String, crate::ast::Value>) -> Json {
    Json::Object(
        inputs
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    serde_json::to_value(value).unwrap_or(Json::Null),
                )
            })
            .collect(),
    )
}

fn tail(bytes: &[u8], limit: usize) -> Vec<u8> {
    let start = bytes.len().saturating_sub(limit);
    bytes[start..].to_vec()
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// Create a directory that only the owner can read, or confirm an empty one.
#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<File> {
    let (directory, _created) =
        crate::runtime::checkpoint::open_or_create_directory_tree(path, true, 0o700)?
            .context("creating record directory")?;
    if !crate::runtime::checkpoint::pinned_directory_names(&directory)?.is_empty() {
        bail!(
            "record directory {} is not empty; choose a new directory",
            path.display()
        );
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                bail!("{} exists and is not a real directory", path.display());
            }
            if fs::read_dir(path)?.next().is_some() {
                bail!(
                    "record directory {} is not empty; choose a new directory",
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(path)
                    .with_context(|| format!("creating record directory {}", path.display()))?;
            }
            #[cfg(not(unix))]
            fs::create_dir_all(path)
                .with_context(|| format!("creating record directory {}", path.display()))?;
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("checking record directory {}", path.display()))
        }
    }
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_private(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_RUN_BYTES {
        bail!("run.json exceeds the {MAX_RUN_BYTES}-byte record file limit");
    }
    let temporary = path.with_extension("json.tmp");
    write_private(&temporary, bytes)?;
    fs::rename(&temporary, path)
        .with_context(|| format!("replacing record file {}", path.display()))?;
    #[cfg(unix)]
    File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn record_start_validation_identity_rejects_parent_swap_before_pin() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let directory = parent.join("record");
        fs::create_dir_all(&directory).unwrap();
        let expected = create_private_directory(&directory)
            .unwrap()
            .metadata()
            .unwrap();
        let displaced = temp.path().join("displaced-parent");
        fs::rename(&parent, &displaced).unwrap();
        fs::create_dir_all(&directory).unwrap();

        assert!(
            crate::runtime::checkpoint::open_pinned_directory_expected(&directory, &expected,)
                .is_err()
        );
        assert!(displaced.join("record").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn record_file_publication_stays_in_pinned_directory_after_parent_swap() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let directory = parent.join("record");
        fs::create_dir_all(&directory).unwrap();
        let pinned = crate::runtime::checkpoint::open_pinned_directory(&directory).unwrap();
        let displaced = temp.path().join("displaced-parent");
        fs::rename(&parent, &displaced).unwrap();
        fs::create_dir_all(&directory).unwrap();

        atomic_write_private_at(&pinned, RUN_FILE, b"pinned record").unwrap();

        assert_eq!(
            fs::read(displaced.join("record").join(RUN_FILE)).unwrap(),
            b"pinned record"
        );
        assert!(!directory.join(RUN_FILE).exists());
    }

    #[test]
    fn long_plans_are_cut_on_a_character_boundary() {
        let text = "é".repeat(10);
        let cut = truncate_text(&text, 5);
        assert_eq!(cut, "éé");
    }

    #[test]
    fn tails_keep_the_end_of_stderr() {
        assert_eq!(tail(b"abcdef", 3), b"def".to_vec());
        assert_eq!(tail(b"ab", 3), b"ab".to_vec());
    }

    #[test]
    fn redaction_applies_to_keys_and_values() {
        let redactor = SecretRedactor::default();
        redactor.register(b"hunter2").unwrap();
        let value = json!({"hunter2": ["x hunter2 y"]});
        let redacted = redact_json(&redactor, &value);
        let text = redacted.to_string();
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn registering_a_secret_structured_value_covers_object_keys() {
        let registry = Registry::default();
        let key = "CANARY-PRIVATE-KEY-315";
        let value = crate::ast::Value::Record(BTreeMap::from([(
            key.to_owned(),
            crate::ast::Value::String("public value".into()),
        )]));
        security::register_sensitive_value(&registry, &value).unwrap();
        let serialized = json!({(key): "public value"});
        let redacted = redact_json(&security::secret_redactor(&registry), &serialized);
        assert!(!redacted.to_string().contains(key));
    }

    #[test]
    fn a_secret_straddling_the_plan_limit_never_survives_truncation() {
        let redactor = SecretRedactor::default();
        let secret = "SECRET-straddle-value-9731";
        redactor.register(secret.as_bytes()).unwrap();
        // Truncating first would keep "SECRET-str" as an unredactable fragment.
        let plan = format!("{}{secret}tail", "x".repeat(MAX_PLAN_BYTES - 10));
        let (stored, _) = prepare_plan(&redactor, &plan);
        assert!(stored.len() <= MAX_PLAN_BYTES);
        assert!(
            !stored.contains("SECRET"),
            "a fragment of the secret survived"
        );
        assert!(
            !stored.contains("straddle"),
            "a fragment of the secret survived"
        );
    }

    #[test]
    fn the_plan_hash_is_over_the_redacted_plan_not_the_raw_plan() {
        let redactor = SecretRedactor::default();
        let secret = "SECRET-confirmation-value-5521";
        redactor.register(secret.as_bytes()).unwrap();
        let plan = format!("run printf with message \"{secret}\"\n");
        let (stored, sha256) = prepare_plan(&redactor, &plan);
        assert_ne!(
            sha256,
            sha256_hex(plan.as_bytes()),
            "the raw plan's hash leaks a guess"
        );
        assert_eq!(sha256, sha256_hex(stored.as_bytes()));
    }

    #[test]
    fn persistence_policy_withholds_unknown_labels_and_secret_environment() {
        let registry = Registry::default();
        let _ = registry
            .set_host_state(security::DataFlowPolicy::from_json(br#"{"version":1}"#).unwrap());
        let public = SensitivitySet::from_label(crate::contracts::Sensitivity::Public);
        let unknown = SensitivitySet::from_label(crate::contracts::Sensitivity::Unknown);
        assert!(persistence_allowed(&registry, &public, false));
        assert!(!persistence_allowed(&registry, &unknown, false));
        assert!(
            !persistence_allowed(&registry, &public, true),
            "declared secret environment is never persisted"
        );
    }
}

#[cfg(test)]
mod blob_name_tests {
    use super::valid_blob_name;

    #[test]
    fn only_the_writers_blob_names_are_accepted() {
        let digest = "a".repeat(64);
        assert!(valid_blob_name(&format!("blobs/{digest}")));
        assert!(valid_blob_name(&format!(
            "blobs/{}",
            "0123456789abcdef".repeat(4)
        )));
        for name in [
            "",
            "blobs/",
            "blobs",
            "../blobs/x",
            "/etc/passwd",
            "blobs/../run.json",
            "blobs/../../etc/passwd",
            "blobs/A",
            "blobs/ABCDEF",
            "run.json",
        ] {
            assert!(!valid_blob_name(name), "{name:?} was accepted");
        }
        assert!(!valid_blob_name(&format!("blobs/{}", "a".repeat(63))));
        assert!(!valid_blob_name(&format!("blobs/{}", "a".repeat(65))));
        assert!(!valid_blob_name(&format!(
            "blobs/{}/{}",
            "a".repeat(32),
            "b".repeat(31)
        )));
    }
}

#[cfg(all(test, unix))]
mod no_follow_tests {
    #[test]
    fn record_reads_refuse_a_link_in_the_final_component() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("target.json"), b"secret").unwrap();
        symlink(
            directory.path().join("target.json"),
            directory.path().join("run.json"),
        )
        .unwrap();
        assert!(super::read_regular_no_follow(&directory.path().join("run.json")).is_err());
        std::fs::write(directory.path().join("plain.json"), b"ok").unwrap();
        assert_eq!(
            super::read_regular_no_follow(&directory.path().join("plain.json")).unwrap(),
            b"ok"
        );
    }
}

#[cfg(test)]
mod bounded_record_tests {
    use super::{read_regular_no_follow, RecordStart, Recorder, StoredRecord, MAX_TRACE_BYTES};
    use serde_json::json;

    #[test]
    fn record_reader_rejects_oversized_trace_with_a_size_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(super::TRACE_FILE);
        std::fs::write(&path, vec![b'x'; MAX_TRACE_BYTES + 1]).unwrap();
        let error = read_regular_no_follow(&path).unwrap_err();
        assert!(error.to_string().contains("record file limit"));
    }

    #[test]
    fn incomplete_record_opens_and_ignores_a_torn_final_trace_line() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(super::RUN_FILE),
            br#"{"protocol":"execute.record/1","finished":false,"steps":1}"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join(super::TRACE_FILE),
            b"{\"event\":\"step\",\"index\":0,\"line\":1,\"applets\":[],\"binaries\":[],\"argv\":[],\"status\":0,\"wall_us\":1,\"failed\":false}\n{\"event\":",
        )
        .unwrap();
        let record = StoredRecord::open(directory.path()).unwrap();
        assert_eq!(record.steps.len(), 1);
        assert_eq!(record.run["finished"], false);
        let summary = super::show_run(directory.path(), false).unwrap();
        assert!(summary.contains("recorded run did not finish"));
    }

    #[test]
    fn recorder_publishes_an_incomplete_run_document_at_start() {
        let directory = tempfile::tempdir().unwrap();
        let registry = crate::manifest::Registry::default();
        let recorder = Recorder::start(RecordStart {
            directory: directory.path(),
            program_source: b"run text.concat with parts []",
            plan: "plan",
            inputs: json!({}),
            registry: &registry,
            audit_key: None,
        })
        .unwrap();
        drop(recorder);
        let record = StoredRecord::open(directory.path()).unwrap();
        assert_eq!(record.run["finished"], false);
        assert_eq!(record.run["steps"], 0);
    }
}

#[cfg(test)]
mod policy_audit_tests {
    use super::{
        show_run, show_run_with_key, RecordStart, Recorder, RunSummary, SealKey, StoredRecord,
        MAX_AUDIT_DECISIONS, MAX_RUN_BYTES,
    };
    use crate::security::{self, PolicyDecisionOutcome, PolicyDecisionReason, PolicyRisk};
    use serde_json::{json, Value};

    /// The fixed seal key for audit tests, so the seal check never depends on
    /// ambient user state. Production code never embeds a key like this.
    static TEST_AUDIT_KEY: [u8; 32] = [0x51; 32];

    fn test_seal() -> SealKey<'static> {
        SealKey::Explicit(&TEST_AUDIT_KEY)
    }

    fn finished_recorder(directory: &tempfile::TempDir, registry: &crate::manifest::Registry) {
        let recorder = Recorder::start(RecordStart {
            directory: directory.path(),
            program_source: b"run text.concat with parts []",
            plan: "plan",
            inputs: json!({}),
            registry,
            audit_key: Some(TEST_AUDIT_KEY),
        })
        .unwrap();
        recorder
            .finish(&RunSummary {
                ok: true,
                exit_status: Some(0),
                error: None,
                output: Vec::new(),
            })
            .unwrap();
    }

    fn run_document(directory: &tempfile::TempDir) -> Value {
        let bytes = std::fs::read(directory.path().join(super::RUN_FILE)).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn no_policy_run_records_absent_confinement_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let registry = crate::manifest::Registry::default();
        finished_recorder(&directory, &registry);

        let document = run_document(&directory);
        assert_eq!(document["policy"]["execution_mode"], "trusted");
        for name in ["capability_policy", "data_flow_policy"] {
            assert_eq!(document["policy"][name]["status"], "absent", "{name}");
            assert_eq!(document["policy"][name]["version"], Value::Null, "{name}");
            assert_eq!(document["policy"][name]["sha256"], Value::Null, "{name}");
        }
        // The policy context names presence plus one-way digests, never
        // contents: no paths, grants, origins or other policy material may
        // reach the record.
        let policy = document["policy"].as_object().unwrap();
        assert_eq!(policy.len(), 3, "{policy:?}");
        assert!(policy["execution_mode"].is_string(), "{policy:?}");
        for name in ["capability_policy", "data_flow_policy"] {
            let identity = policy[name].as_object().unwrap();
            assert_eq!(identity.len(), 3, "{identity:?}");
            assert!(identity["status"].is_string(), "{identity:?}");
        }
        // The only decisions are the record's own persistence checks; the
        // baseline above is what states that no confinement applied.
        let decisions = document["decisions"].as_array().unwrap();
        assert!(!decisions.is_empty());
        assert_eq!(
            document["decision_count"].as_u64().unwrap(),
            decisions.len() as u64
        );
        assert_eq!(document["decisions_truncated"], false);
        // The baseline audit is chained: every kept decision links into the
        // stored head.
        assert!(
            document["audit_head"]
                .as_str()
                .is_some_and(|head| !head.is_empty()),
            "{document}"
        );
        for decision in decisions {
            assert!(
                decision["audit_hash"]
                    .as_str()
                    .is_some_and(|link| !link.is_empty()),
                "{decision}"
            );
        }

        let text = show_run_with_key(directory.path(), false, test_seal()).unwrap();
        assert!(
            text.contains("no confinement: no policy was active"),
            "{text}"
        );
        assert!(!text.contains("warning"), "{text}");

        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        assert_eq!(report["policy"]["execution_mode"], "trusted");
        assert_eq!(report["audit_verified"], true);
        assert_eq!(report["audit_error"], Value::Null);
        assert_eq!(report["audit_head"], document["audit_head"]);
        assert_eq!(report["audit_mac"], document["audit_mac"]);
        assert!(
            report["audit_mac"]
                .as_str()
                .is_some_and(|seal| seal.len() == 64),
            "{report}"
        );
    }

    #[test]
    fn denied_effect_decision_is_persisted_to_the_run_record() {
        let directory = tempfile::tempdir().unwrap();
        let registry = crate::manifest::Registry::default();
        let recorder = Recorder::start(RecordStart {
            directory: directory.path(),
            program_source: b"run text.concat with parts []",
            plan: "plan",
            inputs: json!({}),
            registry: &registry,
            audit_key: Some(TEST_AUDIT_KEY),
        })
        .unwrap();
        security::record_policy_decision(
            &registry,
            "audit.denied",
            "capability:network.connect",
            None,
            PolicyRisk::High,
            PolicyDecisionOutcome::Denied,
            PolicyDecisionReason::CapabilityNotGranted,
        );
        security::record_policy_decision(
            &registry,
            "audit.allowed",
            "effect:text.transform",
            None,
            PolicyRisk::Low,
            PolicyDecisionOutcome::Approved,
            PolicyDecisionReason::PolicyMatched,
        );
        recorder
            .finish(&RunSummary {
                ok: false,
                exit_status: Some(1),
                error: Some("capability policy denies network".to_owned()),
                output: Vec::new(),
            })
            .unwrap();

        let document = run_document(&directory);
        let decisions = document["decisions"].as_array().unwrap();
        let denied = decisions
            .iter()
            .find(|decision| decision["subject"] == "audit.denied")
            .expect("the denied effect is in the persisted log");
        assert_eq!(denied["outcome"], "denied");
        assert_eq!(denied["reason"], "capability_not_granted");
        assert_eq!(denied["action"], "capability:network.connect");
        assert_eq!(denied["risk"], "high");
        assert!(
            document["decision_count"].as_u64().unwrap() >= decisions.len() as u64,
            "{document}"
        );

        let text = show_run_with_key(directory.path(), false, test_seal()).unwrap();
        assert!(text.contains("1 denied"), "{text}");
        assert!(text.contains("audit.denied"), "{text}");
        assert!(text.contains("capability_not_granted"), "{text}");

        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        assert!(report["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|decision| {
                decision["subject"] == "audit.denied" && decision["outcome"] == "denied"
            }));
        // The persisted audit verifies: links, order, count and seal all check.
        assert_eq!(report["audit_verified"], true);
        assert_eq!(report["audit_error"], Value::Null);
    }

    #[test]
    fn persisted_audit_is_bounded_and_marks_truncation() {
        let directory = tempfile::tempdir().unwrap();
        let registry = crate::manifest::Registry::default();
        let recorder = Recorder::start(RecordStart {
            directory: directory.path(),
            program_source: b"run text.concat with parts []",
            plan: "plan",
            inputs: json!({}),
            registry: &registry,
            audit_key: Some(TEST_AUDIT_KEY),
        })
        .unwrap();
        for _ in 0..MAX_AUDIT_DECISIONS + 10 {
            security::record_policy_decision(
                &registry,
                "audit.noise",
                "effect:text.transform",
                None,
                PolicyRisk::Low,
                PolicyDecisionOutcome::Approved,
                PolicyDecisionReason::PolicyMatched,
            );
        }
        recorder
            .finish(&RunSummary {
                ok: true,
                exit_status: Some(0),
                error: None,
                output: Vec::new(),
            })
            .unwrap();

        let total = security::policy_decision_total(&registry);
        let document = run_document(&directory);
        let decisions = document["decisions"].as_array().unwrap();
        assert_eq!(decisions.len(), MAX_AUDIT_DECISIONS);
        assert_eq!(document["decision_count"].as_u64().unwrap(), total);
        assert_eq!(document["decisions_truncated"], true);
        // The tail is kept: the newest sequence numbers survive.
        let first = decisions.first().unwrap()["sequence"].as_u64().unwrap();
        let last = decisions.last().unwrap()["sequence"].as_u64().unwrap();
        assert!(last > first);
        assert_eq!(last, total);
        assert_eq!(first, total - MAX_AUDIT_DECISIONS as u64 + 1);
        // The bounded audit keeps run.json within its size limit.
        let bytes = std::fs::read(directory.path().join(super::RUN_FILE)).unwrap();
        assert!(bytes.len() < MAX_RUN_BYTES, "{}", bytes.len());

        let text = show_run_with_key(directory.path(), false, test_seal()).unwrap();
        assert!(text.contains("most recent"), "{text}");

        // A genuinely truncated audit still verifies: the total accounts for
        // the dropped prefix, and the seal covers the truncated accounting.
        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        assert_eq!(report["audit_verified"], true);
        assert_eq!(report["decision_count"], total);
    }

    #[test]
    fn decision_identifiers_cannot_carry_secret_text_into_the_record() {
        let directory = tempfile::tempdir().unwrap();
        let registry = crate::manifest::Registry::default();
        let canary = "CANARY-AUDIT-SECRET-99173";
        let recorder = Recorder::start(RecordStart {
            directory: directory.path(),
            program_source: b"run text.concat with parts []",
            plan: "plan",
            inputs: json!({}),
            registry: &registry,
            audit_key: Some(TEST_AUDIT_KEY),
        })
        .unwrap();
        security::record_policy_decision(
            &registry,
            &format!("{canary} with spaces"),
            &format!("{canary} action"),
            None,
            PolicyRisk::High,
            PolicyDecisionOutcome::Denied,
            PolicyDecisionReason::MissingGrant,
        );
        recorder
            .finish(&RunSummary {
                ok: false,
                exit_status: None,
                error: Some("denied".to_owned()),
                output: Vec::new(),
            })
            .unwrap();

        for (name, bytes) in [
            (
                super::RUN_FILE,
                std::fs::read(directory.path().join(super::RUN_FILE)).unwrap(),
            ),
            (
                super::TRACE_FILE,
                std::fs::read(directory.path().join(super::TRACE_FILE)).unwrap(),
            ),
        ] {
            assert!(
                !String::from_utf8_lossy(&bytes).contains(canary),
                "{name} carries the secret"
            );
        }
        let record = StoredRecord::open(directory.path()).unwrap();
        assert_eq!(record.run["decisions"][0]["subject"], "unknown");
        // Scrubbing happens before chaining, so the audit over the scrubbed
        // identifiers still verifies.
        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        assert_eq!(report["audit_verified"], true);
    }

    #[test]
    fn show_run_reads_records_written_before_the_audit() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(super::RUN_FILE),
            br#"{"protocol":"execute.record/1","finished":true,"steps":0,"result":{"ok":true}}"#,
        )
        .unwrap();
        std::fs::write(directory.path().join(super::TRACE_FILE), b"").unwrap();

        let text = show_run(directory.path(), false).unwrap();
        assert!(text.contains("record predates policy audit"), "{text}");
        assert!(text.contains("policy decisions: none recorded"), "{text}");

        let report: Value =
            serde_json::from_str(&show_run(directory.path(), true).unwrap()).unwrap();
        assert_eq!(report["policy"], Value::Null);
        assert_eq!(report["decisions"], json!([]));
        assert_eq!(report["decision_count"], 0);
        assert_eq!(report["decisions_truncated"], false);
        // Legacy states are unverified with a machine-readable reason, so JSON
        // consumers never see a false `audit_verified` next to a null error.
        assert_eq!(report["audit_verified"], false);
        assert!(
            report["audit_error"]
                .as_str()
                .is_some_and(|error| error.contains("predates the policy audit")),
            "{report}"
        );
    }

    /// A record shaped like the first bounded audit (decisions but no chain)
    /// stays readable, but `show-run` labels it unverified instead of implying
    /// checks that never ran.
    #[test]
    fn show_run_labels_an_unchained_audit_unverified() {
        let directory = tempfile::tempdir().unwrap();
        let registry = crate::manifest::Registry::default();
        finished_recorder(&directory, &registry);

        let mut document = run_document(&directory);
        for decision in document["decisions"].as_array_mut().unwrap() {
            decision.as_object_mut().unwrap().remove("audit_hash");
        }
        document.as_object_mut().unwrap().remove("audit_head");
        std::fs::write(
            directory.path().join(super::RUN_FILE),
            serde_json::to_string_pretty(&document).unwrap(),
        )
        .unwrap();

        // Without any key the legacy audit is still readable; the seal path is
        // irrelevant because there is no chain to seal.
        let text = show_run_with_key(directory.path(), false, SealKey::Unavailable).unwrap();
        assert!(text.contains("predates chaining"), "{text}");

        let report: Value =
            serde_json::from_str(
                &show_run_with_key(directory.path(), true, SealKey::Unavailable).unwrap(),
            )
            .unwrap();
        assert_eq!(report["audit_verified"], false);
        assert_eq!(report["audit_head"], Value::Null);
        assert!(
            report["audit_error"]
                .as_str()
                .is_some_and(|error| error.contains("predates chaining")),
            "{report}"
        );
        assert!(!report["decisions"].as_array().unwrap().is_empty());
    }

    /// Build a finished record with two caller decisions plus the record's
    /// own persistence checks, then hand back its parsed `run.json`.
    fn audited_document(directory: &tempfile::TempDir) -> Value {
        let registry = crate::manifest::Registry::default();
        let recorder = Recorder::start(RecordStart {
            directory: directory.path(),
            program_source: b"run text.concat with parts []",
            plan: "plan",
            inputs: json!({}),
            registry: &registry,
            audit_key: Some(TEST_AUDIT_KEY),
        })
        .unwrap();
        for (subject, outcome, reason) in [
            (
                "audit.first",
                PolicyDecisionOutcome::Approved,
                PolicyDecisionReason::PolicyMatched,
            ),
            (
                "audit.second",
                PolicyDecisionOutcome::Denied,
                PolicyDecisionReason::CapabilityNotGranted,
            ),
        ] {
            security::record_policy_decision(
                &registry,
                subject,
                "effect:text.transform",
                None,
                PolicyRisk::Low,
                outcome,
                reason,
            );
        }
        recorder
            .finish(&RunSummary {
                ok: true,
                exit_status: Some(0),
                error: None,
                output: Vec::new(),
            })
            .unwrap();
        run_document(directory)
    }

    fn rewrite_run_document(directory: &tempfile::TempDir, document: &Value) {
        std::fs::write(
            directory.path().join(super::RUN_FILE),
            serde_json::to_string_pretty(document).unwrap(),
        )
        .unwrap();
    }

    /// Show a record after tampering: warn-open means both renderings succeed
    /// and carry the failure label instead of hiding the audit.
    fn warn_open_output(directory: &tempfile::TempDir) -> (String, Value) {
        let text = show_run_with_key(directory.path(), false, test_seal()).unwrap();
        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        (text, report)
    }

    /// The warn-open contract every tamper shape must keep: the audit is still
    /// shown, it is labeled untrusted in both renderings, and the tampered
    /// content stays visible for review instead of being hidden.
    fn assert_untrusted_shown(text: &str, report: &Value, fragment: &str) {
        assert!(text.contains("warning"), "{text}");
        assert!(text.contains("untrusted"), "{text}");
        assert_eq!(report["audit_verified"], false);
        assert!(
            report["audit_error"]
                .as_str()
                .is_some_and(|error| error.contains(fragment)),
            "{report}"
        );
        assert!(
            !report["decisions"].as_array().unwrap().is_empty(),
            "a tampered audit must stay reviewable: {report}"
        );
    }

    #[test]
    fn edited_decision_breaks_its_chain_link() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        let flipped = if document["decisions"][0]["outcome"] == "approved" {
            "denied"
        } else {
            "approved"
        };
        document["decisions"][0]["outcome"] = json!(flipped);
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_untrusted_shown(&text, &report, "modified");
        // The edit itself stays visible instead of being hidden.
        assert_eq!(report["decisions"][0]["outcome"], flipped);
    }

    #[test]
    fn deleted_decision_leaves_a_sequence_gap() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        let kept = document["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 1)
            .map(|(_, decision)| decision.clone())
            .collect::<Vec<_>>();
        document["decisions"] = Value::Array(kept.clone());
        // Repair the accounting so the sequence gap is what fails.
        document["decision_count"] = json!(kept.len());
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_untrusted_shown(&text, &report, "removed");
    }

    #[test]
    fn dropped_suffix_with_a_fixed_total_still_fails_on_the_head() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        let mut kept = document["decisions"].as_array().unwrap().clone();
        kept.pop();
        document["decisions"] = Value::Array(kept.clone());
        // Repair the accounting so only the head still binds the dropped tail.
        document["decision_count"] = json!(kept.len());
        document["decisions_truncated"] = json!(false);
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_untrusted_shown(&text, &report, "head");
    }

    #[test]
    fn rewritten_head_is_shown_as_untrusted() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        document["audit_head"] = json!("0".repeat(64));
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_untrusted_shown(&text, &report, "head");
    }

    #[test]
    fn reordered_decisions_are_shown_as_untrusted() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        document["decisions"].as_array_mut().unwrap().swap(0, 1);
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_untrusted_shown(&text, &report, "reordered");
    }

    #[test]
    fn truncation_flag_that_disagrees_with_the_total_is_shown_as_untrusted() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        // More decisions are kept than the flag admits: flip the flag while
        // the total still exceeds the kept decisions.
        let kept_len = document["decisions"].as_array().unwrap().len();
        document["decision_count"] = json!(kept_len + 5);
        document["decisions_truncated"] = json!(false);
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_untrusted_shown(&text, &report, "truncation flag");
    }

    #[test]
    fn record_without_a_key_is_shown_unsealed() {
        let directory = tempfile::tempdir().unwrap();
        let registry = crate::manifest::Registry::default();
        let recorder = Recorder::start(RecordStart {
            directory: directory.path(),
            program_source: b"run text.concat with parts []",
            plan: "plan",
            inputs: json!({}),
            registry: &registry,
            audit_key: None,
        })
        .unwrap();
        recorder
            .finish(&RunSummary {
                ok: true,
                exit_status: Some(0),
                error: None,
                output: Vec::new(),
            })
            .unwrap();

        let document = run_document(&directory);
        assert_eq!(document["audit_mac"], Value::Null);
        let text = show_run_with_key(directory.path(), false, test_seal()).unwrap();
        assert!(text.contains("warning"), "{text}");
        assert!(text.contains("unsealed"), "{text}");
        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        assert_eq!(report["audit_verified"], false);
        assert!(
            report["audit_error"]
                .as_str()
                .is_some_and(|error| error.contains("no audit seal")),
            "{report}"
        );
        assert!(
            !report["decisions"].as_array().unwrap().is_empty(),
            "{report}"
        );
    }

    #[test]
    fn sealed_record_without_a_key_is_shown_unsealed() {
        let directory = tempfile::tempdir().unwrap();
        audited_document(&directory);

        let text = show_run_with_key(directory.path(), false, SealKey::Unavailable).unwrap();
        assert!(text.contains("unsealed"), "{text}");
        let report: Value = serde_json::from_str(
            &show_run_with_key(directory.path(), true, SealKey::Unavailable).unwrap(),
        )
        .unwrap();
        assert_eq!(report["audit_verified"], false);
        assert!(
            report["audit_error"]
                .as_str()
                .is_some_and(|error| error.contains("key is unavailable")),
            "{report}"
        );
    }

    #[test]
    fn seal_under_a_different_key_is_shown_as_untrusted() {
        let directory = tempfile::tempdir().unwrap();
        audited_document(&directory);
        let other_key = [0x33; 32];

        let text =
            show_run_with_key(directory.path(), false, SealKey::Explicit(&other_key)).unwrap();
        assert!(text.contains("untrusted"), "{text}");
        let report: Value = serde_json::from_str(
            &show_run_with_key(directory.path(), true, SealKey::Explicit(&other_key)).unwrap(),
        )
        .unwrap();
        assert_eq!(report["audit_verified"], false);
        assert!(
            report["audit_error"]
                .as_str()
                .is_some_and(|error| error.contains("seal")),
            "{report}"
        );
    }

    /// Copying one record's audit into another record must not verify: the
    /// seal binds the record identity and policy context, not just the total,
    /// the flag and the head.
    #[test]
    fn transplanted_audit_from_another_record_does_not_verify() {
        let source = tempfile::tempdir().unwrap();
        let source_document = audited_document(&source);

        // The target ran a different program, so its identity differs even
        // though the transplanted chain is internally consistent.
        let target = tempfile::tempdir().unwrap();
        let registry = crate::manifest::Registry::default();
        let recorder = Recorder::start(RecordStart {
            directory: target.path(),
            program_source: b"run text.concat with parts [\"other\"]",
            plan: "plan",
            inputs: json!({}),
            registry: &registry,
            audit_key: Some(TEST_AUDIT_KEY),
        })
        .unwrap();
        recorder
            .finish(&RunSummary {
                ok: true,
                exit_status: Some(0),
                error: None,
                output: Vec::new(),
            })
            .unwrap();
        let mut target_document = run_document(&target);
        for name in [
            "decisions",
            "audit_head",
            "decision_count",
            "decisions_truncated",
            "audit_mac",
        ] {
            target_document[name] = source_document[name].clone();
        }
        rewrite_run_document(&target, &target_document);

        let (text, report) = warn_open_output(&target);
        assert_untrusted_shown(&text, &report, "record identity");
    }

    /// Editing the recorded policy identity breaks the seal even though the
    /// chain over the decisions is untouched.
    #[test]
    fn edited_policy_identity_breaks_the_seal() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        document["policy"]["execution_mode"] = json!("untrusted");
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_untrusted_shown(&text, &report, "policy context");
    }

    /// Editing the recorded program hash breaks the seal even though the
    /// chain over the decisions is untouched.
    #[test]
    fn edited_program_identity_breaks_the_seal() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        document["program"]["sha256"] = json!("f".repeat(64));
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_untrusted_shown(&text, &report, "record identity");
    }

    /// A wrong-length explicit key is a caller error: the audit is reported
    /// untrusted, never silently unsealed.
    #[test]
    fn malformed_explicit_key_is_untrusted_not_unsealed() {
        let directory = tempfile::tempdir().unwrap();
        audited_document(&directory);
        let short = [0x51; 3];

        let text =
            show_run_with_key(directory.path(), false, SealKey::Explicit(&short)).unwrap();
        assert!(text.contains("untrusted"), "{text}");
        assert!(!text.contains("unsealed"), "{text}");

        let report: Value = serde_json::from_str(
            &show_run_with_key(directory.path(), true, SealKey::Explicit(&short)).unwrap(),
        )
        .unwrap();
        assert_eq!(report["audit_verified"], false);
        assert!(
            report["audit_error"]
                .as_str()
                .is_some_and(|error| error.contains("wrong length")),
            "{report}"
        );
    }

    /// Policy material — grants, paths, origins, environment allowlists —
    /// never reaches the record, even under a configured policy. The audit
    /// still verifies, bound to the recorded policy identity.
    #[test]
    fn configured_policy_material_never_reaches_the_record() {
        use crate::capabilities::{AppletGrant, CapabilityPolicy, FilesystemGrant};
        use std::collections::{BTreeMap, BTreeSet};

        let directory = tempfile::tempdir().unwrap();
        let registry = crate::manifest::Registry::default();
        let mut applets = BTreeMap::new();
        applets.insert(
            "canary-applet".to_owned(),
            AppletGrant {
                capabilities: BTreeSet::from([
                    "network.connect".to_owned(),
                    "filesystem.read".to_owned(),
                ]),
                binary_sha256: None,
                environment: BTreeSet::from(["CANARY_ENV_ALLOW".to_owned()]),
                filesystem: FilesystemGrant {
                    read: vec![std::path::PathBuf::from("/canary/grant-path")],
                    write: Vec::new(),
                },
                network: true,
                network_origins: BTreeSet::from(["https://canary-origin.example".to_owned()]),
                sandbox_required: false,
            },
        );
        registry.set_host_state(Some(CapabilityPolicy {
            version: crate::capabilities::CAPABILITY_POLICY_VERSION,
            applets,
            host: None,
            host_environment: BTreeSet::from(["CANARY_HOST_ENV".to_owned()]),
            deny_privileged: false,
            allow_wrappers: BTreeSet::new(),
        }));
        finished_recorder(&directory, &registry);

        let bytes = std::fs::read(directory.path().join(super::RUN_FILE)).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for forbidden in [
            "CANARY",
            "canary",
            "applets",
            "network_origins",
            "host_environment",
        ] {
            assert!(
                !text.contains(forbidden),
                "run.json carries policy material {forbidden:?}"
            );
        }
        let document: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(document["policy"]["execution_mode"], "untrusted");
        assert_eq!(document["policy"]["capability_policy"]["status"], "configured");

        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        assert_eq!(report["audit_verified"], true);
        assert_eq!(report["audit_error"], Value::Null);
    }

    /// A key holder can rewrite the audit and reseal it, and `show-run` cannot
    /// tell. This test pins that limitation: the seal raises the bar to key
    /// holders, it does not stop them. Whole-record tamper evidence belongs
    /// to the record-integrity work (issue #414), not to this seal.
    #[test]
    fn key_holder_rewrite_and_reseal_verifies() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        // Flip an outcome the way a forger would, then redo exactly what the
        // recorder does: relink the chain over the forged decisions and seal
        // the forged head under the same key.
        let flipped = if document["decisions"][0]["outcome"] == "approved" {
            "denied"
        } else {
            "approved"
        };
        document["decisions"][0]["outcome"] = json!(flipped);
        let total = document["decision_count"].as_u64().unwrap();
        let truncated = document["decisions_truncated"].as_bool().unwrap();
        let mut head = super::audit_genesis();
        for decision in document["decisions"].as_array_mut().unwrap() {
            head = super::audit_link(&head, decision);
            decision["audit_hash"] = json!(head.clone());
        }
        document["audit_head"] = json!(head.clone());
        // A key holder reforges exactly what the recorder seals: the forged
        // head bound to the same record identity and policy context.
        let seal = {
            let binding = super::SealBinding {
                record_id: document["record_id"]
                    .as_str()
                    .expect("a sealed record carries its identity"),
                program_sha256: document["program"]["sha256"]
                    .as_str()
                    .expect("a sealed record carries its program hash"),
                plan_sha256: document["plan"]["sha256"]
                    .as_str()
                    .expect("a sealed record carries its plan hash"),
                policy: &document["policy"],
            };
            super::audit_seal_hex(&TEST_AUDIT_KEY, &binding, total, truncated, &head)
        };
        document["audit_mac"] = json!(seal);
        rewrite_run_document(&directory, &document);

        let text = show_run_with_key(directory.path(), false, test_seal()).unwrap();
        assert!(!text.contains("warning"), "{text}");
        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        assert_eq!(report["audit_verified"], true);
        assert_eq!(report["decisions"][0]["outcome"], flipped);
    }

    /// Downgrade: stripping `record_id` and resealing under the retired v1
    /// encoding must never verify as v2. The audit stays readable but is
    /// explicitly weak, so a copied v1 audit cannot pass as a bound seal.
    #[test]
    fn stripped_record_id_with_v1_seal_is_legacy_weak_not_verified() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        let total = document["decision_count"].as_u64().unwrap();
        let truncated = document["decisions_truncated"].as_bool().unwrap();
        let head = document["audit_head"].as_str().unwrap().to_owned();
        document
            .as_object_mut()
            .unwrap()
            .remove("record_id");
        document["audit_mac"] =
            json!(super::audit_seal_hex_v1(&TEST_AUDIT_KEY, total, truncated, &head));
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_eq!(report["audit_verified"], false);
        assert!(
            report["audit_error"]
                .as_str()
                .is_some_and(|error| error.contains("retired v1")),
            "{report}"
        );
        assert!(!text.contains("untrusted"), "{text}");
        assert!(text.contains("unsealed"), "{text}");
        assert!(text.contains("could have been copied"), "{text}");
    }

    /// A legacy audit whose v1 seal does not match is tampering or a wrong
    /// key under the retired encoding — never a v2 record-identity failure.
    #[test]
    fn legacy_seal_mismatch_is_untrusted_without_record_identity() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        document
            .as_object_mut()
            .unwrap()
            .remove("record_id");
        document["audit_mac"] = json!("0".repeat(64));
        rewrite_run_document(&directory, &document);

        let (text, report) = warn_open_output(&directory);
        assert_untrusted_shown(&text, &report, "retired v1");
        assert!(
            !report["audit_error"]
                .as_str()
                .unwrap_or_default()
                .contains("record identity"),
            "{report}"
        );
    }

    /// Rotation and tampering stay indistinguishable under v2, and that limit
    /// is stated: a wrong key reports the rotation ambiguity, never the
    /// legacy-weak wording.
    #[test]
    fn wrong_key_reports_rotation_ambiguity_distinct_from_legacy() {
        let directory = tempfile::tempdir().unwrap();
        audited_document(&directory);
        let other_key = [0x44; 32];

        let report: Value = serde_json::from_str(
            &show_run_with_key(directory.path(), true, SealKey::Explicit(&other_key)).unwrap(),
        )
        .unwrap();
        assert_eq!(report["audit_verified"], false);
        let error = report["audit_error"].as_str().unwrap_or_default().to_owned();
        assert!(error.contains("different key"), "{report}");
        assert!(error.contains("indistinguishable"), "{report}");
        assert!(!error.contains("retired v1"), "{report}");
    }

    /// Displayed trust scope: verification covers policy decisions only.
    /// Both renderings name that scope, and editing the result does not move
    /// the audit — pinning the whole-record limitation (issue #414).
    #[test]
    fn verified_output_names_policy_decisions_only_scope() {
        let directory = tempfile::tempdir().unwrap();
        let document = audited_document(&directory);
        assert_eq!(document["audit_seal_version"], json!(2));

        let text = show_run_with_key(directory.path(), false, test_seal()).unwrap();
        assert!(text.contains("trust scope"), "{text}");
        assert!(text.contains("policy decisions"), "{text}");
        assert!(text.contains("shown as recorded"), "{text}");

        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        assert_eq!(report["audit_verified"], true);
        assert_eq!(report["audit_scope"], "policy-decisions-only");
        assert_eq!(report["record_verified"], false);
    }

    #[test]
    fn edited_result_keeps_audit_verified_with_scope_disclaimer() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = audited_document(&directory);
        document["result"]["ok"] = json!(false);
        document["result"]["error"] = json!("forged failure");
        rewrite_run_document(&directory, &document);

        let text = show_run_with_key(directory.path(), false, test_seal()).unwrap();
        assert!(text.contains("trust scope"), "{text}");
        let report: Value =
            serde_json::from_str(&show_run_with_key(directory.path(), true, test_seal()).unwrap())
                .unwrap();
        // The seal never covered the result: the audit still verifies while
        // the forged result is shown as recorded. Whole-record binding is
        // explicitly out of scope here (issue #414).
        assert_eq!(report["audit_verified"], true);
        assert_eq!(report["ok"], json!(false));
        assert_eq!(report["error"], json!("forged failure"));
        assert_eq!(report["audit_scope"], "policy-decisions-only");
    }
}
