//! Validation and lexical execution frames for lowered finite program calls.
use super::*;
use crate::ast::{InputDeclaration, InputType};

pub(super) fn validate_definitions(program: &Program, registry: &Registry) -> Result<()> {
    for (name, definition) in program.definitions.iter() {
        // Validation reads the body; borrowing avoids copying every literal and
        // input default merely to construct a temporary finite Program.
        let checked = (|| -> Result<()> {
            crate::inputs::validate(&definition.inputs)?;
            control::validate_shape(&definition.steps)?;
            let mut flow = control::FlowState {
                names: definition
                    .inputs
                    .iter()
                    .map(|input| input.name.clone())
                    .collect(),
                types: definition
                    .inputs
                    .iter()
                    .map(|input| (input.name.clone(), input.kind))
                    .collect(),
                has_data: true,
                started: false,
            };
            validate_range(
                &definition.steps,
                registry,
                &mut flow,
                0..definition.steps.len(),
                true,
                false,
            )
        })();
        checked.with_context(|| format!("program {name:?} defined at line {}", definition.line))?;
    }
    Ok(())
}

pub(super) fn validate_arguments(
    call: &Invocation,
    inputs: &[InputDeclaration],
    flow: &control::FlowState,
) -> Result<()> {
    if call.direct || call.cache || call.status_binding.is_some() {
        bail!("program calls cannot be direct binaries, cached or capture native status");
    }
    crate::inputs::validate(inputs)?;
    for name in call.parameters.keys() {
        if !inputs.iter().any(|input| input.name == *name) {
            bail!("program {:?} has no input {name:?}", call.operation);
        }
    }
    for input in inputs {
        let Some(value) = call.parameters.get(&input.name).or(input.default.as_ref()) else {
            bail!(
                "program {:?} requires input {:?}",
                call.operation,
                input.name
            );
        };
        check_value(value, &flow.names)?;
        if let Value::Reference(name) = value {
            if let Some(kind) = flow.types.get(name) {
                if input.kind != InputType::Json && input.kind != *kind {
                    bail!(
                        "program input {:?} expects {}, but {name:?} contains {kind}",
                        input.name,
                        input.kind
                    );
                }
            }
        } else if !contains_reference(value) {
            input.validate_value(value)?;
        } else if input.kind == InputType::TextList {
            let Value::List(items) = value else {
                bail!("program input {:?} expects list of text", input.name);
            };
            for item in items {
                match item {
                    Value::String(text) if !text.contains('\0') => {}
                    Value::Reference(name)
                        if flow
                            .types
                            .get(name)
                            .is_none_or(|kind| *kind == InputType::Text) => {}
                    _ => bail!("program input {:?} expects text list items", input.name),
                }
            }
        } else if input.kind != InputType::Json {
            bail!("program input {:?} expects {}", input.name, input.kind);
        }
    }
    Ok(())
}

pub(super) struct Frame<'a> {
    pub name: &'a str,
    pub line: usize,
    pub binding: Option<&'a str>,
    pub bindings: BTreeMap<String, Value>,
    pub cwd: PathBuf,
}

/// The schema was checked before effects; resolve each actual value once.
pub(super) fn bind(
    call: &Invocation,
    inputs: &[InputDeclaration],
    parent: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>> {
    inputs
        .iter()
        .map(|input| {
            let value = match call.parameters.get(&input.name) {
                Some(value) => resolve(value, parent)?,
                None => input
                    .default
                    .clone()
                    .ok_or_else(|| anyhow!("missing program input {:?}", input.name))?,
            };
            input.validate_value(&value)?;
            Ok((input.name.clone(), value))
        })
        .collect()
}

pub(super) fn preflight(
    program: &Program,
    registry: &Registry,
    supplied: &BTreeMap<String, Value>,
) -> Result<()> {
    fn known(value: &Value, values: &BTreeMap<String, Value>) -> Value {
        match value {
            Value::Reference(name) => values.get(name).cloned().unwrap_or_else(|| value.clone()),
            Value::List(items) => {
                Value::List(items.iter().map(|item| known(item, values)).collect())
            }
            Value::Record(items) => Value::Record(
                items
                    .iter()
                    .map(|(name, item)| (name.clone(), known(item, values)))
                    .collect(),
            ),
            value => value.clone(),
        }
    }
    fn native(
        call: &Invocation,
        line: usize,
        registry: &Registry,
        values: &BTreeMap<String, Value>,
        types: &BTreeMap<String, InputType>,
    ) -> Result<()> {
        let mut resolved = call.clone();
        resolved.parameters = call
            .parameters
            .iter()
            .map(|(name, value)| (name.clone(), known(value, values)))
            .collect();
        registry
            .resolve_call(call)?
            .validate_with_types(&resolved, types)
            .with_context(|| format!("line {line}: program inputs"))
    }
    let mut values = supplied.clone();
    let mut types: BTreeMap<_, _> = program
        .inputs
        .iter()
        .map(|input| (input.name.clone(), input.kind))
        .collect();
    if let Some(Trigger::Listen {
        source: ListenSource::Applet(call),
        line,
        ..
    }) = &program.trigger
    {
        native(call, *line, registry, &values, &types)?;
    }
    let mut scopes = Vec::new();
    for step in &program.steps {
        match &step.kind {
            StepKind::CallProgram { call, inputs, .. } => {
                let mut child = BTreeMap::new();
                for input in inputs {
                    if let Some(value) = call.parameters.get(&input.name).or(input.default.as_ref())
                    {
                        let value = known(value, &values);
                        if !contains_reference(&value) {
                            input.validate_value(&value).with_context(|| {
                                format!(
                                    "program {:?}, called at line {}",
                                    call.operation, step.line
                                )
                            })?;
                            child.insert(input.name.clone(), value);
                        }
                    }
                }
                let child_types = inputs
                    .iter()
                    .map(|input| (input.name.clone(), input.kind))
                    .collect();
                scopes.push((
                    std::mem::replace(&mut values, child),
                    std::mem::replace(&mut types, child_types),
                    call.binding.clone(),
                ));
            }
            StepKind::EndCall => {
                let (parent, parent_types, binding) = scopes
                    .pop()
                    .context("program return without preflight scope")?;
                values = parent;
                types = parent_types;
                if let Some(name) = binding {
                    values.remove(&name);
                    types.insert(name, InputType::Text);
                }
            }
            StepKind::Invoke { call, .. } => {
                native(call, step.line, registry, &values, &types)?;
                if let Some(name) = &call.binding {
                    values.remove(name);
                    types.insert(name.clone(), InputType::Text);
                }
                if let Some(name) = &call.status_binding {
                    values.remove(name);
                    types.insert(name.clone(), InputType::Integer);
                }
            }
            StepKind::Read { binding, .. }
            | StepKind::ReadStdin { binding }
            | StepKind::ReadCallInput { binding } => {
                if let Some(name) = binding {
                    values.remove(name);
                    types.insert(name.clone(), InputType::Text);
                }
            }
            StepKind::ParseJson { binding } => {
                values.remove(binding);
                types.insert(binding.clone(), InputType::Json);
            }
            StepKind::TakeJson {
                pointer,
                source,
                kind,
                binding,
            } => {
                if let Ok(source) = structured_source(source, &values) {
                    crate::structured::take(pointer, &source, *kind, step.line)
                        .with_context(|| format!("line {}: program inputs", step.line))?;
                }
                values.remove(binding);
                types.insert(binding.clone(), *kind);
            }
            _ => {}
        }
    }
    Ok(())
}
