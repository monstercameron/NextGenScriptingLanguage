//! Desired-state reconciliation engine: observe, diff, apply only the
//! difference, then verify.
//!
//! `reconcile` is a pure sequencing function over operations the caller
//! injects as closures. Nothing here runs an applet, so every phase is tested
//! with fakes. The runtime decides how each operation maps to an applet call.

use serde_json::{Map, Value};
use std::fmt;

/// Deepest object nesting the structural diff descends into. Below this depth
/// an object is compared as one value, so the walk always terminates.
pub const MAX_DIFF_DEPTH: usize = 128;

/// Error text reported by an injected operation.
pub type OperationResult<T> = Result<T, String>;

/// Injected observe: returns the current state.
pub type ObserveFn<'a> = dyn FnMut() -> OperationResult<Value> + 'a;
/// Injected diff: `(desired, observed)` to the planned difference. A null,
/// empty object or empty array means there is nothing to apply.
pub type DiffFn<'a> = dyn FnMut(&Value, &Value) -> OperationResult<Value> + 'a;
/// Injected apply: receives only the difference, never the full desired state.
pub type ApplyFn<'a> = dyn FnMut(&Value) -> OperationResult<()> + 'a;
/// Injected verify: receives the desired state and returns the state observed
/// after apply.
pub type VerifyFn<'a> = dyn FnMut(&Value) -> OperationResult<Value> + 'a;

/// The operations a reconciler supplies. `diff` and `verify` are optional:
/// without `diff` the structural diff is used, and without `verify` the engine
/// observes again after apply and diffs that.
pub struct Operations<'a> {
    pub observe: Box<ObserveFn<'a>>,
    pub diff: Option<Box<DiffFn<'a>>>,
    pub apply: Box<ApplyFn<'a>>,
    pub verify: Option<Box<VerifyFn<'a>>>,
}

/// The engine phase in which an operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Observe,
    Diff,
    Apply,
    /// The verify operation, or the observe that stands in for it.
    Verify,
}

impl fmt::Display for Phase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Observe => "observe",
            Self::Diff => "diff",
            Self::Apply => "apply",
            Self::Verify => "verify",
        })
    }
}

/// The result of one reconciliation.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The observed state already matched the desired state. Apply was not called.
    Unchanged { observed: Value },
    /// Apply ran with `diff`, and the state checked afterwards matches desired.
    Changed { diff: Value, observed: Value },
    /// Apply ran, but the state checked afterwards still differs from desired.
    VerificationFailed {
        desired: Value,
        observed: Value,
        remaining_diff: Value,
    },
    /// An operation failed in `phase`. Apply is never retried.
    Failed { phase: Phase, message: String },
}

/// Run one reconciliation of `desired` against the injected operations.
///
/// Observe the current state. Compute the difference. An empty difference ends
/// the run with `Unchanged`. Otherwise call apply once with the difference, then
/// verify. The remaining difference after verify decides between `Changed` and
/// `VerificationFailed`.
pub fn reconcile(desired: &Value, mut ops: Operations<'_>) -> Outcome {
    let observed = match (ops.observe)() {
        Ok(observed) => observed,
        Err(message) => return failed(Phase::Observe, message),
    };
    let planned = match planned_change(desired, &observed, ops.diff.as_deref_mut()) {
        Ok(planned) => planned,
        Err(message) => return failed(Phase::Diff, message),
    };
    let Some(diff) = planned else {
        return Outcome::Unchanged { observed };
    };
    if let Err(message) = (ops.apply)(&diff) {
        return failed(Phase::Apply, message);
    }
    let after = match ops.verify.as_mut() {
        Some(verify) => verify(desired),
        None => (ops.observe)(),
    };
    let observed_after = match after {
        Ok(observed) => observed,
        Err(message) => return failed(Phase::Verify, message),
    };
    match planned_change(desired, &observed_after, ops.diff.as_deref_mut()) {
        Ok(None) => Outcome::Changed {
            diff,
            observed: observed_after,
        },
        Ok(Some(remaining_diff)) => Outcome::VerificationFailed {
            desired: desired.clone(),
            observed: observed_after,
            remaining_diff,
        },
        Err(message) => failed(Phase::Diff, message),
    }
}

fn failed(phase: Phase, message: String) -> Outcome {
    Outcome::Failed { phase, message }
}

