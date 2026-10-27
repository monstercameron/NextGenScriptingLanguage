//! Shared bounded loop frames. The hosts keep transport and cancellation policy.
use super::*;
use crate::ast::{InputType, LoopKind};

#[cfg(not(target_arch = "wasm32"))]
pub(super) const MAX_LOOP_ITERATIONS: usize = 1_000_000;
#[cfg(target_arch = "wasm32")]
pub(super) const MAX_LOOP_ITERATIONS: usize = 100_000;

/// A resolved `for each` input is not an array or a text list.
#[derive(Debug, Default)]
pub struct ForEachInputTypeError {
    /// Source line when the invalid value was found during validation.
    pub line: Option<usize>,
}

pub(super) fn at_line(mut error: anyhow::Error, line: usize) -> anyhow::Error {
    if let Some(error) = error.downcast_mut::<ForEachInputTypeError>() {
        error.line = Some(line);
    }
    error
}

impl std::fmt::Display for ForEachInputTypeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("for each requires a JSON array or list of text")
    }
}
impl std::error::Error for ForEachInputTypeError {}

pub(super) fn repeat_count(value: &Value) -> Result<usize> {
    match value {
        Value::Integer(count) if *count >= 0 => usize::try_from(*count)
            .map_err(|_| anyhow!("repeat count is too large for this platform")),
        Value::Integer(_) => bail!("repeat count must be non-negative"),
        _ => bail!("repeat count requires an integer"),
    }
}

fn repeat_count_with_limit(value: &Value, limit: usize) -> Result<usize> {
    let count = repeat_count(value)?;
    if count > limit {
        bail!("loop iteration budget of {limit} exceeded");
    }
    Ok(count)
}
fn list_items(value: &Value) -> Result<&[Value]> {
    let Value::List(items) = value else {
        return Err(ForEachInputTypeError::default().into());
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
            Some(InputType::IntegerList) => Ok(InputType::Integer),
            Some(InputType::JsonList) => Ok(InputType::Json),
            Some(InputType::Json) => Ok(InputType::Json),
            Some(kind) => bail!("for each requires a list or JSON array, {name:?} is {kind}"),
            // Untyped component bindings retain the historical text-list
            // interpretation. Declared JSON inputs opt into general values.
            None => Ok(InputType::Text),
        },
        Value::List(items) if items.iter().all(is_text_item) => Ok(InputType::Text),
        Value::List(items) if items.iter().all(|item| matches!(item, Value::Integer(_))) => {
            Ok(InputType::Integer)
        }
        Value::List(_) => Ok(InputType::Json),
        _ => Err(ForEachInputTypeError::default().into()),
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
        // The command's applet and arguments are validated by the caller, which
        // owns the registry; a command loop has no values to check here.
        LoopKind::WhileCommand { .. } => {}
    }
    Ok(())
}

