//! Decision core for faults that match no recovery policy row (issue #390).
//!
//! Everything here is pure. The caller supplies the candidate actions with
//! their reversibility status from `contracts`, the incident state, and a
//! classifier closure; no model is called and no I/O happens in this module.
//! The stderr text in an `EvidenceBundle` must already be redacted by the
//! redaction engine (`crate::redact`) before it reaches `EvidenceBundle::new`;
//! this module only caps its length and never redacts.

use crate::contracts::EffectReversibilityStatus;
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// The fallback action that is always eligible and never supplied by a caller.
pub(crate) const ESCALATE: &str = "escalate";
/// Confidence percent a model answer needs before its label is acted on.
pub(crate) const DEFAULT_MINIMUM_CONFIDENCE: u8 = 85;
/// Largest stderr tail kept in an evidence bundle, in bytes.
pub(crate) const STDERR_TAIL_LIMIT: usize = 2048;
/// Number of most recent event names kept in an evidence bundle.
pub(crate) const EVENT_LIMIT: usize = 20;
/// Failed ranked actions after which the next decision is `escalate`.
pub(crate) const FAILED_ACTION_LIMIT: u8 = 2;

/// One declared action for an applet or service, with its reversibility.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate<'a> {
    pub(crate) name: &'a str,
    pub(crate) reversibility: EffectReversibilityStatus<'a>,
}

/// The closed label set for one incident: eligible action names in sorted
/// order, with `escalate` always last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EligibleSet {
    labels: Vec<String>,
}

impl EligibleSet {
    pub(crate) fn labels(&self) -> &[String] {
        &self.labels
    }

    fn contains(&self, label: &str) -> bool {
        self.labels.iter().any(|candidate| candidate == label)
    }
}

/// Only reversible and confirmable actions are eligible. `Irreversible` and
/// `Undeclared` actions are dropped, and a caller-supplied `escalate` is an error.
pub(crate) fn eligible_actions(candidates: &[Candidate<'_>]) -> Result<EligibleSet> {
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        if candidate.name == ESCALATE {
            bail!("action name {ESCALATE:?} is reserved for the escalation fallback");
        }
        if !seen.insert(candidate.name) {
            bail!(
                "candidate action {:?} is declared more than once",
                candidate.name
            );
        }
    }
    let mut labels: Vec<String> = candidates
        .iter()
        .filter(|candidate| is_eligible(candidate.reversibility))
        .map(|candidate| candidate.name.to_owned())
        .collect();
    labels.sort();
    labels.push(ESCALATE.to_owned());
    Ok(EligibleSet { labels })
}

fn is_eligible(status: EffectReversibilityStatus<'_>) -> bool {
    matches!(
        status,
        EffectReversibilityStatus::Reversible { .. }
            | EffectReversibilityStatus::Confirmable { .. }
    )
}

/// Guard state for one incident. It allows at most one model decision, and
/// once `FAILED_ACTION_LIMIT` ranked actions have failed it forces `escalate`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IncidentState {
    decided: bool,
    failed_actions: u8,
}

impl IncidentState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_failed_action(&mut self) {
        self.failed_actions = self.failed_actions.saturating_add(1);
    }

    pub(crate) fn failed_actions(&self) -> u8 {
        self.failed_actions
    }

    pub(crate) fn has_decided(&self) -> bool {
        self.decided
    }
}

/// What the classifier returned: a label and a confidence percent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) label: String,
    pub(crate) confidence: u8,
}

/// Who produced the decision. `Policy` is a guard that ran without the
/// classifier; `Model` means the classifier was consulted, and its result
/// was then accepted or rejected by the threshold and label checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decider {
    Policy,
    Model,
}

/// Why the decision came out as it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    ModelChoice,
    BelowMinimum,
    OutOfSet,
    InvalidConfidence,
    TwoFailedActions,
    AlreadyDecided,
}

/// The recorded outcome of one decision. It holds no evidence text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) decided_by: Decider,
    pub(crate) labels: Vec<String>,
    pub(crate) choice: String,
    pub(crate) confidence: Option<u8>,
    pub(crate) reason: Reason,
}

