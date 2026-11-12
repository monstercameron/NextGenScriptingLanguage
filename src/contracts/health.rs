//! Health probes, fault classes and recovery policy rows (issue #332).
//!
//! This module is declarative data plus one pure selector. Nothing here runs
//! a probe, calls an applet or writes a run record: the runtime loop does that
//! and feeds probe states and attempt counts into `select_recovery`.
//!
//! Fault codes and classes are the symbolic values a failure record already
//! carries: `code` is the sysexits-style name from `exit_status_code`, and
//! `class` is the record's `kind`. Tests keep both lists in step with the
//! runtime.

use super::{deserialize_present, valid_capability_id, valid_outcome_name, AppletContract, Effect};
use crate::contracts::EffectReversibilityStatus;
use crate::model_schema::recovery::ESCALATE;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Most health probes one applet or service contract may declare.
pub const MAX_HEALTH_PROBES: usize = 32;
/// Shortest and longest a probe may take, in milliseconds (one hour at most).
pub const MAX_PROBE_TIMEOUT_MS: u64 = 3_600_000;
/// Longest gap between probe runs, in milliseconds (one day at most).
pub const MAX_PROBE_INTERVAL_MS: u64 = 86_400_000;
/// Most recovery rows one policy may declare.
pub const MAX_RECOVERY_ROWS: usize = 64;
/// Most times one row may select its action within one incident.
pub const MAX_RECOVERY_RETRIES: u32 = 1_000;
/// Longest backoff between recovery attempts, in milliseconds (one day at most).
pub const MAX_RECOVERY_BACKOFF_MS: u64 = 86_400_000;
/// Longest JSON pointer a probe expectation may name, in bytes.
const MAX_POINTER_LEN: usize = 1024;

/// A typed health check on an applet or service. The runtime calls `applet`
/// with `arguments`, then checks the typed result against `expect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthProbe {
    /// Name a recovery row uses to refer to this probe.
    pub name: String,
    /// Namespaced applet to call.
    pub applet: String,
    /// Arguments for the call, as a JSON object.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub arguments: Map<String, Value>,
    /// What the typed result must show for the probe to pass.
    pub expect: ProbeExpectation,
    /// Longest the call may take.
    pub timeout_ms: u64,
    /// Gap between runs while the applet is being watched.
    pub interval_ms: u64,
}

/// The expectation language a probe may use. It is the same language the
/// grammar already has: a pointer's value equals a literal, or a pointer is
/// present. There is no arithmetic and no comparison beyond equality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbeExpectation {
    /// The value at `pointer` equals `value`, a string, number or boolean.
    Equals { pointer: String, value: Value },
    /// A value exists at `pointer`.
    Present { pointer: String },
}

impl ProbeExpectation {
    fn validate(&self, probe: &str) -> Result<()> {
        let (pointer, literal) = match self {
            Self::Equals { pointer, value } => (pointer, Some(value)),
            Self::Present { pointer } => (pointer, None),
        };
        if !valid_pointer(pointer) {
            bail!(
                "health probe {probe:?} must name its result with a JSON pointer such as /status"
            );
        }
        if let Some(value) = literal {
            if !matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
                bail!("health probe {probe:?} must compare with a string, number or boolean");
            }
        }
        Ok(())
    }
}

impl HealthProbe {
    fn validate(&self) -> Result<()> {
        if !valid_outcome_name(&self.name) {
            bail!(
                "health probe name {:?} must be a lowercase identifier",
                self.name
            );
        }
        if !valid_capability_id(&self.applet) {
            bail!(
                "health probe {:?} must call a namespaced applet name",
                self.name
            );
        }
        self.expect.validate(&self.name)?;
        if !(1..=MAX_PROBE_TIMEOUT_MS).contains(&self.timeout_ms) {
            bail!(
                "health probe {:?} timeout_ms must be between 1 and {MAX_PROBE_TIMEOUT_MS}",
                self.name
            );
        }
        if !(1..=MAX_PROBE_INTERVAL_MS).contains(&self.interval_ms) {
            bail!(
                "health probe {:?} interval_ms must be between 1 and {MAX_PROBE_INTERVAL_MS}",
                self.name
            );
        }
        Ok(())
    }
}

/// Validate a contract's probe list: bounded count, valid entries, unique names.
pub(crate) fn validate_probes(probes: &[HealthProbe]) -> Result<()> {
    if probes.len() > MAX_HEALTH_PROBES {
        bail!("an applet declares at most {MAX_HEALTH_PROBES} health probes");
    }
    let mut names = BTreeSet::new();
    for probe in probes {
        probe.validate()?;
        if !names.insert(probe.name.as_str()) {
            bail!("health probe names must be unique ({:?})", probe.name);
        }
    }
    Ok(())
}

/// RFC 6901 pointer syntax: empty (the whole result) or a sequence of
/// `/`-prefixed segments in which every `~` is `~0` or `~1`.
fn valid_pointer(pointer: &str) -> bool {
    if pointer.len() > MAX_POINTER_LEN {
        return false;
    }
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return false;
    }
    pointer
        .split('~')
        .skip(1)
        .all(|rest| matches!(rest.chars().next(), Some('0' | '1')))
}

/// Symbolic failure code, as a failure record's `code` field carries it.
/// The closed set is the exit-status mapping in `exit_status_code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultCode {
    Usage,
    DataError,
    NoInput,
    Unavailable,
    Software,
    TempFail,
    NoPermission,
    Config,
    NotExecutable,
    NotFound,
    Interrupted,
}

