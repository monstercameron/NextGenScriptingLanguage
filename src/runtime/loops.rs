//! Shared bounded loop frames. The hosts keep transport and cancellation policy.
use super::*;
use crate::ast::{InputType, LoopKind};

pub(super) const MAX_LOOP_ITERATIONS: usize = 100_000;

pub(super) fn repeat_count(value: &Value) -> Result<usize> {
    match value {
        Value::Integer(count) if *count >= 0 && *count <= MAX_LOOP_ITERATIONS as i64 => {
            Ok(*count as usize)
        }
        Value::Integer(_) => bail!("repeat count must be between 0 and {MAX_LOOP_ITERATIONS}"),
        _ => bail!("repeat count requires an integer"),
    }
}
fn list_items(value: &Value) -> Result<&[Value]> {
    let Value::List(items) = value else {
        bail!("for each requires a JSON array or list of text");
    };
    Ok(items)
}

pub(super) fn element_type(
    value: &Value,
    types: &BTreeMap<String, InputType>,
) -> Result<InputType> {
    let is_text_item = |item: &Value| match item {
        Value::String(text) => !text.contains('\0'),
        Value::Reference(name) => types.get(name).is_none_or(|kind| *kind == InputType::Text),
        _ => false,
    };
    match value {
        Value::Reference(name) => match types.get(name).copied() {
            Some(InputType::TextList) => Ok(InputType::Text),
            Some(InputType::Json) => Ok(InputType::Json),
            Some(kind) => bail!("for each requires a list or JSON array, {name:?} is {kind}"),
            // Untyped component bindings retain the historical text-list
            // interpretation. Declared JSON inputs opt into general values.
            None => Ok(InputType::Text),
        },
        Value::List(items) if items.iter().all(is_text_item) => Ok(InputType::Text),
        Value::List(_) => Ok(InputType::Json),
        _ => bail!("for each requires a JSON array or list of text"),
    }
}

/// Validate the header even when its body can never run.
pub(super) fn validate(kind: &LoopKind, flow: &control::FlowState) -> Result<()> {
    match kind {
        LoopKind::Repeat { count } => {
            check_value(count, &flow.names)?;
            match count {
                Value::Reference(name)
                    if flow
                        .types
                        .get(name)
                        .is_none_or(|kind| *kind == InputType::Integer) => {}
                Value::Reference(_) => bail!("repeat count requires an integer"),
                value => {
                    repeat_count(value)?;
                }
            }
        }
        LoopKind::ForEach { binding, values } => {
            if flow.names.contains(binding) {
                bail!("iteration binding {binding:?} is already defined");
            }
            check_value(values, &flow.names)?;
            element_type(values, &flow.types)?;
        }
        LoopKind::While { condition } => {
            control::validate_condition(condition, &flow.names, &flow.types)?
        }
    }
    Ok(())
}

/// Check known inputs before any native or filesystem effects. Unknown results
/// retain references and are checked when the loop is entered.
pub(super) fn preflight(kind: &LoopKind, values: &BTreeMap<String, Value>) -> Result<()> {
    match kind {
        LoopKind::Repeat { count } => {
            if !contains_reference(count) {
                repeat_count(count)?;
            } else if let Ok(value) = resolve(count, values) {
                repeat_count(&value)?;
            }
        }
        LoopKind::ForEach { values: source, .. } => {
            if !contains_reference(source) {
                list_items(source)?;
            } else if let Ok(value) = resolve(source, values) {
                list_items(&value)?;
            }
        }
        LoopKind::While { condition } => {
            let left = resolve(&condition.left, values).ok();
            let right = resolve(&condition.right, values).ok();
            if let (Some(left), Some(right)) = (left, right) {
                let resolved = crate::ast::Condition {
                    left,
                    right,
                    negated: condition.negated,
                    operator: condition.operator,
                };
                control::validate_condition(&resolved, &BTreeSet::new(), &BTreeMap::new())?;
                control::evaluate(&resolved, &BTreeMap::new())?;
            }
        }
    }
    Ok(())
}