/// Check known inputs before any native or filesystem effects. Unknown results
/// retain references and are checked when the loop is entered.
pub(super) fn preflight(
    kind: &LoopKind,
    values: &BTreeMap<String, Value>,
    limit: Option<usize>,
) -> Result<()> {
    match kind {
        LoopKind::Repeat { count } => {
            let known = if !contains_reference(count) {
                Some(count.clone())
            } else {
                resolve(count, values).ok()
            };
            if let Some(value) = known {
                if let Some(limit) = limit {
                    repeat_count_with_limit(&value, limit)?;
                } else {
                    repeat_count(&value)?;
                }
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
        LoopKind::WhileCommand { .. } => {}
    }
    Ok(())
}

enum Iterations {
    Repeat(usize),
    ForEach {
        binding: String,
        items: std::vec::IntoIter<Value>,
        /// Items yielded so far, including those restored from a checkpoint.
        taken: usize,
        /// The item the loop body is running for, kept for element records.
        current: Option<Value>,
    },
    While,
}
struct Frame {
    header: usize,
    outer: BTreeSet<String>,
    iterations: Iterations,
    entered: usize,
}
pub(super) struct LoopStack {
    frames: Vec<Frame>,
    limit: usize,
    shared_entered: Option<usize>,
}
impl Default for LoopStack {
    fn default() -> Self {
        Self::with_limit(MAX_LOOP_ITERATIONS)
    }
}
/// Runs a `while run <command>` header check: the invocation, the expected
/// outcome and the current bindings. Returns whether the outcome matched.
pub(super) type CommandCheck<'a> =
    dyn FnMut(&crate::ast::Invocation, bool, &BTreeMap<String, Value>) -> Result<bool> + 'a;

impl LoopStack {
    pub fn with_limit(limit: usize) -> Self {
        Self::with_policy(limit, false)
    }

    pub fn with_policy(limit: usize, legacy_shared: bool) -> Self {
        Self {
            frames: Vec::new(),
            limit,
            shared_entered: legacy_shared.then_some(0),
        }
    }

    /// Enter or re-check a loop whose header has no command condition.
    #[cfg(test)]
    pub fn header(
        &mut self,
        index: usize,
        kind: &LoopKind,
        bindings: &mut BTreeMap<String, Value>,
    ) -> Result<bool> {
        self.header_with_commands(index, kind, bindings, &mut |_, _, _| {
            bail!("command loop headers need a runtime to run their command")
        })
    }

    pub fn header_with_commands(
        &mut self,
        index: usize,
        kind: &LoopKind,
        bindings: &mut BTreeMap<String, Value>,
        command: &mut CommandCheck<'_>,
    ) -> Result<bool> {
        if self.frames.last().is_none_or(|frame| frame.header != index) {
            let iterations = match kind {
                LoopKind::Repeat { count } => Iterations::Repeat(repeat_count_with_limit(
                    &resolve(count, bindings)?,
                    self.limit,
                )?),
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
                        taken: 0,
                        current: None,
                    }
                }
                LoopKind::While { .. } | LoopKind::WhileCommand { .. } => Iterations::While,
            };
            self.frames.push(Frame {
                header: index,
                outer: bindings.keys().cloned().collect(),
                iterations,
                entered: 0,
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
            Iterations::ForEach {
                binding,
                items,
                taken,
                current,
            } => {
                if let Some(item) = items.next() {
                    *taken += 1;
                    *current = Some(item.clone());
                    bindings.insert(binding.clone(), item);
                    true
                } else {
                    false
                }
            }
            Iterations::While => match kind {
                LoopKind::While { condition } => control::evaluate(condition, bindings)?,
                LoopKind::WhileCommand {
                    call,
                    expect_success,
                } => command(call, *expect_success, &*bindings)?,
                _ => bail!("loop frame kind mismatch"),
            },
        };
        if !active {
            let frame = self.frames.pop().expect("active loop frame");
            bindings.retain(|name, _| frame.outer.contains(name));
            return Ok(false);
        }
        let entered = self.shared_entered.as_mut().unwrap_or(&mut frame.entered);
        if *entered >= self.limit {
            bail!("loop iteration budget of {} exceeded", self.limit);
        }
        *entered += 1;
        Ok(true)
    }
    /// The element index and item of the `for each` at `header` that is running now.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn current_element(&self, header: usize) -> Option<(usize, &Value)> {
        let frame = self
            .frames
            .iter()
            .rev()
            .find(|frame| frame.header == header)?;
        match &frame.iterations {
            Iterations::ForEach {
                taken,
                current: Some(item),
                ..
            } => Some((taken - 1, item)),
            _ => None,
        }
    }

    /// Re-enter a top-level `for each` whose first `consumed` items already ran in a
    /// checkpointed run. The next header check yields item `consumed`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn restore_for_each(
        &mut self,
        header: usize,
        binding: &str,
        outer: BTreeSet<String>,
        mut items: Vec<Value>,
        consumed: usize,
    ) -> Result<()> {
        if consumed > items.len() {
            bail!("checkpoint restored more for each elements than the list holds");
        }
        if self.frames.last().is_some() {
            bail!("checkpoint restored a for each inside another loop");
        }
        let restored_total = self
            .shared_entered
            .unwrap_or(0)
            .checked_add(consumed)
            .ok_or_else(|| anyhow!("loop iteration budget of {} exceeded", self.limit))?;
        if restored_total > self.limit {
            bail!("loop iteration budget of {} exceeded", self.limit);
        }
        if let Some(total) = &mut self.shared_entered {
            *total = restored_total;
        }
        let remaining = items.split_off(consumed);
        self.frames.push(Frame {
            header,
            outer,
            entered: consumed,
            iterations: Iterations::ForEach {
                binding: binding.to_owned(),
                items: remaining.into_iter(),
                taken: consumed,
                current: None,
            },
        });
        Ok(())
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
            &BTreeMap::from([("flag".into(), Value::String("true".into()))]),
            None,
        )
        .is_err());
        assert!(preflight(
            &kind,
            &BTreeMap::from([("flag".into(), Value::List(vec![]))]),
            None,
        )
        .is_err());
        assert!(preflight(
            &kind,
            &BTreeMap::from([("flag".into(), Value::Bool(false))]),
            None,
        )
        .is_ok());
        assert!(preflight(&kind, &BTreeMap::new(), None).is_ok());
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
    fn nested_loops_have_independent_limits() -> Result<()> {
        let one = LoopKind::Repeat {
            count: Value::Integer(1),
        };
        let mut stack = LoopStack::with_limit(1);
        let mut bindings = BTreeMap::new();
        assert!(stack.header(0, &one, &mut bindings)?);
        assert!(stack.header(2, &one, &mut bindings)?);
        stack.end_iteration(2, &mut bindings)?;
        assert!(!stack.header(2, &one, &mut bindings)?);
        stack.end_iteration(0, &mut bindings)?;
        assert!(!stack.header(0, &one, &mut bindings)?);
        assert!(stack.header(4, &one, &mut bindings)?);
        Ok(())
    }

    #[test]
    fn legacy_policy_shares_one_counter_across_nested_and_sequential_loops() -> Result<()> {
        let one = LoopKind::Repeat {
            count: Value::Integer(1),
        };
        let mut stack = LoopStack::with_policy(1, true);
        let mut bindings = BTreeMap::new();
        assert!(stack.header(0, &one, &mut bindings)?);
        assert!(stack
            .header(2, &one, &mut bindings)
            .unwrap_err()
            .to_string()
            .contains("budget of 1"));
        let mut sequential = LoopStack::with_policy(1, true);
        assert!(sequential.header(0, &one, &mut bindings)?);
        sequential.end_iteration(0, &mut bindings)?;
        assert!(!sequential.header(0, &one, &mut bindings)?);
        assert!(sequential
            .header(4, &one, &mut bindings)
            .unwrap_err()
            .to_string()
            .contains("budget of 1"));
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