impl FaultCode {
    /// Every code, in the order of the exit-status table.
    pub const ALL: [FaultCode; 11] = [
        Self::Usage,
        Self::DataError,
        Self::NoInput,
        Self::Unavailable,
        Self::Software,
        Self::TempFail,
        Self::NoPermission,
        Self::Config,
        Self::NotExecutable,
        Self::NotFound,
        Self::Interrupted,
    ];

    /// The record string for this code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::DataError => "data_error",
            Self::NoInput => "no_input",
            Self::Unavailable => "unavailable",
            Self::Software => "software",
            Self::TempFail => "temp_fail",
            Self::NoPermission => "no_permission",
            Self::Config => "config",
            Self::NotExecutable => "not_executable",
            Self::NotFound => "not_found",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Failure class, as a failure record's `kind` field carries it. A class is
/// every non-success kind the record can hold; `success` and `absence` are
/// outcomes, not faults, so they have no class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultClass {
    Failure,
    Retryable,
    Timeout,
    Cancelled,
    Signaled,
    SpawnError,
    TransportError,
    ContractDecodeError,
    IoError,
    OutputLimit,
    CapacityError,
    ResourceExhaust,
    TerminalInputRequired,
    UnknownError,
}

impl FaultClass {
    /// Every class.
    pub const ALL: [FaultClass; 14] = [
        Self::Failure,
        Self::Retryable,
        Self::Timeout,
        Self::Cancelled,
        Self::Signaled,
        Self::SpawnError,
        Self::TransportError,
        Self::ContractDecodeError,
        Self::IoError,
        Self::OutputLimit,
        Self::CapacityError,
        Self::ResourceExhaust,
        Self::TerminalInputRequired,
        Self::UnknownError,
    ];

    /// The record string for this class.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Retryable => "retryable",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Signaled => "signaled",
            Self::SpawnError => "spawn_error",
            Self::TransportError => "transport_error",
            Self::ContractDecodeError => "contract_decode_error",
            Self::IoError => "io_error",
            Self::OutputLimit => "output_limit",
            Self::CapacityError => "capacity_error",
            Self::ResourceExhaust => "resource_exhaust",
            Self::TerminalInputRequired => "terminal_input_required",
            Self::UnknownError => "unknown_error",
        }
    }
}

/// One fault as the runtime sees it: its class and, when the process exited
/// with a listed status, its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fault {
    pub class: FaultClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<FaultCode>,
}

/// What a recovery row matches: exactly one of a code or a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultMatch {
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub code: Option<FaultCode>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub class: Option<FaultClass>,
}

impl FaultMatch {
    fn validate(&self, row: usize) -> Result<()> {
        match (self.code, self.class) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            _ => bail!("recovery row {row} must match exactly one of code or class"),
        }
    }

    fn matches(&self, fault: Fault) -> bool {
        match (self.code, self.class) {
            (Some(code), None) => fault.code == Some(code),
            (None, Some(class)) => fault.class == class,
            // Validation rejects the other shapes; an unvalidated row matches nothing.
            _ => false,
        }
    }
}

/// Whether a named health probe passed on its last run. A probe the runtime
/// has not run yet has no state, and a row that needs it failing does not match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeState {
    Passing,
    Failing,
}

/// What to do when no row applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unmatched {
    /// Stop the run with the fault unchanged.
    #[default]
    Fail,
    /// Hand the fault to a person.
    Escalate,
    /// Pass the policy's actions to the ranking core in `model_schema::recovery`.
    Rank,
}

/// One ordered row of a recovery policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRow {
    /// The fault this row matches.
    pub on: FaultMatch,
    /// A health probe that must be failing for the row to match.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub when_failing: Option<String>,
    /// The action name, or `escalate` to hand the fault to a person.
    pub action: String,
    /// The contract's declared effect the action performs. Required for every
    /// action except `escalate`, and it decides whether the action may run
    /// automatically.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub effect: Option<String>,
    /// How many times this row may select its action within one incident.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub retries: u32,
    /// Wait before each attempt, in milliseconds.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub backoff_ms: u64,
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Ordered recovery rows for one applet or service, with the `unmatched` setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPolicy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<RecoveryRow>,
    #[serde(default)]
    pub unmatched: Unmatched,
}

impl RecoveryPolicy {
    /// Validate the rows against the contract's probes and declared effects.
    pub(crate) fn validate(&self, probes: &[HealthProbe], effects: &[Effect]) -> Result<()> {
        if self.rows.len() > MAX_RECOVERY_ROWS {
            bail!("a recovery policy declares at most {MAX_RECOVERY_ROWS} rows");
        }
        // Each action name must always perform the same effect, so the rank
        // candidates keep one reversibility per name.
        let mut effect_of_action: BTreeMap<&str, &str> = BTreeMap::new();
        for (index, row) in self.rows.iter().enumerate() {
            row.on.validate(index)?;
            if let Some(probe) = row.when_failing.as_deref() {
                if !probes.iter().any(|declared| declared.name == probe) {
                    bail!("recovery row {index} names health probe {probe:?}, which the contract does not declare");
                }
            }
            if row.action == ESCALATE {
                if row.effect.is_some() || row.retries != 0 || row.backoff_ms != 0 {
                    bail!("escalate row {index} cannot declare an effect, retries or backoff");
                }
                continue;
            }
            if !valid_outcome_name(&row.action) {
                bail!(
                    "recovery row {index} action {:?} must be a lowercase identifier",
                    row.action
                );
            }
            let Some(effect) = row.effect.as_deref() else {
                bail!(
                    "recovery row {index} action {:?} must name the declared effect it performs",
                    row.action
                );
            };
            if !effects.iter().any(|declared| declared == effect) {
                bail!("recovery row {index} names effect {effect:?}, which the contract does not declare");
            }
            if !(1..=MAX_RECOVERY_RETRIES).contains(&row.retries) {
                bail!("recovery row {index} retries must be between 1 and {MAX_RECOVERY_RETRIES}");
            }
            if row.backoff_ms > MAX_RECOVERY_BACKOFF_MS {
                bail!("recovery row {index} backoff_ms must be at most {MAX_RECOVERY_BACKOFF_MS}");
            }
            if effect_of_action
                .insert(row.action.as_str(), effect)
                .is_some_and(|previous| previous != effect)
            {
                bail!(
                    "recovery action {:?} cannot perform two different effects",
                    row.action
                );
            }
        }
        Ok(())
    }
}