enum Iterations {
    Repeat(usize),
    ForEach {
        binding: String,
        items: std::vec::IntoIter<Value>,
    },
    While,
}
struct Frame {
    header: usize,
    outer: BTreeSet<String>,
    iterations: Iterations,
}
#[derive(Default)]
pub(super) struct LoopStack {
    frames: Vec<Frame>,
    entered: usize,
}
impl LoopStack {
    pub fn header(
        &mut self,
        index: usize,
        kind: &LoopKind,
        bindings: &mut BTreeMap<String, Value>,
    ) -> Result<bool> {
        if self.frames.last().is_none_or(|frame| frame.header != index) {
            let iterations = match kind {
                LoopKind::Repeat { count } => {
                    Iterations::Repeat(repeat_count(&resolve(count, bindings)?)?)
                }
                LoopKind::ForEach { binding, values } => {
                    if bindings.contains_key(binding) {
                        bail!("iteration binding {binding:?} is already defined");
                    }
                    let values = resolve(values, bindings)?;
                    let Value::List(items) = values else {
                        bail!("for each requires a JSON array or list of text");
                    };
                    Iterations::ForEach {
                        binding: binding.clone(),
                        items: items.into_iter(),
                    }
                }
                LoopKind::While { .. } => Iterations::While,
            };
            self.frames.push(Frame {
                header: index,
                outer: bindings.keys().cloned().collect(),
                iterations,
            });
        }
        let frame = self.frames.last_mut().expect("loop frame was pushed");
        let active = match &mut frame.iterations {
            Iterations::Repeat(remaining) => {
                if *remaining == 0 {
                    false
                } else {
                    *remaining -= 1;
                    true
                }
            }
            Iterations::ForEach { binding, items } => {
                if let Some(item) = items.next() {
                    bindings.insert(binding.clone(), item);
                    true
                } else {
                    false
                }
            }
            Iterations::While => {
                let LoopKind::While { condition } = kind else {
                    bail!("loop frame kind mismatch");
                };
                control::evaluate(condition, bindings)?
            }
        };
        if !active {
            let frame = self.frames.pop().expect("active loop frame");
            bindings.retain(|name, _| frame.outer.contains(name));
            return Ok(false);
        }
        if self.entered >= MAX_LOOP_ITERATIONS {
            bail!("loop iteration budget of {MAX_LOOP_ITERATIONS} exceeded");
        }
        self.entered += 1;
        Ok(true)
    }
    pub fn end_iteration(
        &mut self,
        header: usize,
        bindings: &mut BTreeMap<String, Value>,
    ) -> Result<()> {
        let frame = self.frames.last().context("loop end without active loop")?;
        if frame.header != header {
            bail!("loop end does not match active loop");
        }
        bindings.retain(|name, _| frame.outer.contains(name));
        Ok(())
    }

