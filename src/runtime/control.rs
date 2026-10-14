//! Validated structured control flow; no shell, byte copies or truthiness.
use super::*;
use crate::ast::{Condition, InputType, Step};

pub(super) fn validate_shape(steps: &[Step]) -> Result<()> {
    enum Frame {
        Loop {
            start: usize,
            finish: usize,
        },
        If {
            finish: usize,
            other: usize,
            seen: bool,
        },
        Call {
            start: usize,
            finish: usize,
        },
    }
    impl Frame {
        fn finish(&self) -> usize {
            match self {
                Self::If { finish, .. } | Self::Call { finish, .. } | Self::Loop { finish, .. } => {
                    *finish
                }
            }
        }
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut calls = 0;
    for (index, step) in steps.iter().enumerate() {
        match &step.kind {
            StepKind::Loop { end, .. } => {
                let finish = index.checked_add(*end).context("loop offset overflow")?;
                if *end < 3
                    || finish > steps.len()
                    || stack.last().is_some_and(|parent| finish >= parent.finish())
                    || stack.len().saturating_sub(calls) >= 64
                    || !matches!(steps[finish - 1].kind, StepKind::EndLoop { back } if back == *end - 1)
                {
                    bail!("line {}: invalid loop block offsets or nesting", step.line);
                }
                stack.push(Frame::Loop {
                    start: index,
                    finish,
                });
            }
            StepKind::EndLoop { back } => {
                let Some(Frame::Loop { start, finish }) = stack.pop() else {
                    bail!("line {}: loop end without loop", step.line);
                };
                if finish != index + 1 || index.checked_sub(*back) != Some(start) {
                    bail!("line {}: invalid loop end", step.line);
                }
            }
            StepKind::If { otherwise, end, .. } => {
                let finish = index
                    .checked_add(*end)
                    .context("conditional offset overflow")?;
                let other = index
                    .checked_add(*otherwise)
                    .context("conditional offset overflow")?;
                if *end < 3
                    || *otherwise < 2
                    || (otherwise != end && (*otherwise < 3 || *otherwise >= end - 1))
                    || other > finish
                    || finish > steps.len()
                    || stack.last().is_some_and(|parent| finish >= parent.finish())
                    || stack.len().saturating_sub(calls) >= 64
                {
                    bail!(
                        "line {}: invalid conditional block offsets or nesting",
                        step.line
                    );
                }
                stack.push(Frame::If {
                    finish,
                    other,
                    seen: false,
                });
            }
            StepKind::Otherwise { end } => {
                let Some(Frame::If {
                    finish,
                    other,
                    seen,
                }) = stack.last_mut()
                else {
                    bail!("line {}: otherwise outside a conditional", step.line);
                };
                if *seen || *other != index + 1 || index.checked_add(*end) != Some(*finish) {
                    bail!("line {}: invalid otherwise boundary", step.line);
                }
                *seen = true;
            }
            StepKind::EndIf => {
                let Some(Frame::If {
                    finish,
                    other,
                    seen,
                }) = stack.pop()
                else {
                    bail!("line {}: conditional end without if", step.line);
                };
                if finish != index + 1 || (!seen && other != finish) {
                    bail!("line {}: invalid conditional end", step.line);
                }
            }
            StepKind::CallProgram { end, .. } => {
                let finish = index
                    .checked_add(*end)
                    .context("program call offset overflow")?;
                if *end < 3
                    || finish > steps.len()
                    || stack.last().is_some_and(|parent| finish >= parent.finish())
                    || calls >= 16
                    || !matches!(steps[finish - 1].kind, StepKind::EndCall)
                {
                    bail!("line {}: invalid program call boundary or depth", step.line);
                }
                stack.push(Frame::Call {
                    start: index,
                    finish,
                });
                calls += 1;
            }
            StepKind::EndCall => {
                let Some(Frame::Call { finish, .. }) = stack.pop() else {
                    bail!("line {}: program end without call", step.line);
                };
                if finish != index + 1 {
                    bail!("line {}: invalid program end", step.line);
                }
                calls -= 1;
            }
            StepKind::ReadCallInput { .. } if !matches!(stack.last(), Some(Frame::Call { start, .. }) if *start + 1 == index) =>
            {
                bail!(
                    "line {}: reusable stdin must begin a program call",
                    step.line
                );
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        bail!("unterminated conditional or program call");
    }
    Ok(())
}

fn scalar_kind(value: &Value, types: &BTreeMap<String, InputType>) -> Result<Option<InputType>> {
    let kind = match value {
        Value::String(_) => Some(InputType::Text),
        Value::Integer(_) => Some(InputType::Integer),
        Value::Bool(_) => Some(InputType::Boolean),
        Value::Reference(name) => types.get(name).copied(),
        Value::List(_) => Some(InputType::TextList),
        Value::Record(_) | Value::Null | Value::Number(_) => Some(InputType::Json),
    };
    if matches!(kind, Some(InputType::TextList | InputType::Json)) {
        bail!("conditions compare text, integer or boolean scalars; select a typed scalar from JSON first");
    }
    Ok(kind)
}
pub(super) fn validate_condition(
    condition: &Condition,
    names: &BTreeSet<String>,
    types: &BTreeMap<String, InputType>,
) -> Result<()> {
    check_value(&condition.left, names)?;
    check_value(&condition.right, names)?;
    let left = scalar_kind(&condition.left, types)?;
    let right = scalar_kind(&condition.right, types)?;
    if let (Some(left), Some(right)) = (left, right) {
        if left != right {
            bail!("condition compares {left} with {right}; implicit conversion is unsupported");
        }
    }
    Ok(())
}
fn operand<'a>(value: &'a Value, bindings: &'a BTreeMap<String, Value>) -> Result<&'a Value> {
    if let Value::Reference(name) = value {
        bindings
            .get(name)
            .ok_or_else(|| anyhow!("undefined result {name:?}"))
    } else {
        Ok(value)
    }
}
pub(super) fn evaluate(condition: &Condition, bindings: &BTreeMap<String, Value>) -> Result<bool> {
    let left = operand(&condition.left, bindings)?;
    let right = operand(&condition.right, bindings)?;
    let equal = match (left, right) {
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Integer(left), Value::Integer(right)) => left == right,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        _ => bail!(
            "condition requires two scalars of the same type; implicit conversion is unsupported"
        ),
    };
    Ok(equal != condition.negated)
}

#[derive(Clone)]
pub(super) struct FlowState {
    pub names: BTreeSet<String>,
    pub types: BTreeMap<String, InputType>,
    pub has_data: bool,
    pub started: bool,
    pub invariant: BTreeMap<String, InputType>,
    pub invariant_names: BTreeSet<String>,
    pub loop_depth: usize,
}
impl FlowState {
    pub fn bind_type(&mut self, name: &str, kind: InputType) -> Result<()> {
        if self.invariant_names.contains(name) && !self.invariant.contains_key(name) {
            bail!("loop cannot rebind {name:?}: its outer type is unknown; bind a typed result before the loop");
        }
        if let Some(expected) = self.invariant.get(name) {
            if *expected != kind {
                bail!("loop binding {name:?} must remain {expected}, received {kind}");
            }
        }
        self.types.insert(name.to_owned(), kind);
        Ok(())
    }
    pub fn merge(mut yes: Self, no: &Self) -> Result<Self> {
        yes.names.retain(|name| no.names.contains(name));
        for (name, left) in &yes.types {
            if yes.names.contains(name) {
                if let Some(right) = no.types.get(name) {
                    if left != right {
                        bail!("result {name:?} has different types across conditional branches: {left} and {right}");
                    }
                }
            }
        }
        yes.types
            .retain(|name, _| yes.names.contains(name) && no.types.contains_key(name));
        yes.has_data &= no.has_data;
        yes.started |= no.started;
        Ok(yes)
    }
}

#[cfg(test)]
mod loop_shape_tests {
    use super::*;
    fn step(kind: StepKind) -> Step {
        Step { line: 1, kind }
    }
    fn header(end: usize) -> Step {
        step(StepKind::Loop {
            kind: crate::ast::LoopKind::Repeat {
                count: Value::Integer(1),
            },
            end,
        })
    }
    #[test]
    fn rejects_forged_backedges_and_control_boundary_crossings() {
        let body = step(StepKind::EncodeJson {
            source: Value::Bool(true),
        });
        assert!(
            validate_shape(&[header(3), body.clone(), step(StepKind::EndLoop { back: 2 })]).is_ok()
        );
        assert!(
            validate_shape(&[header(3), body.clone(), step(StepKind::EndLoop { back: 0 })])
                .is_err()
        );
        assert!(validate_shape(&[
            header(usize::MAX),
            body.clone(),
            step(StepKind::EndLoop { back: 2 })
        ])
        .is_err());
        assert!(validate_shape(&[
            header(4),
            header(3),
            body,
            step(StepKind::EndLoop { back: 2 })
        ])
        .is_err());
        assert!(validate_shape(&[step(StepKind::EndLoop { back: 1 })]).is_err());
    }
}
