//! Validated structured control flow; no shell, byte copies or truthiness.
use super::*;
use crate::ast::{Condition, InputType, Step};

pub(super) fn validate_shape(steps: &[Step], return_allowed: bool) -> Result<()> {
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
        Try {
            finish: usize,
            catch: Option<usize>,
            finally: Option<usize>,
            catch_seen: bool,
            finally_seen: bool,
        },
    }
    impl Frame {
        fn finish(&self) -> usize {
            match self {
                Self::If { finish, .. }
                | Self::Call { finish, .. }
                | Self::Loop { finish, .. }
                | Self::Try { finish, .. } => *finish,
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
            StepKind::Try {
                catch,
                finally,
                end,
            } => {
                let finish = index.checked_add(*end).context("try offset overflow")?;
                let catch = catch
                    .map(|offset| index.checked_add(offset).context("catch offset overflow"))
                    .transpose()?;
                let finally = finally
                    .map(|offset| index.checked_add(offset).context("finally offset overflow"))
                    .transpose()?;
                let body_end = catch.or(finally).unwrap_or(finish.saturating_sub(1));
                let catch_target = finally.unwrap_or_else(|| finish.saturating_sub(1));
                let catch_shape = catch.is_none_or(|marker| {
                    marker > index + 1
                        && marker < finish.saturating_sub(1)
                        && marker + 1 < catch_target
                        && matches!(steps.get(marker).map(|step| &step.kind), Some(StepKind::Catch { end, .. }) if marker.checked_add(*end) == Some(catch_target))
                });
                let finally_shape = finally.is_none_or(|marker| {
                    marker > index + 1
                        && marker < finish.saturating_sub(1)
                        && marker + 1 < finish.saturating_sub(1)
                        && matches!(
                            steps.get(marker).map(|step| &step.kind),
                            Some(StepKind::Finally)
                        )
                });
                if *end < 3
                    || finish > steps.len()
                    || stack.last().is_some_and(|parent| finish >= parent.finish())
                    || stack.len().saturating_sub(calls) >= 64
                    || (catch.is_none() && finally.is_none())
                    || catch
                        .zip(finally)
                        .is_some_and(|(catch, finally)| catch >= finally)
                    || body_end <= index + 1
                    || !catch_shape
                    || !finally_shape
                    || !matches!(
                        steps.get(finish.saturating_sub(1)).map(|step| &step.kind),
                        Some(StepKind::EndTry)
                    )
                {
                    bail!("line {}: invalid try block offsets or nesting", step.line);
                }
                stack.push(Frame::Try {
                    finish,
                    catch,
                    finally,
                    catch_seen: false,
                    finally_seen: false,
                });
            }
            StepKind::Catch { .. } => {
                let Some(Frame::Try {
                    catch,
                    finally_seen,
                    catch_seen,
                    ..
                }) = stack.last_mut()
                else {
                    bail!("line {}: catch outside a try block", step.line);
                };
                if *catch != Some(index) || *catch_seen || *finally_seen {
                    bail!("line {}: misplaced catch marker", step.line);
                }
                *catch_seen = true;
            }
            StepKind::Finally => {
                let Some(Frame::Try {
                    finally,
                    finally_seen,
                    catch,
                    catch_seen,
                    ..
                }) = stack.last_mut()
                else {
                    bail!("line {}: finally outside a try block", step.line);
                };
                if *finally != Some(index) || *finally_seen || (catch.is_some() && !*catch_seen) {
                    bail!("line {}: misplaced finally marker", step.line);
                }
                *finally_seen = true;
            }
            StepKind::EndTry => {
                let Some(Frame::Try {
                    finish,
                    catch,
                    finally,
                    catch_seen,
                    finally_seen,
                }) = stack.pop()
                else {
                    bail!("line {}: try end without try", step.line);
                };
                if finish != index + 1
                    || catch.is_some() != catch_seen
                    || finally.is_some() != finally_seen
                {
                    bail!("line {}: invalid try end", step.line);
                }
            }
            StepKind::BreakLoop | StepKind::ContinueLoop => {
                let last_call = stack
                    .iter()
                    .rposition(|frame| matches!(frame, Frame::Call { .. }));
                if !stack.iter().enumerate().any(|(position, frame)| {
                    matches!(frame, Frame::Loop { .. })
                        && last_call.is_none_or(|call| position > call)
                }) {
                    bail!(
                        "line {}: loop transfer is only valid inside a loop",
                        step.line
                    );
                }
            }
            StepKind::Return if !return_allowed && calls == 0 => {
                bail!(
                    "line {}: return is only valid inside a reusable program",
                    step.line
                );
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
        Value::List(values)
            if values.iter().all(|item| match item {
                Value::String(_) => true,
                Value::Reference(name) => {
                    types.get(name).is_none_or(|kind| *kind == InputType::Text)
                }
                _ => false,
            }) =>
        {
            Some(InputType::TextList)
        }
        Value::List(_) => Some(InputType::Json),
        Value::Record(_) | Value::Null | Value::Number(_) => Some(InputType::Json),
    };
    Ok(kind)
}

fn validate_equality_types(left: Option<InputType>, right: Option<InputType>) -> Result<()> {
    if let (Some(left), Some(right)) = (left, right) {
        if left != right && left != InputType::Json && right != InputType::Json {
            bail!(
                "condition compares {left} with {right}; values must have the same type, and implicit conversion is unsupported"
            );
        }
    }
    Ok(())
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
    match condition.operator {
        crate::ast::ConditionOperator::Equals => validate_equality_types(left, right)?,
        crate::ast::ConditionOperator::Contains => {
            match left {
                Some(InputType::Text | InputType::TextList | InputType::Json) | None => {}
                Some(kind) => {
                    bail!("contains is unsupported for {kind}; use text, a list, or a JSON object")
                }
            }
            if matches!(left, Some(InputType::Text | InputType::TextList))
                && right.is_some_and(|kind| kind != InputType::Text)
            {
                bail!("string and text-list containment requires a text value");
            }
        }
    }
    Ok(())
}
pub(super) fn evaluate(condition: &Condition, bindings: &BTreeMap<String, Value>) -> Result<bool> {
    let left = super::resolve(&condition.left, bindings)?;
    let right = super::resolve(&condition.right, bindings)?;
    let matched = match condition.operator {
        crate::ast::ConditionOperator::Equals => {
            // Typed equality is deep across records and arrays. Values whose
            // JSON variants differ simply compare unequal (including null checks).
            // Recheck resolved names here: an undeclared host value has no
            // schema during preflight, but must not gain scalar coercion.
            let no_declarations = BTreeMap::new();
            validate_equality_types(
                scalar_kind(&left, &no_declarations)?,
                scalar_kind(&right, &no_declarations)?,
            )?;
            left == right
        }
        crate::ast::ConditionOperator::Contains => match (&left, &right) {
            (Value::String(haystack), Value::String(needle)) => haystack.contains(needle),
            (Value::List(items), needle) => items.iter().any(|item| item == needle),
            (Value::Record(fields), Value::String(key)) => fields.contains_key(key),
            _ => bail!(
                "contains requires text in text, a value in a list, or a text key in a record"
            ),
        },
    };
    Ok(matched != condition.negated)
}

#[derive(Clone)]
pub(super) struct FlowState {
    pub names: BTreeSet<String>,
    pub types: BTreeMap<String, InputType>,
    /// Job handles available for inspection/await/cancel in this lexical call.
    pub jobs: BTreeSet<String>,
    /// Handles that have not yet been explicitly awaited or cancelled.
    pub unresolved_jobs: BTreeSet<String>,
    pub has_data: bool,
    /// Whether the last process stage explicitly discarded or inherited its
    /// stdout. This is distinct from an initially empty stream: legacy `then`
    /// calls have always been allowed to receive EOF when there is no input.
    pub stdout_unavailable: bool,
    pub started: bool,
    pub invariant: BTreeMap<String, InputType>,
    pub invariant_names: BTreeSet<String>,
    pub loop_depth: usize,
}
impl FlowState {
    pub fn bind_type(&mut self, name: &str, kind: InputType) -> Result<()> {
        if self.invariant_names.contains(name) && !self.invariant.contains_key(name) {
            bail!(
                "loop cannot rebind {name:?}: its outer type is unknown; bind a typed result before the loop"
            );
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
                        bail!(
                            "result {name:?} has different types across conditional branches: {left} and {right}"
                        );
                    }
                }
            }
        }
        yes.types
            .retain(|name, _| yes.names.contains(name) && no.types.contains_key(name));
        yes.jobs.retain(|name| no.jobs.contains(name));
        yes.unresolved_jobs
            .extend(no.unresolved_jobs.iter().cloned());
        yes.has_data &= no.has_data;
        yes.stdout_unavailable |= no.stdout_unavailable;
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
        assert!(validate_shape(
            &[header(3), body.clone(), step(StepKind::EndLoop { back: 2 })],
            false
        )
        .is_ok());
        assert!(validate_shape(
            &[header(3), body.clone(), step(StepKind::EndLoop { back: 0 })],
            false
        )
        .is_err());
        assert!(validate_shape(
            &[
                header(usize::MAX),
                body.clone(),
                step(StepKind::EndLoop { back: 2 })
            ],
            false
        )
        .is_err());
        assert!(validate_shape(
            &[
                header(4),
                header(3),
                body,
                step(StepKind::EndLoop { back: 2 })
            ],
            false
        )
        .is_err());
        assert!(validate_shape(&[step(StepKind::EndLoop { back: 1 })], false).is_err());
    }

    #[test]
    fn validates_try_boundaries_and_transfer_lexical_scopes() {
        let body = step(StepKind::EncodeJson {
            source: Value::Bool(true),
        });
        let valid_try = [
            step(StepKind::Try {
                catch: Some(2),
                finally: Some(4),
                end: 7,
            }),
            body.clone(),
            step(StepKind::Catch {
                binding: "failure".into(),
                end: 2,
            }),
            body.clone(),
            step(StepKind::Finally),
            body.clone(),
            step(StepKind::EndTry),
        ];
        assert!(validate_shape(&valid_try, false).is_ok());

        let orphan_catch = [step(StepKind::Catch {
            binding: "failure".into(),
            end: 1,
        })];
        assert!(validate_shape(&orphan_catch, false).is_err());

        let invalid_try = [
            step(StepKind::Try {
                catch: Some(3),
                finally: Some(2),
                end: 6,
            }),
            body.clone(),
            step(StepKind::Finally),
            step(StepKind::Catch {
                binding: "failure".into(),
                end: 2,
            }),
            body,
            step(StepKind::EndTry),
        ];
        assert!(validate_shape(&invalid_try, false).is_err());

        assert!(validate_shape(&[step(StepKind::BreakLoop)], false).is_err());
        assert!(validate_shape(&[step(StepKind::ContinueLoop)], false).is_err());
        assert!(validate_shape(&[step(StepKind::Return)], false).is_err());
        assert!(validate_shape(&[step(StepKind::Return)], true).is_ok());

        let valid_loop = [
            header(3),
            step(StepKind::BreakLoop),
            step(StepKind::EndLoop { back: 2 }),
        ];
        assert!(validate_shape(&valid_loop, false).is_ok());
    }
}