    /// Remove nested loop frames crossed by a lexical transfer. Each popped
    /// frame restores the names that existed before that loop was entered.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn unwind_to(
        &mut self,
        depth: usize,
        bindings: &mut BTreeMap<String, Value>,
    ) -> Result<()> {
        if depth > self.frames.len() {
            bail!("control transfer targets an inactive loop scope");
        }
        while self.frames.len() > depth {
            let frame = self.frames.pop().expect("loop depth checked");
            bindings.retain(|name, _| frame.outer.contains(name));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_temporary_outer_type_drift_and_unknown_outer_writes() -> Result<()> {
        let registry = Registry::from_sources(&[] as &[&str])?;
        let program = crate::parser::parse(
            "accept flag as boolean\nrepeat 1 times\n  run \"true\" with arguments [] and keep status as flag\n",
        )?;
        assert!(format!(
            "{:#}",
            super::super::validate(&program, &registry).unwrap_err()
        )
        .contains("must remain boolean"));
        let unknown = crate::parser::parse(
            "repeat 1 times\n  run \"true\" with arguments [] and keep status as flag\n",
        )?;
        let error =
            validate_with_inputs(&unknown, &registry, BTreeSet::from(["flag".into()]), false)
                .unwrap_err();
        assert!(format!("{error:#}").contains("outer type is unknown"));
        // Reading unknown external values remains supported.
        let read =
            crate::parser::parse("repeat 1 times\n  run \"printf\" with arguments [flag]\n")?;
        validate_with_inputs(&read, &registry, BTreeSet::from(["flag".into()]), false)?;
        Ok(())
    }

    #[test]
    fn known_while_operands_are_checked_before_effects() {
        let kind = LoopKind::While {
            condition: crate::ast::Condition {
                left: Value::Reference("flag".into()),
                right: Value::Bool(true),
                negated: false,
                operator: crate::ast::ConditionOperator::Equals,
            },
        };
        assert!(preflight(
            &kind,
            &BTreeMap::from([("flag".into(), Value::String("true".into()))])
        )
        .is_err());
        assert!(preflight(
            &kind,
            &BTreeMap::from([("flag".into(), Value::List(vec![]))])
        )
        .is_err());
        assert!(preflight(
            &kind,
            &BTreeMap::from([("flag".into(), Value::Bool(false))])
        )
        .is_ok());
        assert!(preflight(&kind, &BTreeMap::new()).is_ok());
    }

    #[test]
    fn list_snapshot_and_iteration_scope_preserve_outer_updates() -> Result<()> {
        let mut bindings = BTreeMap::from([
            (
                "items".into(),
                Value::List(vec![Value::String("a".into()), Value::String("b".into())]),
            ),
            ("outer".into(), Value::Integer(1)),
        ]);
        let kind = LoopKind::ForEach {
            binding: "item".into(),
            values: Value::Reference("items".into()),
        };
        let mut stack = LoopStack::default();
        assert!(stack.header(3, &kind, &mut bindings)?);
        assert_eq!(bindings["item"], Value::String("a".into()));
        bindings.insert("items".into(), Value::List(vec![]));
        bindings.insert("outer".into(), Value::Integer(2));
        bindings.insert("local".into(), Value::Bool(true));
        stack.end_iteration(3, &mut bindings)?;
        assert!(!bindings.contains_key("local"));
        assert!(!bindings.contains_key("item"));
        assert!(stack.header(3, &kind, &mut bindings)?);
        assert_eq!(bindings["item"], Value::String("b".into()));
        stack.end_iteration(3, &mut bindings)?;
        assert!(!stack.header(3, &kind, &mut bindings)?);
        assert_eq!(bindings["outer"], Value::Integer(2));
        assert!(!bindings.contains_key("item"));
        Ok(())
    }

    #[test]
    fn nested_loops_share_entered_budget_without_charging_exhausted_headers() -> Result<()> {
        let one = LoopKind::Repeat {
            count: Value::Integer(1),
        };
        let zero = LoopKind::Repeat {
            count: Value::Integer(0),
        };
        let mut stack = LoopStack {
            entered: MAX_LOOP_ITERATIONS - 1,
            ..LoopStack::default()
        };
        let mut bindings = BTreeMap::new();
        assert!(stack.header(0, &one, &mut bindings)?);
        assert!(!stack.header(2, &zero, &mut bindings)?);
        assert!(stack
            .header(4, &one, &mut bindings)
            .unwrap_err()
            .to_string()
            .contains("iteration budget"));
        let mut stack = LoopStack {
            entered: MAX_LOOP_ITERATIONS - 1,
            ..LoopStack::default()
        };
        assert!(stack.header(0, &one, &mut bindings)?);
        stack.end_iteration(0, &mut bindings)?;
        assert!(!stack.header(0, &one, &mut bindings)?);
        Ok(())
    }

    #[test]
    fn while_reevaluates_updated_outer_binding_and_zero_repeat_has_no_binding_changes() -> Result<()>
    {
        let mut bindings = BTreeMap::from([("ready".into(), Value::Bool(false))]);
        let kind = LoopKind::While {
            condition: crate::ast::Condition {
                left: Value::Reference("ready".into()),
                right: Value::Bool(false),
                negated: false,
                operator: crate::ast::ConditionOperator::Equals,
            },
        };
        let mut stack = LoopStack::default();
        assert!(stack.header(0, &kind, &mut bindings)?);
        bindings.insert("ready".into(), Value::Bool(true));
        stack.end_iteration(0, &mut bindings)?;
        assert!(!stack.header(0, &kind, &mut bindings)?);
        let before = bindings.clone();
        assert!(!stack.header(
            3,
            &LoopKind::Repeat {
                count: Value::Integer(0)
            },
            &mut bindings
        )?);
        assert_eq!(before, bindings);
        Ok(())
    }
}