/// Decide one incident. Guards run first and never call `classify`. A
/// consulted label is chosen only when it is in `eligible` and its confidence
/// is at least `minimum`, and otherwise the choice is `escalate`.
pub(crate) fn decide(
    state: &mut IncidentState,
    eligible: &EligibleSet,
    minimum: u8,
    classify: impl FnOnce(&[String]) -> Verdict,
) -> Decision {
    if state.failed_actions >= FAILED_ACTION_LIMIT {
        return guard_escalation(eligible, Reason::TwoFailedActions);
    }
    if state.decided {
        return guard_escalation(eligible, Reason::AlreadyDecided);
    }
    state.decided = true;

    let verdict = classify(eligible.labels());
    let (choice, reason) = if !eligible.contains(&verdict.label) {
        (ESCALATE.to_owned(), Reason::OutOfSet)
    } else if verdict.confidence > 100 {
        (ESCALATE.to_owned(), Reason::InvalidConfidence)
    } else if verdict.confidence < minimum {
        (ESCALATE.to_owned(), Reason::BelowMinimum)
    } else {
        (verdict.label, Reason::ModelChoice)
    };
    Decision {
        decided_by: Decider::Model,
        labels: eligible.labels().to_vec(),
        choice,
        confidence: Some(verdict.confidence),
        reason,
    }
}

fn guard_escalation(eligible: &EligibleSet, reason: Reason) -> Decision {
    Decision {
        decided_by: Decider::Policy,
        labels: eligible.labels().to_vec(),
        choice: ESCALATE.to_owned(),
        confidence: None,
        reason,
    }
}

/// One action already tried in the incident and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriedAction {
    pub(crate) action: String,
    pub(crate) outcome: String,
}

/// Typed evidence for the classifier. `stderr_tail` must already be redacted
/// by `crate::redact` before it is passed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvidenceBundle {
    pub(crate) fault_code: String,
    pub(crate) fault_class: String,
    pub(crate) stderr_tail: String,
    pub(crate) last_events: Vec<String>,
    pub(crate) tried: Vec<TriedAction>,
}

impl EvidenceBundle {
    /// Builds a bundle. The stderr keeps its final `STDERR_TAIL_LIMIT` bytes,
    /// with the cut moved forward to a char boundary, and the events keep
    /// their final `EVENT_LIMIT` names.
    pub(crate) fn new(
        fault_code: &str,
        fault_class: &str,
        redacted_stderr: &str,
        events: &[String],
        tried: Vec<TriedAction>,
    ) -> Self {
        let start = events.len().saturating_sub(EVENT_LIMIT);
        Self {
            fault_code: fault_code.to_owned(),
            fault_class: fault_class.to_owned(),
            stderr_tail: tail_within(redacted_stderr, STDERR_TAIL_LIMIT).to_owned(),
            last_events: events.get(start..).unwrap_or_default().to_vec(),
            tried,
        }
    }

    /// SHA-256 of a canonical encoding: a version tag, then each string and
    /// count as a length-prefixed field in fixed order. Equal bundles hash
    /// equally, and different field values cannot produce the same encoding.
    pub(crate) fn sha256(&self) -> String {
        let mut canonical = Vec::new();
        push_field(&mut canonical, "execute.recovery.evidence.v1");
        push_field(&mut canonical, &self.fault_code);
        push_field(&mut canonical, &self.fault_class);
        push_field(&mut canonical, &self.stderr_tail);
        push_count(&mut canonical, self.last_events.len());
        for event in &self.last_events {
            push_field(&mut canonical, event);
        }
        push_count(&mut canonical, self.tried.len());
        for attempt in &self.tried {
            push_field(&mut canonical, &attempt.action);
            push_field(&mut canonical, &attempt.outcome);
        }
        let mut hash = Sha256::new();
        hash.update(&canonical);
        format!("{:x}", hash.finalize())
    }
}

fn push_field(buffer: &mut Vec<u8>, value: &str) {
    push_count(buffer, value.len());
    buffer.extend_from_slice(value.as_bytes());
}

fn push_count(buffer: &mut Vec<u8>, count: usize) {
    buffer.extend_from_slice(&(count as u64).to_be_bytes());
}