/// Why a step escalates instead of acting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationReason<'a> {
    /// The matching row is an `escalate` row.
    Declared { row: usize },
    /// The matching row's effect is irreversible, so it is never automatic.
    Irreversible {
        row: usize,
        action: &'a str,
        effect: &'a str,
    },
    /// The matching row's effect has no reversibility declaration, so it is
    /// never automatic either. `effect` is `None` only for an unvalidated row.
    Undeclared {
        row: usize,
        action: &'a str,
        effect: Option<&'a str>,
    },
    /// No row applied and `unmatched` is `escalate`.
    Unmatched,
}

/// One action the policy declares, with the reversibility of its effect. The
/// ranking core takes these: `eligible_actions` turns them into its label set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankCandidate<'a> {
    pub action: &'a str,
    pub reversibility: EffectReversibilityStatus<'a>,
}

/// The decision for one fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryStep<'a> {
    /// Run `action` from row `row`, after `backoff_ms`. `attempt` counts this
    /// selection, starting at 1.
    Act {
        row: usize,
        action: &'a str,
        backoff_ms: u64,
        attempt: u32,
    },
    /// Hand the fault to a person.
    Escalate(EscalationReason<'a>),
    /// Stop the run with the fault unchanged (`unmatched` is `fail`).
    Fail,
    /// Let the ranking core choose (`unmatched` is `rank`). Only actions with
    /// retries left are listed, once per action name, in row order.
    Rank { candidates: Vec<RankCandidate<'a>> },
}

/// Select the recovery step for `fault`.
///
/// Rows are read in order, and the first one that applies wins. A row applies
/// when its fault matches, when its `when_failing` probe (if any) is failing,
/// and when it is an `escalate` row or an action with retries left. An action
/// whose effect is irreversible or undeclared yields an escalation at once and
/// is never run automatically, even when its retries are used up. `attempts`
/// maps a row's index to the times that row has already acted in this
/// incident. When no row applies, `unmatched` decides.
pub fn select_recovery<'a>(
    contract: &'a AppletContract,
    policy: &'a RecoveryPolicy,
    fault: Fault,
    probe_states: &BTreeMap<String, ProbeState>,
    attempts: &BTreeMap<usize, u32>,
) -> RecoveryStep<'a> {
    for (index, row) in policy.rows.iter().enumerate() {
        if !row.on.matches(fault) {
            continue;
        }
        if let Some(probe) = row.when_failing.as_deref() {
            if probe_states.get(probe) != Some(&ProbeState::Failing) {
                continue;
            }
        }
        if row.action == ESCALATE {
            return RecoveryStep::Escalate(EscalationReason::Declared { row: index });
        }
        let action = row.action.as_str();
        let Some(effect) = row.effect.as_deref() else {
            return RecoveryStep::Escalate(EscalationReason::Undeclared {
                row: index,
                action,
                effect: None,
            });
        };
        match contract.effect_reversibility(effect) {
            EffectReversibilityStatus::Irreversible => {
                return RecoveryStep::Escalate(EscalationReason::Irreversible {
                    row: index,
                    action,
                    effect,
                });
            }
            EffectReversibilityStatus::Undeclared => {
                return RecoveryStep::Escalate(EscalationReason::Undeclared {
                    row: index,
                    action,
                    effect: Some(effect),
                });
            }
            EffectReversibilityStatus::Reversible { .. }
            | EffectReversibilityStatus::Confirmable { .. } => {}
        }
        let used = attempts.get(&index).copied().unwrap_or(0);
        if used >= row.retries {
            continue;
        }
        return RecoveryStep::Act {
            row: index,
            action,
            backoff_ms: row.backoff_ms,
            attempt: used.saturating_add(1),
        };
    }
    match policy.unmatched {
        Unmatched::Fail => RecoveryStep::Fail,
        Unmatched::Escalate => RecoveryStep::Escalate(EscalationReason::Unmatched),
        Unmatched::Rank => RecoveryStep::Rank {
            candidates: rank_candidates(contract, policy, attempts),
        },
    }
}

fn rank_candidates<'a>(
    contract: &'a AppletContract,
    policy: &'a RecoveryPolicy,
    attempts: &BTreeMap<usize, u32>,
) -> Vec<RankCandidate<'a>> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    for (index, row) in policy.rows.iter().enumerate() {
        let Some(effect) = row.effect.as_deref() else {
            continue;
        };
        if row.action == ESCALATE || attempts.get(&index).copied().unwrap_or(0) >= row.retries {
            continue;
        }
        if seen.insert(row.action.as_str()) {
            candidates.push(RankCandidate {
                action: row.action.as_str(),
                reversibility: contract.effect_reversibility(effect),
            });
        }
    }
    candidates
}