#[cfg(test)]
mod structured_condition_tests {
    use super::*;

    #[test]
    fn json_equality_is_deep_and_null_comparison_returns_false_for_non_null() {
        let record = Value::Record(BTreeMap::from([
            ("name".into(), Value::String("Ada".into())),
            ("active".into(), Value::Bool(true)),
        ]));
        let equal_record = Condition {
            left: Value::Reference("left".into()),
            right: record.clone(),
            negated: false,
            operator: crate::ast::ConditionOperator::Equals,
        };
        let null_check = Condition {
            left: Value::Reference("left".into()),
            right: Value::Null,
            negated: false,
            operator: crate::ast::ConditionOperator::Equals,
        };
        let bindings = BTreeMap::from([("left".into(), record)]);

        assert!(evaluate(&equal_record, &bindings).unwrap());
        assert!(!evaluate(&null_check, &bindings).unwrap());
    }

    #[test]
    fn contains_checks_values_in_json_arrays_and_keys_in_records() {
        let list_condition = Condition {
            left: Value::Reference("items".into()),
            right: Value::Integer(3),
            negated: false,
            operator: crate::ast::ConditionOperator::Contains,
        };
        let key_condition = Condition {
            left: Value::Reference("record".into()),
            right: Value::String("ready".into()),
            negated: false,
            operator: crate::ast::ConditionOperator::Contains,
        };
        let bindings = BTreeMap::from([
            (
                "items".into(),
                Value::List(vec![Value::Integer(2), Value::Integer(3)]),
            ),
            (
                "record".into(),
                Value::Record(BTreeMap::from([("ready".into(), Value::Bool(false))])),
            ),
        ]);

        assert!(evaluate(&list_condition, &bindings).unwrap());
        assert!(evaluate(&key_condition, &bindings).unwrap());
    }
}