/// The difference of `observed` from `desired`, or `None` when there is none.
/// The injected diff decides when present; otherwise the structural diff does.
fn planned_change(
    desired: &Value,
    observed: &Value,
    diff: Option<&mut DiffFn<'_>>,
) -> OperationResult<Option<Value>> {
    match diff {
        Some(diff) => {
            let value = diff(desired, observed)?;
            Ok((!is_empty_diff(&value)).then_some(value))
        }
        None => Ok(structural_diff(desired, observed)),
    }
}

fn is_empty_diff(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(fields) => fields.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// The structural difference of `observed` from `desired`, or `None` when none.
///
/// The walk is partial: only keys that `desired` mentions are compared, so
/// observed state the desired state does not mention is never a difference.
/// Objects are compared key by key in sorted order. Arrays and scalars are
/// compared as whole values. A difference carries the desired value at the
/// point where it differs. A key missing from `observed` contributes its whole
/// desired value.
pub fn structural_diff(desired: &Value, observed: &Value) -> Option<Value> {
    diff_at(desired, observed, 0)
}

fn diff_at(desired: &Value, observed: &Value, depth: usize) -> Option<Value> {
    if depth >= MAX_DIFF_DEPTH {
        return (desired != observed).then(|| desired.clone());
    }
    match (desired, observed) {
        (Value::Object(wanted), Value::Object(seen)) => {
            let mut entries: Vec<(&String, &Value)> = wanted.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            let mut changes = Map::new();
            for (key, wanted_value) in entries {
                let change = match seen.get(key) {
                    None => Some(wanted_value.clone()),
                    Some(seen_value) => diff_at(wanted_value, seen_value, depth + 1),
                };
                if let Some(change) = change {
                    changes.insert(key.clone(), change);
                }
            }
            if changes.is_empty() {
                None
            } else {
                Some(Value::Object(changes))
            }
        }
        _ => (desired != observed).then(|| desired.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    /// Records each call in order, so tests can assert sequencing and inputs.
    #[derive(Default)]
    struct Calls(RefCell<Vec<String>>);

    impl Calls {
        fn record(&self, entry: String) {
            self.0.borrow_mut().push(entry);
        }

        fn names(&self) -> Vec<String> {
            self.0.borrow().clone()
        }
    }

    /// Writes a diff into a state value: objects merge key by key, anything
    /// else replaces the state.
    fn apply_diff(state: &mut Value, diff: &Value) {
        match (state, diff) {
            (Value::Object(state), Value::Object(diff)) => {
                for (key, value) in diff {
                    apply_diff(state.entry(key.clone()).or_insert(Value::Null), value);
                }
            }
            (state, diff) => *state = diff.clone(),
        }
    }

    /// Fake operations over one shared state. Observe reads it, apply merges the
    /// diff into it, verify reads it again.
    fn fake<'a>(state: &'a RefCell<Value>, calls: &'a Calls) -> Operations<'a> {
        Operations {
            observe: Box::new(move || {
                calls.record("observe".into());
                Ok(state.borrow().clone())
            }),
            diff: None,
            apply: Box::new(move |diff: &Value| {
                calls.record(format!("apply {diff}"));
                apply_diff(&mut state.borrow_mut(), diff);
                Ok(())
            }),
            verify: Some(Box::new(move |_desired: &Value| {
                calls.record("verify".into());
                Ok(state.borrow().clone())
            })),
        }
    }

    #[test]
    fn converged_state_calls_only_observe() {
        let state = RefCell::new(json!({"mode": "on", "volume": 80}));
        let calls = Calls::default();
        let outcome = reconcile(&json!({"mode": "on"}), fake(&state, &calls));
        assert_eq!(
            outcome,
            Outcome::Unchanged {
                observed: json!({"mode": "on", "volume": 80})
            }
        );
        assert_eq!(calls.names(), vec!["observe"]);
    }

    #[test]
    fn difference_calls_observe_apply_verify_in_order_and_reports_the_diff() {
        let state = RefCell::new(json!({"mode": "off"}));
        let calls = Calls::default();
        let outcome = reconcile(&json!({"mode": "on"}), fake(&state, &calls));
        assert_eq!(
            outcome,
            Outcome::Changed {
                diff: json!({"mode": "on"}),
                observed: json!({"mode": "on"}),
            }
        );
        assert_eq!(
            calls.names(),
            vec!["observe", r#"apply {"mode":"on"}"#, "verify"]
        );
    }

    #[test]
    fn verification_failure_reports_desired_observed_and_remaining_diff() {
        let state = RefCell::new(json!({"mode": "off"}));
        let calls = Calls::default();
        let mut ops = fake(&state, &calls);
        // Apply reports success but changes nothing, so verify still differs.
        ops.apply = Box::new(|diff: &Value| {
            calls.record(format!("apply {diff}"));
            Ok(())
        });
        let outcome = reconcile(&json!({"mode": "on"}), ops);
        assert_eq!(
            outcome,
            Outcome::VerificationFailed {
                desired: json!({"mode": "on"}),
                observed: json!({"mode": "off"}),
                remaining_diff: json!({"mode": "on"}),
            }
        );
        assert_eq!(
            calls.names(),
            vec!["observe", r#"apply {"mode":"on"}"#, "verify"]
        );
    }

    #[test]
    fn observe_error_stops_before_diff_and_apply() {
        let calls = Calls::default();
        let ops = Operations {
            observe: Box::new(|| Err("observe broke".to_string())),
            diff: None,
            apply: Box::new(|_: &Value| {
                calls.record("apply".into());
                Ok(())
            }),
            verify: None,
        };
        assert_eq!(
            reconcile(&json!({"mode": "on"}), ops),
            Outcome::Failed {
                phase: Phase::Observe,
                message: "observe broke".into(),
            }
        );
        assert!(calls.names().is_empty());
    }

    #[test]
    fn diff_error_stops_before_apply() {
        let calls = Calls::default();
        let ops = Operations {
            observe: Box::new(|| Ok(json!({"mode": "off"}))),
            diff: Some(Box::new(|_: &Value, _: &Value| {
                Err("diff broke".to_string())
            })),
            apply: Box::new(|_: &Value| {
                calls.record("apply".into());
                Ok(())
            }),
            verify: None,
        };
        assert_eq!(
            reconcile(&json!({"mode": "on"}), ops),
            Outcome::Failed {
                phase: Phase::Diff,
                message: "diff broke".into(),
            }
        );
        assert!(calls.names().is_empty());
    }

    #[test]
    fn apply_error_is_reported_and_verify_is_not_called() {
        let calls = Calls::default();
        let state = RefCell::new(json!({"mode": "off"}));
        let mut ops = fake(&state, &calls);
        ops.apply = Box::new(|diff: &Value| {
            calls.record(format!("apply {diff}"));
            Err("apply broke".to_string())
        });
        assert_eq!(
            reconcile(&json!({"mode": "on"}), ops),
            Outcome::Failed {
                phase: Phase::Apply,
                message: "apply broke".into(),
            }
        );
        assert_eq!(
            calls.names(),
            vec!["observe".to_string(), r#"apply {"mode":"on"}"#.to_string()]
        );
    }

    #[test]
    fn verify_error_is_reported_after_a_single_apply() {
        let calls = Calls::default();
        let state = RefCell::new(json!({"mode": "off"}));
        let mut ops = fake(&state, &calls);
        ops.verify = Some(Box::new(|_: &Value| Err("verify broke".to_string())));
        assert_eq!(
            reconcile(&json!({"mode": "on"}), ops),
            Outcome::Failed {
                phase: Phase::Verify,
                message: "verify broke".into(),
            }
        );
        let names = calls.names();
        assert_eq!(
            names
                .iter()
                .filter(|name| name.starts_with("apply"))
                .count(),
            1
        );
    }

    #[test]
    fn without_verify_the_engine_observes_again_and_diffs() {
        let observations = Cell::new(0_u32);
        let calls = Calls::default();
        let ops = Operations {
            observe: Box::new(|| {
                observations.set(observations.get() + 1);
                calls.record("observe".into());
                if observations.get() == 1 {
                    Ok(json!({"mode": "off"}))
                } else {
                    Err("second observe broke".to_string())
                }
            }),
            diff: None,
            apply: Box::new(|diff: &Value| {
                calls.record(format!("apply {diff}"));
                Ok(())
            }),
            verify: None,
        };
        assert_eq!(
            reconcile(&json!({"mode": "on"}), ops),
            Outcome::Failed {
                phase: Phase::Verify,
                message: "second observe broke".into(),
            }
        );
        assert_eq!(
            calls.names(),
            vec!["observe", r#"apply {"mode":"on"}"#, "observe"]
        );
    }

    #[test]
    fn partial_desired_state_ignores_unmentioned_keys() {
        let state = RefCell::new(json!({"mode": "on", "volume": 80, "extra": [1, 2]}));
        let calls = Calls::default();
        let outcome = reconcile(&json!({"mode": "on"}), fake(&state, &calls));
        assert!(matches!(outcome, Outcome::Unchanged { .. }));
        assert_eq!(calls.names(), vec!["observe"]);

        let calls = Calls::default();
        let outcome = reconcile(&json!({"mode": "off"}), fake(&state, &calls));
        assert_eq!(
            outcome,
            Outcome::Changed {
                diff: json!({"mode": "off"}),
                observed: json!({"mode": "off", "volume": 80, "extra": [1, 2]}),
            }
        );
    }

    #[test]
    fn a_second_run_after_a_successful_apply_applies_nothing() {
        let state = RefCell::new(json!({"mode": "off"}));
        let calls = Calls::default();
        let desired = json!({"mode": "on"});
        let first = reconcile(&desired, fake(&state, &calls));
        assert!(matches!(first, Outcome::Changed { .. }));
        let second = reconcile(&desired, fake(&state, &calls));
        assert!(matches!(second, Outcome::Unchanged { .. }));
        assert_eq!(
            calls.names(),
            vec!["observe", r#"apply {"mode":"on"}"#, "verify", "observe",]
        );
    }

    #[test]
    fn empty_injected_diff_skips_apply_even_when_structure_differs() {
        let calls = Calls::default();
        let ops = Operations {
            observe: Box::new(|| Ok(json!({"mode": "off"}))),
            diff: Some(Box::new(|_: &Value, _: &Value| Ok(json!({})))),
            apply: Box::new(|_: &Value| {
                calls.record("apply".into());
                Ok(())
            }),
            verify: None,
        };
        assert_eq!(
            reconcile(&json!({"mode": "on"}), ops),
            Outcome::Unchanged {
                observed: json!({"mode": "off"})
            }
        );
        assert!(calls.names().is_empty());
    }

    #[test]
    fn injected_diff_is_what_apply_receives() {
        let calls = Calls::default();
        let ops = Operations {
            observe: Box::new(|| Ok(json!({"mode": "off"}))),
            // Reports a difference until the custom key is present.
            diff: Some(Box::new(|_: &Value, observed: &Value| {
                if observed.get("custom").is_some() {
                    Ok(json!({}))
                } else {
                    Ok(json!({"custom": true}))
                }
            })),
            apply: Box::new(|diff: &Value| {
                calls.record(format!("apply {diff}"));
                Ok(())
            }),
            verify: Some(Box::new(|_: &Value| Ok(json!({"custom": true})))),
        };
        assert_eq!(
            reconcile(&json!({"mode": "on"}), ops),
            Outcome::Changed {
                diff: json!({"custom": true}),
                observed: json!({"custom": true}),
            }
        );
        assert_eq!(calls.names(), vec![r#"apply {"custom":true}"#.to_string()]);
    }

    #[test]
    fn structural_diff_is_recursive_over_objects_and_whole_for_arrays() {
        assert_eq!(
            structural_diff(
                &json!({"a": {"b": 2, "d": 1}}),
                &json!({"a": {"b": 1, "c": 9}})
            ),
            Some(json!({"a": {"b": 2, "d": 1}}))
        );
        assert_eq!(
            structural_diff(&json!({"a": {"b": 1}}), &json!({"a": {"b": 1, "c": 2}})),
            None
        );
        assert_eq!(
            structural_diff(&json!({"list": [1, 2]}), &json!({"list": [1, 2, 3]})),
            Some(json!({"list": [1, 2]}))
        );
        assert_eq!(
            structural_diff(&json!({"list": [1, 2]}), &json!({"list": [2, 1]})),
            Some(json!({"list": [1, 2]}))
        );
        assert_eq!(
            structural_diff(&json!({"x": null}), &json!({})),
            Some(json!({"x": null}))
        );
        assert_eq!(
            structural_diff(&json!({"a": {"x": 1}}), &json!({"a": 5})),
            Some(json!({"a": {"x": 1}}))
        );
    }

    #[test]
    fn structural_diff_is_deterministic_with_sorted_keys() {
        let diff = structural_diff(&json!({"b": 2, "a": 1, "c": 3}), &json!({})).unwrap();
        assert_eq!(
            serde_json::to_string(&diff).unwrap(),
            r#"{"a":1,"b":2,"c":3}"#
        );
    }

    #[test]
    fn structural_diff_bounds_depth_without_losing_differences() {
        let nest = |leaf: i64| {
            let mut value = json!(leaf);
            for _ in 0..(MAX_DIFF_DEPTH + 10) {
                value = json!({ "k": value });
            }
            value
        };
        assert_eq!(structural_diff(&nest(1), &nest(1)), None);
        assert!(structural_diff(&nest(1), &nest(2)).is_some());
    }
}