/// A declared probe that did not pass, with the evidence a doctor report shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeFailure {
    /// The probe's declared name.
    pub name: String,
    /// Why it did not pass, for example `/state is "down", expected "up"`.
    pub evidence: String,
}

impl ProbeExpectation {
    /// Check a typed probe result against this expectation. The error is the evidence.
    fn check(&self, result: &Value) -> std::result::Result<(), String> {
        match self {
            Self::Equals { pointer, value } => match result.pointer(pointer) {
                Some(found) if found == value => Ok(()),
                Some(found) => Err(format!("{pointer} is {found}, expected {value}")),
                None => Err(format!("{pointer} is missing")),
            },
            Self::Present { pointer } => match result.pointer(pointer) {
                Some(_) => Ok(()),
                None => Err(format!("{pointer} is missing")),
            },
        }
    }
}

/// Check each declared probe against its supplied typed result and return the
/// probes that did not pass, in declaration order. A probe with no result in
/// `results` fails with evidence saying so. Nothing here runs a probe.
pub fn failing_probes(
    probes: &[HealthProbe],
    results: &BTreeMap<String, Value>,
) -> Vec<ProbeFailure> {
    probes
        .iter()
        .filter_map(|probe| {
            let evidence = match results.get(&probe.name) {
                Some(result) => probe.expect.check(result).err()?,
                None => "no result was supplied".to_owned(),
            };
            Some(ProbeFailure {
                name: probe.name.clone(),
                evidence,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_schema::recovery::{eligible_actions, Candidate};
    use crate::runtime::execution_result::{exit_status_code, ExecutionKind};
    use serde_json::json;

    fn probe(name: &str) -> Value {
        json!({
            "name": name,
            "applet": "sys.probe",
            "arguments": {"target": "db"},
            "expect": {"equals": {"pointer": "/state", "value": "up"}},
            "timeout_ms": 5000,
            "interval_ms": 30000
        })
    }

    /// A contract with four effects: a reversible write, a confirmable
    /// restart, an irreversible drop, and a `sys.ping` with no reversibility entry.
    fn contract_with(health: Value, recovery: Value) -> Result<AppletContract> {
        let contract: AppletContract = serde_json::from_value(json!({
            "version": 1,
            "capabilities": ["sys.write", "sys.drop", "sys.restart", "sys.ping"],
            "effects": ["sys.write", "sys.drop", "sys.restart", "sys.ping"],
            "reversibility": {
                "sys.write": {"kind": "reversible", "compensate": "sys.undo_write"},
                "sys.restart": {"kind": "confirmable", "confirm_within_ms": 60000},
                "sys.drop": {"kind": "irreversible"}
            },
            "health": health,
            "recovery": recovery,
        }))?;
        contract.validate()?;
        Ok(contract)
    }

    fn standard_policy(unmatched: &str) -> Value {
        json!({
            "unmatched": unmatched,
            "rows": [
                {"on": {"code": "temp_fail"}, "when_failing": "db_up", "action": "restart",
                 "effect": "sys.restart", "retries": 2, "backoff_ms": 1000},
                {"on": {"class": "timeout"}, "action": "write_retry",
                 "effect": "sys.write", "retries": 1},
                {"on": {"class": "timeout"}, "action": "drop_retry",
                 "effect": "sys.drop", "retries": 3},
                {"on": {"code": "not_found"}, "action": "escalate"}
            ]
        })
    }

    fn standard_contract(unmatched: &str) -> AppletContract {
        contract_with(json!([probe("db_up")]), standard_policy(unmatched)).unwrap()
    }

    fn policy_of(contract: &AppletContract) -> &RecoveryPolicy {
        contract.recovery.as_ref().unwrap()
    }

    fn rejected(health: Value, recovery: Value) -> String {
        match contract_with(health, recovery) {
            Ok(_) => panic!("contract should have been refused"),
            Err(error) => error.to_string(),
        }
    }

    fn temp_fail() -> Fault {
        Fault {
            class: FaultClass::Failure,
            code: Some(FaultCode::TempFail),
        }
    }

    fn timeout() -> Fault {
        Fault {
            class: FaultClass::Timeout,
            code: None,
        }
    }

    fn failing(names: &[&str]) -> BTreeMap<String, ProbeState> {
        names
            .iter()
            .map(|name| ((*name).to_owned(), ProbeState::Failing))
            .collect()
    }

    #[test]
    fn probes_and_recovery_are_absent_by_default_and_not_serialized() {
        let contract: AppletContract = serde_json::from_value(json!({"version": 1})).unwrap();
        contract.validate().unwrap();
        assert!(contract.health.is_empty());
        assert!(contract.recovery.is_none());
        let serialized = serde_json::to_value(&contract).unwrap();
        assert!(serialized.get("health").is_none());
        assert!(serialized.get("recovery").is_none());
    }

    #[test]
    fn a_full_contract_round_trips_through_json() {
        let contract = standard_contract("rank");
        let serialized = serde_json::to_value(&contract).unwrap();
        let reparsed: AppletContract = serde_json::from_value(serialized).unwrap();
        assert_eq!(reparsed, contract);
    }

    #[test]
    fn probe_fields_survive_a_round_trip_with_their_expectation() {
        let parsed: HealthProbe = serde_json::from_value(probe("db_up")).unwrap();
        assert_eq!(parsed.arguments.get("target"), Some(&json!("db")));
        let present: HealthProbe = serde_json::from_value(json!({
            "name": "has_pid", "applet": "sys.probe",
            "expect": {"present": {"pointer": "/pid"}},
            "timeout_ms": 1, "interval_ms": 1
        }))
        .unwrap();
        assert!(present.arguments.is_empty());
        for probe in [parsed, present] {
            let serialized = serde_json::to_value(&probe).unwrap();
            let reparsed: HealthProbe = serde_json::from_value(serialized).unwrap();
            assert_eq!(reparsed, probe);
        }
    }

    #[test]
    fn a_valid_probe_declaration_is_accepted() {
        let contract = contract_with(
            json!([
                probe("db_up"),
                {"name": "has_pid", "applet": "sys.probe", "expect": {"present": {"pointer": ""}},
                 "timeout_ms": 3600000, "interval_ms": 86400000}
            ]),
            json!({"rows": []}),
        )
        .unwrap();
        assert_eq!(contract.health.len(), 2);
    }

    #[test]
    fn probe_arguments_must_be_a_json_object() {
        let mut bad = probe("db_up");
        bad["arguments"] = json!(["db"]);
        let result: Result<HealthProbe, _> = serde_json::from_value(bad);
        assert!(result.is_err());
    }

    #[test]
    fn probe_names_and_applets_are_validated() {
        let mut bad_name = probe("DB Up");
        assert!(rejected(json!([bad_name.clone()]), json!({})).contains("lowercase identifier"));
        bad_name = probe("db_up");
        bad_name["applet"] = json!("probe");
        assert!(rejected(json!([bad_name]), json!({})).contains("namespaced applet name"));
    }

    #[test]
    fn probe_names_must_be_unique_and_bounded() {
        let message = rejected(json!([probe("db_up"), probe("db_up")]), json!({}));
        assert!(message.contains("must be unique"), "{message}");
        let many: Vec<Value> = (0..=MAX_HEALTH_PROBES)
            .map(|index| probe(&format!("p{index}")))
            .collect();
        let message = rejected(Value::Array(many), json!({}));
        assert!(message.contains("at most"), "{message}");
    }

    #[test]
    fn probe_timing_has_generous_but_finite_bounds() {
        for (field, value) in [
            ("timeout_ms", 0),
            ("timeout_ms", MAX_PROBE_TIMEOUT_MS + 1),
            ("interval_ms", 0),
            ("interval_ms", MAX_PROBE_INTERVAL_MS + 1),
        ] {
            let mut bad = probe("db_up");
            bad[field] = json!(value);
            let message = rejected(json!([bad]), json!({}));
            assert!(message.contains(field), "{field}={value}: {message}");
        }
    }

    #[test]
    fn expectations_use_pointers_and_scalar_literals_only() {
        let mut no_slash = probe("db_up");
        no_slash["expect"] = json!({"equals": {"pointer": "state", "value": "up"}});
        assert!(rejected(json!([no_slash]), json!({})).contains("JSON pointer"));

        let mut bad_escape = probe("db_up");
        bad_escape["expect"] = json!({"present": {"pointer": "/a~2b"}});
        assert!(rejected(json!([bad_escape]), json!({})).contains("JSON pointer"));

        let mut null_literal = probe("db_up");
        null_literal["expect"] = json!({"equals": {"pointer": "/state", "value": null}});
        assert!(rejected(json!([null_literal]), json!({})).contains("string, number or boolean"));

        let mut array_literal = probe("db_up");
        array_literal["expect"] = json!({"equals": {"pointer": "/state", "value": [1]}});
        assert!(rejected(json!([array_literal]), json!({})).contains("string, number or boolean"));
    }

    #[test]
    fn expectations_reject_comparison_operators() {
        let result: Result<ProbeExpectation, _> =
            serde_json::from_value(json!({"greater_than": {"pointer": "/n", "value": 1}}));
        assert!(result.is_err());
        let result: Result<ProbeExpectation, _> =
            serde_json::from_value(json!({"equals": {"pointer": "/n", "value": 1, "op": "<"}}));
        assert!(result.is_err());
    }

    #[test]
    fn recovery_rows_round_trip_and_keep_their_order() {
        let policy: RecoveryPolicy = serde_json::from_value(standard_policy("escalate")).unwrap();
        let actions: Vec<&str> = policy.rows.iter().map(|row| row.action.as_str()).collect();
        assert_eq!(
            actions,
            ["restart", "write_retry", "drop_retry", "escalate"]
        );
        assert_eq!(policy.unmatched, Unmatched::Escalate);
        let reparsed: RecoveryPolicy =
            serde_json::from_value(serde_json::to_value(&policy).unwrap()).unwrap();
        assert_eq!(reparsed, policy);
        assert_eq!(
            serde_json::to_value(Unmatched::Rank).unwrap(),
            json!("rank")
        );
    }

    #[test]
    fn unmatched_defaults_to_fail() {
        let policy: RecoveryPolicy = serde_json::from_value(json!({})).unwrap();
        assert_eq!(policy.unmatched, Unmatched::Fail);
        assert!(policy.rows.is_empty());
    }

    #[test]
    fn closed_vocabularies_refuse_unknown_values() {
        let result: Result<FaultMatch, _> = serde_json::from_value(json!({"code": "temp_fail_x"}));
        assert!(result.is_err());
        let result: Result<FaultMatch, _> = serde_json::from_value(json!({"class": "crashed"}));
        assert!(result.is_err());
        let result: Result<FaultMatch, _> =
            serde_json::from_value(json!({"code": "temp_fail", "bogus": 1}));
        assert!(result.is_err());
        let result: Result<RecoveryPolicy, _> =
            serde_json::from_value(json!({"unmatched": "ignore"}));
        assert!(result.is_err());
    }

    #[test]
    fn a_row_matches_exactly_one_of_code_or_class() {
        let both = rejected(
            json!([probe("db_up")]),
            json!({"rows": [{"on": {"code": "temp_fail", "class": "timeout"},
                "action": "escalate"}]}),
        );
        assert!(both.contains("exactly one"), "{both}");
        let neither = rejected(
            json!([]),
            json!({"rows": [{"on": {}, "action": "escalate"}]}),
        );
        assert!(neither.contains("exactly one"), "{neither}");
    }

    #[test]
    fn a_row_may_only_wait_on_a_probe_the_contract_declares() {
        let message = rejected(
            json!([probe("db_up")]),
            json!({"rows": [{"on": {"class": "timeout"}, "when_failing": "cache_up",
                "action": "escalate"}]}),
        );
        assert!(message.contains("does not declare"), "{message}");
    }

    #[test]
    fn escalate_rows_take_no_effect_retries_or_backoff() {
        let message = rejected(
            json!([]),
            json!({"rows": [{"on": {"class": "timeout"}, "action": "escalate",
                "effect": "sys.write"}]}),
        );
        assert!(message.contains("escalate row 0"), "{message}");
    }

    #[test]
    fn an_action_must_name_a_declared_effect_and_some_retries() {
        let no_effect = rejected(
            json!([]),
            json!({"rows": [{"on": {"class": "timeout"}, "action": "retry", "retries": 1}]}),
        );
        assert!(
            no_effect.contains("must name the declared effect"),
            "{no_effect}"
        );

        let undeclared = rejected(
            json!([]),
            json!({"rows": [{"on": {"class": "timeout"}, "action": "retry",
                "effect": "sys.other", "retries": 1}]}),
        );
        assert!(undeclared.contains("does not declare"), "{undeclared}");

        let no_retries = rejected(
            json!([]),
            json!({"rows": [{"on": {"class": "timeout"}, "action": "retry",
                "effect": "sys.write"}]}),
        );
        assert!(
            no_retries.contains("retries must be between"),
            "{no_retries}"
        );

        let bad_name = rejected(
            json!([]),
            json!({"rows": [{"on": {"class": "timeout"}, "action": "Retry Now",
                "effect": "sys.write", "retries": 1}]}),
        );
        assert!(bad_name.contains("lowercase identifier"), "{bad_name}");
    }

    #[test]
    fn one_action_name_cannot_perform_two_effects() {
        let message = rejected(
            json!([]),
            json!({"rows": [
                {"on": {"class": "timeout"}, "action": "retry", "effect": "sys.write", "retries": 1},
                {"on": {"code": "temp_fail"}, "action": "retry", "effect": "sys.restart", "retries": 1}
            ]}),
        );
        assert!(message.contains("two different effects"), "{message}");
    }

    #[test]
    fn retries_and_backoff_have_generous_but_finite_bounds() {
        let too_many = rejected(
            json!([]),
            json!({"rows": [{"on": {"class": "timeout"}, "action": "retry",
                "effect": "sys.write", "retries": MAX_RECOVERY_RETRIES + 1}]}),
        );
        assert!(too_many.contains("retries must be between"), "{too_many}");
        let too_long = rejected(
            json!([]),
            json!({"rows": [{"on": {"class": "timeout"}, "action": "retry",
                "effect": "sys.write", "retries": 1, "backoff_ms": MAX_RECOVERY_BACKOFF_MS + 1}]}),
        );
        assert!(too_long.contains("backoff_ms"), "{too_long}");
    }

    #[test]
    fn first_matching_row_wins_and_probe_gates_are_read() {
        let contract = standard_contract("fail");
        let policy = policy_of(&contract);
        let step = select_recovery(
            &contract,
            policy,
            temp_fail(),
            &failing(&["db_up"]),
            &BTreeMap::new(),
        );
        assert_eq!(
            step,
            RecoveryStep::Act {
                row: 0,
                action: "restart",
                backoff_ms: 1000,
                attempt: 1
            }
        );
        // The probe is not failing, so row 0 does not apply, and nothing else
        // matches a temp_fail code.
        let passing = select_recovery(
            &contract,
            policy,
            temp_fail(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(passing, RecoveryStep::Fail);
    }

    #[test]
    fn rows_are_read_in_order_for_a_class_match() {
        let contract = standard_contract("fail");
        let policy = policy_of(&contract);
        // A timeout has no code, so row 0 does not match. Row 1 is the first
        // reversible class match.
        let step = select_recovery(
            &contract,
            policy,
            timeout(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(
            step,
            RecoveryStep::Act {
                row: 1,
                action: "write_retry",
                backoff_ms: 0,
                attempt: 1
            }
        );
    }

    #[test]
    fn an_exhausted_row_is_skipped_and_the_next_row_applies() {
        let contract = contract_with(
            json!([]),
            json!({"rows": [
                {"on": {"class": "timeout"}, "action": "first", "effect": "sys.write", "retries": 2},
                {"on": {"class": "timeout"}, "action": "second", "effect": "sys.write", "retries": 1}
            ]}),
        )
        .unwrap();
        let policy = policy_of(&contract);
        let mut attempts = BTreeMap::new();
        attempts.insert(0, 1);
        let step = select_recovery(&contract, policy, timeout(), &BTreeMap::new(), &attempts);
        assert_eq!(
            step,
            RecoveryStep::Act {
                row: 0,
                action: "first",
                backoff_ms: 0,
                attempt: 2
            }
        );
        attempts.insert(0, 2);
        let step = select_recovery(&contract, policy, timeout(), &BTreeMap::new(), &attempts);
        assert_eq!(
            step,
            RecoveryStep::Act {
                row: 1,
                action: "second",
                backoff_ms: 0,
                attempt: 1
            }
        );
        attempts.insert(1, 1);
        let step = select_recovery(&contract, policy, timeout(), &BTreeMap::new(), &attempts);
        assert_eq!(step, RecoveryStep::Fail);
    }

    #[test]
    fn an_irreversible_action_escalates_and_never_falls_through() {
        let contract = standard_contract("fail");
        let policy = policy_of(&contract);
        // Row 1 is reversible and used up, so the next match is the
        // irreversible drop row, which must escalate rather than run.
        let mut attempts = BTreeMap::new();
        attempts.insert(1, 1);
        let step = select_recovery(&contract, policy, timeout(), &BTreeMap::new(), &attempts);
        assert_eq!(
            step,
            RecoveryStep::Escalate(EscalationReason::Irreversible {
                row: 2,
                action: "drop_retry",
                effect: "sys.drop"
            })
        );
    }

    #[test]
    fn the_irreversible_guard_holds_even_with_retries_left_on_the_row() {
        let contract = standard_contract("fail");
        let policy = policy_of(&contract);
        // Row 2 has three retries and none used, yet it still escalates
        // because its effect is irreversible.
        let step = select_recovery(
            &contract,
            policy,
            Fault {
                class: FaultClass::Timeout,
                code: None,
            },
            &BTreeMap::new(),
            &BTreeMap::from([(1, 1)]),
        );
        assert!(matches!(
            step,
            RecoveryStep::Escalate(EscalationReason::Irreversible { row: 2, .. })
        ));
    }

    #[test]
    fn an_undeclared_effect_is_never_automatic_either() {
        let contract = contract_with(
            json!([]),
            json!({"rows": [{"on": {"class": "timeout"}, "action": "ping_retry",
                "effect": "sys.ping", "retries": 2}]}),
        )
        .unwrap();
        let step = select_recovery(
            &contract,
            policy_of(&contract),
            timeout(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(
            step,
            RecoveryStep::Escalate(EscalationReason::Undeclared {
                row: 0,
                action: "ping_retry",
                effect: Some("sys.ping")
            })
        );
    }

    #[test]
    fn a_confirmable_action_may_be_selected() {
        let contract = standard_contract("fail");
        let step = select_recovery(
            &contract,
            policy_of(&contract),
            temp_fail(),
            &failing(&["db_up"]),
            &BTreeMap::new(),
        );
        assert!(matches!(
            step,
            RecoveryStep::Act {
                action: "restart",
                ..
            }
        ));
    }

    #[test]
    fn an_escalate_row_declares_escalation_for_its_fault() {
        let contract = standard_contract("fail");
        let step = select_recovery(
            &contract,
            policy_of(&contract),
            Fault {
                class: FaultClass::Failure,
                code: Some(FaultCode::NotFound),
            },
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(
            step,
            RecoveryStep::Escalate(EscalationReason::Declared { row: 3 })
        );
    }

    #[test]
    fn no_match_follows_the_unmatched_setting() {
        let unmatched_fault = Fault {
            class: FaultClass::Failure,
            code: Some(FaultCode::Usage),
        };

        let contract = standard_contract("fail");
        let step = select_recovery(
            &contract,
            policy_of(&contract),
            unmatched_fault,
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(step, RecoveryStep::Fail);

        let contract = standard_contract("escalate");
        let step = select_recovery(
            &contract,
            policy_of(&contract),
            unmatched_fault,
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(step, RecoveryStep::Escalate(EscalationReason::Unmatched));
    }

    #[test]
    fn rank_hands_the_policy_actions_to_the_ranking_core() {
        let contract = standard_contract("rank");
        let policy = policy_of(&contract);
        let unmatched_fault = Fault {
            class: FaultClass::Failure,
            code: Some(FaultCode::Usage),
        };
        let step = select_recovery(
            &contract,
            policy,
            unmatched_fault,
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        let RecoveryStep::Rank { candidates } = &step else {
            panic!("expected a rank step, got {step:?}");
        };
        let names: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.action)
            .collect();
        assert_eq!(names, ["restart", "write_retry", "drop_retry"]);

        // The ranking core takes the same candidate shape. Irreversible
        // actions are dropped there, so the label set is what it may choose from.
        let as_core: Vec<Candidate<'_>> = candidates
            .iter()
            .map(|candidate| Candidate {
                name: candidate.action,
                reversibility: candidate.reversibility,
            })
            .collect();
        let eligible = eligible_actions(&as_core).unwrap();
        let labels: Vec<&str> = eligible.labels().iter().map(String::as_str).collect();
        assert_eq!(labels, ["restart", "write_retry", "escalate"]);
    }

    #[test]
    fn rank_leaves_out_exhausted_actions() {
        let contract = standard_contract("rank");
        let policy = policy_of(&contract);
        let unmatched_fault = Fault {
            class: FaultClass::Failure,
            code: Some(FaultCode::Usage),
        };
        let step = select_recovery(
            &contract,
            policy,
            unmatched_fault,
            &BTreeMap::new(),
            &BTreeMap::from([(1, 1), (0, 2)]),
        );
        let RecoveryStep::Rank { candidates } = &step else {
            panic!("expected a rank step, got {step:?}");
        };
        let names: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.action)
            .collect();
        assert_eq!(names, ["drop_retry"]);
    }

    #[test]
    fn fault_codes_are_the_exit_status_codes_the_runtime_names() {
        let from_runtime: BTreeSet<&str> = (0..=255).filter_map(exit_status_code).collect();
        let declared: BTreeSet<&str> = FaultCode::ALL.iter().map(|code| code.as_str()).collect();
        assert_eq!(from_runtime, declared);
        for code in FaultCode::ALL {
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
        }
    }

    #[test]
    fn fault_classes_are_the_failure_kinds_the_runtime_records() {
        fn class_of(kind: ExecutionKind) -> Option<FaultClass> {
            match kind {
                ExecutionKind::Success | ExecutionKind::Absence => None,
                ExecutionKind::Failure => Some(FaultClass::Failure),
                ExecutionKind::Retryable => Some(FaultClass::Retryable),
                ExecutionKind::Timeout => Some(FaultClass::Timeout),
                ExecutionKind::Cancelled => Some(FaultClass::Cancelled),
                ExecutionKind::Signaled => Some(FaultClass::Signaled),
                ExecutionKind::SpawnError => Some(FaultClass::SpawnError),
                ExecutionKind::TransportError => Some(FaultClass::TransportError),
                ExecutionKind::ContractDecodeError => Some(FaultClass::ContractDecodeError),
                ExecutionKind::IoError => Some(FaultClass::IoError),
                ExecutionKind::OutputLimit => Some(FaultClass::OutputLimit),
                ExecutionKind::CapacityError => Some(FaultClass::CapacityError),
                ExecutionKind::ResourceExhaust => Some(FaultClass::ResourceExhaust),
                ExecutionKind::TerminalInputRequired => Some(FaultClass::TerminalInputRequired),
                ExecutionKind::UnknownError => Some(FaultClass::UnknownError),
            }
        }
        let kinds = [
            ExecutionKind::Success,
            ExecutionKind::Absence,
            ExecutionKind::Failure,
            ExecutionKind::Retryable,
            ExecutionKind::Timeout,
            ExecutionKind::Cancelled,
            ExecutionKind::Signaled,
            ExecutionKind::SpawnError,
            ExecutionKind::TransportError,
            ExecutionKind::ContractDecodeError,
            ExecutionKind::IoError,
            ExecutionKind::OutputLimit,
            ExecutionKind::CapacityError,
            ExecutionKind::ResourceExhaust,
            ExecutionKind::TerminalInputRequired,
            ExecutionKind::UnknownError,
        ];
        let mapped: BTreeSet<&str> = kinds
            .iter()
            .filter_map(|kind| class_of(*kind))
            .map(|class| {
                assert_eq!(serde_json::to_value(class).unwrap(), json!(class.as_str()));
                class.as_str()
            })
            .collect();
        let declared: BTreeSet<&str> = FaultClass::ALL.iter().map(|class| class.as_str()).collect();
        assert_eq!(mapped, declared);
        for kind in kinds {
            if let Some(class) = class_of(kind) {
                assert_eq!(class.as_str(), kind.as_str());
            }
        }
    }

    #[test]
    fn a_fault_round_trips_and_omits_an_absent_code() {
        let fault = timeout();
        let serialized = serde_json::to_value(fault).unwrap();
        assert_eq!(serialized, json!({"class": "timeout"}));
        let reparsed: Fault = serde_json::from_value(serialized).unwrap();
        assert_eq!(reparsed, fault);
    }

    #[test]
    fn a_contract_probe_list_with_an_unknown_key_is_refused() {
        let mut bad = probe("db_up");
        bad["rollback"] = json!("sys.undo");
        let result: Result<HealthProbe, _> = serde_json::from_value(bad);
        assert!(result.is_err());
    }

    #[test]
    fn the_doctor_report_names_only_the_failing_probe_with_its_evidence() {
        let probes: Vec<HealthProbe> = vec![
            serde_json::from_value(probe("db_up")).unwrap(),
            serde_json::from_value(json!({
                "name": "queue_up",
                "applet": "sys.probe",
                "expect": {"present": {"pointer": "/depth"}},
                "timeout_ms": 5000,
                "interval_ms": 30000
            }))
            .unwrap(),
        ];
        let results = BTreeMap::from([
            ("db_up".to_owned(), json!({"state": "down"})),
            ("queue_up".to_owned(), json!({"depth": 3})),
        ]);
        assert_eq!(
            failing_probes(&probes, &results),
            vec![ProbeFailure {
                name: "db_up".to_owned(),
                evidence: "/state is \"down\", expected \"up\"".to_owned(),
            }]
        );
    }

    #[test]
    fn a_probe_with_no_supplied_result_is_reported_as_failing() {
        let probes: Vec<HealthProbe> = vec![serde_json::from_value(probe("db_up")).unwrap()];
        let failures = failing_probes(&probes, &BTreeMap::new());
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "db_up");
        assert_eq!(failures[0].evidence, "no result was supplied");
    }
}