/// The longest suffix of `text` that is at most `limit` bytes and starts on a
/// char boundary.
fn tail_within(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.get(start..).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn reversible(name: &str) -> Candidate<'_> {
        Candidate {
            name,
            reversibility: EffectReversibilityStatus::Reversible {
                compensate: "service.undo",
            },
        }
    }

    fn set(names: &[&str]) -> EligibleSet {
        let candidates: Vec<Candidate<'_>> = names.iter().map(|name| reversible(name)).collect();
        eligible_actions(&candidates).expect("fixture set is valid")
    }

    fn verdict(label: &str, confidence: u8) -> Verdict {
        Verdict {
            label: label.to_owned(),
            confidence,
        }
    }

    #[test]
    fn eligibility_keeps_only_reversible_and_confirmable_kinds() {
        let candidates = [
            reversible("restart_bus"),
            Candidate {
                name: "pause_device",
                reversibility: EffectReversibilityStatus::Confirmable {
                    confirm_within_ms: 5000,
                },
            },
            Candidate {
                name: "wipe_cache",
                reversibility: EffectReversibilityStatus::Irreversible,
            },
            Candidate {
                name: "mystery",
                reversibility: EffectReversibilityStatus::Undeclared,
            },
        ];
        let eligible = eligible_actions(&candidates).expect("valid candidates");
        assert_eq!(
            eligible.labels(),
            ["pause_device", "restart_bus", "escalate"]
        );
    }

    #[test]
    fn escalate_is_always_present_and_reserved() {
        let empty = eligible_actions(&[]).expect("empty is valid");
        assert_eq!(empty.labels(), ["escalate"]);

        let refused = eligible_actions(&[reversible("escalate")]);
        assert!(refused.is_err(), "caller-supplied escalate must be refused");
    }

    #[test]
    fn duplicate_candidate_names_are_refused() {
        let duplicated = eligible_actions(&[reversible("restart_bus"), reversible("restart_bus")]);
        assert!(duplicated.is_err());
    }

    #[test]
    fn confident_in_set_answer_is_chosen() {
        let eligible = set(&["restart_bus", "restart_dep"]);
        let mut state = IncidentState::new();
        let decision = decide(&mut state, &eligible, 85, |_: &[String]| {
            verdict("restart_dep", 90)
        });
        assert_eq!(decision.choice, "restart_dep");
        assert_eq!(decision.decided_by, Decider::Model);
        assert_eq!(decision.reason, Reason::ModelChoice);
        assert_eq!(decision.confidence, Some(90));
        assert!(state.has_decided());
    }

    #[test]
    fn answer_at_default_threshold_is_chosen_and_below_escalates() {
        let eligible = set(&["restart_dep"]);
        let mut at = IncidentState::new();
        let chosen = decide(
            &mut at,
            &eligible,
            DEFAULT_MINIMUM_CONFIDENCE,
            |_: &[String]| verdict("restart_dep", DEFAULT_MINIMUM_CONFIDENCE),
        );
        assert_eq!(chosen.choice, "restart_dep");

        let mut below = IncidentState::new();
        let escalated = decide(
            &mut below,
            &eligible,
            DEFAULT_MINIMUM_CONFIDENCE,
            |_: &[String]| verdict("restart_dep", DEFAULT_MINIMUM_CONFIDENCE - 1),
        );
        assert_eq!(escalated.choice, ESCALATE);
        assert_eq!(escalated.reason, Reason::BelowMinimum);
        assert_eq!(escalated.decided_by, Decider::Model);
    }

    #[test]
    fn label_outside_the_closed_set_escalates_with_reason() {
        let eligible = set(&["restart_dep"]);
        let mut state = IncidentState::new();
        let decision = decide(&mut state, &eligible, 85, |_: &[String]| {
            verdict("rm_everything", 99)
        });
        assert_eq!(decision.choice, ESCALATE);
        assert_eq!(decision.reason, Reason::OutOfSet);
        assert_eq!(decision.labels, ["restart_dep", "escalate"]);
    }

    #[test]
    fn confidence_above_one_hundred_escalates() {
        let eligible = set(&["restart_dep"]);
        let mut state = IncidentState::new();
        let decision = decide(&mut state, &eligible, 85, |_: &[String]| {
            verdict("restart_dep", 200)
        });
        assert_eq!(decision.choice, ESCALATE);
        assert_eq!(decision.reason, Reason::InvalidConfidence);
    }

    #[test]
    fn second_decision_in_one_incident_does_not_call_classifier() {
        let eligible = set(&["restart_dep"]);
        let calls = Cell::new(0u32);
        let mut state = IncidentState::new();

        let first = decide(&mut state, &eligible, 85, |_: &[String]| {
            calls.set(calls.get() + 1);
            verdict("restart_dep", 95)
        });
        assert_eq!(first.choice, "restart_dep");
        assert_eq!(calls.get(), 1);

        let second = decide(&mut state, &eligible, 85, |_: &[String]| {
            calls.set(calls.get() + 1);
            verdict("restart_dep", 95)
        });
        assert_eq!(calls.get(), 1, "classifier must not run a second time");
        assert_eq!(second.choice, ESCALATE);
        assert_eq!(second.decided_by, Decider::Policy);
        assert_eq!(second.reason, Reason::AlreadyDecided);
        assert_eq!(second.confidence, None);
    }

    #[test]
    fn two_failed_actions_force_escalate_without_classifier() {
        let eligible = set(&["restart_dep"]);
        let calls = Cell::new(0u32);

        let mut one_failure = IncidentState::new();
        one_failure.record_failed_action();
        let still_ranked = decide(&mut one_failure, &eligible, 85, |_: &[String]| {
            calls.set(calls.get() + 1);
            verdict("restart_dep", 95)
        });
        assert_eq!(still_ranked.choice, "restart_dep");
        assert_eq!(calls.get(), 1);

        let mut two_failures = IncidentState::new();
        two_failures.record_failed_action();
        two_failures.record_failed_action();
        assert_eq!(two_failures.failed_actions(), 2);
        let forced = decide(&mut two_failures, &eligible, 85, |_: &[String]| {
            calls.set(calls.get() + 1);
            verdict("restart_dep", 95)
        });
        assert_eq!(calls.get(), 1, "classifier must not run after two failures");
        assert_eq!(forced.choice, ESCALATE);
        assert_eq!(forced.decided_by, Decider::Policy);
        assert_eq!(forced.reason, Reason::TwoFailedActions);
    }

    #[test]
    fn stderr_tail_cap_never_splits_a_multibyte_character() {
        // Each euro sign is three bytes, so a 2048-byte suffix would start mid-character.
        let stderr = "\u{20ac}".repeat(1000);
        let tail = tail_within(&stderr, STDERR_TAIL_LIMIT);
        assert!(tail.len() <= STDERR_TAIL_LIMIT);
        assert_eq!(tail.len(), 2046);
        assert!(tail.chars().all(|c| c == '\u{20ac}'));

        let short = "fine";
        assert_eq!(tail_within(short, STDERR_TAIL_LIMIT), short);
    }

    #[test]
    fn evidence_keeps_only_the_last_event_names() {
        let events: Vec<String> = (0..25).map(|index| format!("event.{index}")).collect();
        let bundle = EvidenceBundle::new("exit.1", "dependency_down", "", &events, Vec::new());
        assert_eq!(bundle.last_events.len(), EVENT_LIMIT);
        assert_eq!(
            bundle.last_events.first().map(String::as_str),
            Some("event.5")
        );
        assert_eq!(
            bundle.last_events.last().map(String::as_str),
            Some("event.24")
        );
    }

    fn sample_bundle() -> EvidenceBundle {
        EvidenceBundle::new(
            "exit.1",
            "dependency_down",
            "connection refused",
            &["start".to_owned(), "probe_failed".to_owned()],
            vec![TriedAction {
                action: "restart_dep".to_owned(),
                outcome: "failed".to_owned(),
            }],
        )
    }

    #[test]
    fn equal_bundles_hash_equally_and_a_changed_field_changes_the_hash() {
        let original = sample_bundle();
        assert_eq!(original.sha256(), sample_bundle().sha256());
        assert_eq!(original.sha256().len(), 64);

        let mut changed_code = sample_bundle();
        changed_code.fault_code = "exit.2".to_owned();
        assert_ne!(original.sha256(), changed_code.sha256());

        let mut changed_outcome = sample_bundle();
        changed_outcome.tried[0].outcome = "succeeded".to_owned();
        assert_ne!(original.sha256(), changed_outcome.sha256());

        let mut changed_event = sample_bundle();
        changed_event.last_events[0] = "start_again".to_owned();
        assert_ne!(original.sha256(), changed_event.sha256());

        let mut changed_stderr = sample_bundle();
        changed_stderr.stderr_tail.push('!');
        assert_ne!(original.sha256(), changed_stderr.sha256());
    }

    #[test]
    fn length_prefixes_keep_field_boundaries_unambiguous() {
        let mut joined = sample_bundle();
        joined.fault_code = "exit.1dependency".to_owned();
        joined.fault_class = "_down".to_owned();
        assert_ne!(sample_bundle().sha256(), joined.sha256());
    }
}
