//! Validation and lexical execution frames for lowered finite program calls.
use super::*;

use crate::ast::{InputDeclaration, InputType};

pub(super) fn validate_definitions(program: &Program, registry: &Registry) -> Result<()> {
    for (name, definition) in program.definitions.iter() {
        // Validation reads the body; borrowing avoids copying every literal and
        // input default merely to construct a temporary finite Program.
        let checked = (|| -> Result<()> {
            crate::inputs::validate(&definition.inputs)?;
            control::validate_shape(&definition.steps, true)?;
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
                jobs: BTreeSet::new(),
                unresolved_jobs: BTreeSet::new(),
                has_data: true,
                stdout_unavailable: false,
                started: false,
                invariant: BTreeMap::new(),
                invariant_names: BTreeSet::new(),
                loop_depth: 0,
            };
            validate_range(
                &definition.steps,
                registry,
                &mut flow,
                0..definition.steps.len(),
                true,
                false,
                true,
            )
            .and_then(|()| {
                if flow.unresolved_jobs.is_empty() {
                    Ok(())
                } else {
                    bail!("every background job must be awaited or cancelled before normal reusable-program exit")
                }
            })
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
    if call.discard_stderr {
        bail!("program calls cannot use `and discard stderr`");
    }
    if call.direct || call.cache || call.status_binding.is_some() || call.result_binding.is_some() {
        bail!(
            "program calls cannot be direct binaries, cached or capture native status or execution results"
        );
    }
    crate::inputs::validate(inputs)?;
    // Keep tiny schemas on a linear lookup, as inputs::validated_specs does.
    // If both sides are large, index declaration names once instead of
    // rescanning the full schema for every supplied parameter. BTreeMap order
    // still determines which unknown name is reported first.
    let input_names = (inputs.len() > 8 && call.parameters.len() > 8).then(|| {
        inputs
            .iter()
            .map(|input| input.name.as_str())
            .collect::<BTreeSet<_>>()
    });
    for name in call.parameters.keys() {
        let declared = input_names.as_ref().map_or_else(
            || inputs.iter().any(|input| input.name == *name),
            |names| names.contains(name.as_str()),
        );
        if !declared {
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
    #[cfg(not(target_arch = "wasm32"))]
    pub job_scope: usize,
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
        // Literal arguments already are the resolved values. Only calls that
        // reference bindings need an owned invocation, with each value copied
        // once rather than cloning the original map before replacing it.
        let resolved = if call.parameters.values().any(contains_reference) {
            Cow::Owned(Invocation {
                operation: call.operation.clone(),
                direct: call.direct,
                discard_stderr: call.discard_stderr,
                parameters: call
                    .parameters
                    .iter()
                    .map(|(name, value)| (name.clone(), known(value, values)))
                    .collect(),
                binding: call.binding.clone(),
                cache: call.cache,
                status_binding: call.status_binding.clone(),
                result_binding: call.result_binding.clone(),
            })
        } else {
            Cow::Borrowed(call)
        };
        registry
            .resolve_call(call)?
            .validate_with_types(&resolved, types)
            .with_context(|| format!("line {line}: program inputs"))
    }
    // Most preflight passes only inspect supplied values. Keep their large
    // trees borrowed until a known binding actually becomes unavailable.
    fn forget(values: &mut Cow<'_, BTreeMap<String, Value>>, name: &str) {
        match values {
            Cow::Owned(owned) => {
                owned.remove(name);
            }
            Cow::Borrowed(borrowed) => {
                if borrowed.contains_key(name) {
                    // The invalidated value is no longer observable. Avoid
                    // copying its potentially large tree just to discard it.
                    *values = Cow::Owned(
                        borrowed
                            .iter()
                            .filter(|(key, _)| key.as_str() != name)
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect(),
                    );
                }
            }
        }
    }
    let mut values = Cow::Borrowed(supplied);
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
    let mut loop_scopes = Vec::new();
    for step in &program.steps {
        match &step.kind {
            StepKind::Loop { kind, .. } => {
                loops::preflight(kind, &values)
                    .with_context(|| format!("line {}: loop inputs", step.line))?;
                loop_scopes.push((
                    values.keys().cloned().collect::<BTreeSet<_>>(),
                    types.clone(),
                ));
                if let crate::ast::LoopKind::ForEach { binding, .. } = kind {
                    forget(&mut values, binding);
                    types.insert(binding.clone(), InputType::Text);
                }
            }
            StepKind::EndLoop { .. } => {
                let (outer, outer_types) = loop_scopes
                    .pop()
                    .context("loop end without preflight scope")?;
                if values.keys().any(|name| !outer.contains(name)) {
                    values.to_mut().retain(|name, _| outer.contains(name));
                }
                types = outer_types;
            }
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
                    std::mem::replace(&mut values, Cow::Owned(child)),
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
                    forget(&mut values, &name);
                    types.insert(name, InputType::Text);
                }
            }
            StepKind::TaskGroup {
                binding,
                tasks,
                each,
                ..
            } => {
                if let Some(each) = each {
                    // Element values are only known at runtime; check the
                    // template with the element typed like a loop binding.
                    let mut item_values = values.clone().into_owned();
                    item_values.remove(&each.binding);
                    let mut item_types = types.clone();
                    item_types.insert(
                        each.binding.clone(),
                        loops::element_type(&each.values, &types)?,
                    );
                    for task in tasks {
                        native(task, step.line, registry, &item_values, &item_types)?;
                    }
                }
                for task in tasks.iter().filter(|_| each.is_none()) {
                    native(task, step.line, registry, &values, &types)?;
                }
                forget(&mut values, binding);
                types.insert(binding.clone(), InputType::Json);
            }
            StepKind::SpawnJob { stages, .. } => {
                for stage in stages {
                    native(&stage.call, stage.line, registry, &values, &types)?;
                }
            }
            StepKind::InspectJob { binding, .. } => {
                forget(&mut values, binding);
                types.insert(binding.clone(), InputType::Json);
            }
            StepKind::AwaitJob {
                status_binding,
                result_binding,
                ..
            } => {
                if let Some(name) = status_binding {
                    forget(&mut values, name);
                    types.insert(name.clone(), InputType::Integer);
                }
                if let Some(name) = result_binding {
                    forget(&mut values, name);
                    types.insert(name.clone(), InputType::Json);
                }
            }
            StepKind::Invoke { call, .. } => {
                native(call, step.line, registry, &values, &types)?;
                if let Some(name) = &call.binding {
                    forget(&mut values, name);
                    let kind = registry
                        .resolve_call(call)?
                        .output_kind()
                        .unwrap_or(InputType::Text);
                    types.insert(name.clone(), kind);
                }
                if let Some(name) = &call.status_binding {
                    forget(&mut values, name);
                    types.insert(name.clone(), InputType::Integer);
                }
                if let Some(name) = &call.result_binding {
                    forget(&mut values, name);
                    types.insert(name.clone(), InputType::Json);
                }
            }
            StepKind::Read { binding, .. }
            | StepKind::ReadStdin { binding }
            | StepKind::ReadCallInput { binding } => {
                if let Some(name) = binding {
                    forget(&mut values, name);
                    types.insert(name.clone(), InputType::Text);
                }
            }
            StepKind::ParseJson { binding } => {
                forget(&mut values, binding);
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
                forget(&mut values, binding);
                types.insert(binding.clone(), *kind);
            }
            StepKind::TakeJsonDynamic {
                pointer,
                source,
                kind,
                binding,
            } => {
                if !contains_reference(pointer) {
                    let pointer = known(pointer, &values);
                    if let Value::String(pointer) = pointer {
                        crate::structured::validate_pointer(&pointer)?;
                        if let Ok(source) = structured_source(source, &values) {
                            crate::structured::take(&pointer, &source, *kind, step.line)
                                .with_context(|| format!("line {}: program inputs", step.line))?;
                        }
                    }
                }
                forget(&mut values, binding);
                types.insert(binding.clone(), *kind);
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn large_reusable_call_indexes_names_and_preserves_validation_errors() -> Result<()> {
        let count = 2_048;
        let names = (0..count)
            .map(|index| format!("param{index:05}"))
            .collect::<Vec<_>>();
        let mut source = String::from("define a program named worker\n");
        for (index, name) in names.iter().enumerate() {
            source.push_str("  accept ");
            source.push_str(name);
            source.push_str(" as text");
            if index + 1 == count {
                source.push_str(" with default \"fallback\"");
            }
            source.push('\n');
        }
        source.push_str("  read from stdin\nrun program worker with ");
        source.push_str(
            &names[..count - 1]
                .iter()
                .map(|name| format!("{name} \"value\""))
                .collect::<Vec<_>>()
                .join(" and "),
        );
        source.push('\n');

        let registry = Registry::default();
        let program = crate::parser::parse(&source)?;
        // The last declaration is intentionally omitted at the call site and
        // supplied by its default.
        super::super::validate(&program, &registry)?;

        let mut unknown = program.clone();
        let StepKind::CallProgram { call, .. } = &mut unknown.steps[0].kind else {
            panic!("expanded workflow should begin with a reusable call");
        };
        call.parameters
            .insert("z_unknown".into(), Value::String("value".into()));
        call.parameters
            .insert("a_unknown".into(), Value::String("value".into()));
        let StepKind::CallProgram { call, inputs, .. } = &unknown.steps[0].kind else {
            panic!("expanded workflow should begin with a reusable call");
        };
        // Differential oracle for the old linear lookup: unknown names are
        // still selected in the BTreeMap's deterministic key order.
        let expected_unknown = call
            .parameters
            .keys()
            .find(|name| {
                !inputs
                    .iter()
                    .any(|input| input.name.as_str() == name.as_str())
            })
            .expect("the test call contains unknown names");
        assert_eq!(expected_unknown.as_str(), "a_unknown");
        let error = super::super::validate(&unknown, &registry).unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains(&format!("has no input {expected_unknown:?}")),
            "{error}"
        );
        assert!(!error.contains("z_unknown"), "{error}");

        let mut missing_required = program.clone();
        let StepKind::CallProgram { call, .. } = &mut missing_required.steps[0].kind else {
            panic!("expanded workflow should begin with a reusable call");
        };
        call.parameters.remove(&names[1]);
        let error = super::super::validate(&missing_required, &registry).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("requires input {:?}", names[1])),
            "{error:#}"
        );

        let mut wrong_type = program;
        let StepKind::CallProgram { call, .. } = &mut wrong_type.steps[0].kind else {
            panic!("expanded workflow should begin with a reusable call");
        };
        call.parameters.insert(names[0].clone(), Value::Bool(true));
        let error = super::super::validate(&wrong_type, &registry).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("input {:?} expects text", names[0])),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    #[ignore = "explicit release preflight state timing; no processes or effects"]
    #[allow(clippy::assertions_on_constants)]
    fn supplied_preflight_state() -> Result<()> {
        assert!(!cfg!(debug_assertions), "use --release");
        let registry = Registry::default();
        let mut results = BTreeMap::new();
        for (name, mode, size) in [
            ("empty_control", 0, 0),
            ("record_8192", 0, 8192),
            ("record_64k", 0, 64 * 1024),
            ("record_1m", 0, 1024 * 1024),
            ("json_64k", 1, 64 * 1024),
            ("json_4m", 1, 4 * 1024 * 1024),
            ("overwrite_1m_control", 2, 1024 * 1024),
            ("loop_1m", 3, 1024 * 1024),
        ] {
            let source = match mode {
                0 => "run \"true\"\n",
                1 => {
                    "accept payload as json\ntake integer at \"/n\" from payload as number\nthen encode number as json\n"
                }
                2 => "accept record as text\nread \"missing\" as record\n",
                _ => "repeat 0 times\n  run \"true\"\n",
            };
            let program = crate::parser::parse(source)?;
            let supplied = if size == 0 {
                BTreeMap::new()
            } else if mode == 1 {
                BTreeMap::from([(
                    "payload".into(),
                    Value::Record(BTreeMap::from([
                        ("n".into(), Value::Integer(42)),
                        ("padding".into(), Value::String("x".repeat(size))),
                    ])),
                )])
            } else {
                BTreeMap::from([("record".into(), Value::String("x".repeat(size)))])
            };
            let before = supplied.clone();
            preflight(&program, &registry, &supplied)?;
            assert_eq!(before, supplied);
            let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&supplied)?));
            let iterations = if size >= 64 * 1024 { 10 } else { 100 };
            let mut raw = Vec::new();
            for batch in 0..60 {
                let start = Instant::now();
                for _ in 0..iterations {
                    preflight(
                        std::hint::black_box(&program),
                        &registry,
                        std::hint::black_box(&supplied),
                    )?;
                }
                if batch >= 10 {
                    raw.push(start.elapsed().as_secs_f64() * 1e9 / f64::from(iterations));
                }
            }
            assert_eq!(before, supplied);
            let mut sorted = raw.clone();
            sorted.sort_by(f64::total_cmp);
            results.insert(name, json!({"source":source,"inputs_sha256":fingerprint,"payload_bytes":size,"p50_ns":(sorted[24]+sorted[25])/2.0,"p95_ns":sorted[47],"raw_batch_average_ns":raw}));
        }
        fs::write(
            std::env::var("OPT_BENCH_REPORT")?,
            serde_json::to_vec_pretty(
                &json!({"results":results,"method":"Private static preflight entry. Parsed programs, input trees, clone/immutability/hash oracles and registry construction excluded. Validation, known-value handling, scope/type tracking and temporary destruction included. No processes, filesystem effects or provider calls. 10 warmup batches,50measured batches,10 or100 operations each. Actual caller maps checked unchanged before/after timing; not runtime/CLI/event latency or RSS."}),
            )?,
        )?;
        Ok(())
    }

    // Exact pre-optimization implementation, retained as the semantic oracle.
    fn legacy_preflight(
        program: &Program,
        registry: &Registry,
        supplied: &BTreeMap<String, Value>,
    ) -> Result<()> {
        fn known(value: &Value, values: &BTreeMap<String, Value>) -> Value {
            match value {
                Value::Reference(name) => {
                    values.get(name).cloned().unwrap_or_else(|| value.clone())
                }
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
            // Literal arguments already are the resolved values. Only calls that
            // reference bindings need an owned invocation, with each value copied
            // once rather than cloning the original map before replacing it.
            let resolved = if call.parameters.values().any(contains_reference) {
                Cow::Owned(Invocation {
                    operation: call.operation.clone(),
                    direct: call.direct,
                    discard_stderr: call.discard_stderr,
                    parameters: call
                        .parameters
                        .iter()
                        .map(|(name, value)| (name.clone(), known(value, values)))
                        .collect(),
                    binding: call.binding.clone(),
                    cache: call.cache,
                    status_binding: call.status_binding.clone(),
                    result_binding: call.result_binding.clone(),
                })
            } else {
                Cow::Borrowed(call)
            };
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
        let mut loop_scopes = Vec::new();
        for step in &program.steps {
            match &step.kind {
                StepKind::Loop { kind, .. } => {
                    loops::preflight(kind, &values)
                        .with_context(|| format!("line {}: loop inputs", step.line))?;
                    loop_scopes.push((
                        values.keys().cloned().collect::<BTreeSet<_>>(),
                        types.clone(),
                    ));
                    if let crate::ast::LoopKind::ForEach { binding, .. } = kind {
                        values.remove(binding);
                        types.insert(binding.clone(), InputType::Text);
                    }
                }
                StepKind::EndLoop { .. } => {
                    let (outer, outer_types) = loop_scopes
                        .pop()
                        .context("loop end without preflight scope")?;
                    values.retain(|name, _| outer.contains(name));
                    types = outer_types;
                }
                StepKind::CallProgram { call, inputs, .. } => {
                    let mut child = BTreeMap::new();
                    for input in inputs {
                        if let Some(value) =
                            call.parameters.get(&input.name).or(input.default.as_ref())
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
                    if let Some(name) = &call.result_binding {
                        values.remove(name);
                        types.insert(name.clone(), InputType::Json);
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

    #[test]
    fn preflight_borrowing_preserves_errors_scopes_and_caller_values() -> Result<()> {
        let registry = Registry::default();
        let sources = [
            "run \"true\"\n",
            "run \"printf\" with arguments [\"%s\", record] as rendered\nthen \"printf\" with arguments [\"%s\", rendered]\n",
            "read \"missing\" as record\nrun \"printf\" with arguments [\"%s\", record]\n",
            "accept payload as json\ntake integer at \"/n\" from payload as number\nrun \"printf\" with arguments [\"%s\", number]\n",
            "repeat count times\n  run \"true\" as rendered\nrun \"printf\" with arguments [\"%s\", rendered]\n",
            "for each item in files\n  run \"printf\" with arguments [\"%s\", item] as rendered\n",
            "define a program named worker\n  accept item as text with default \"default\"\n  run \"printf\" with arguments [\"%s\", item] as private\nrun program worker with item record as rendered\nrun \"printf\" with arguments [\"%s\", rendered]\n",
            "if true is true\n  run \"printf\" with arguments [\"%s\", record] as rendered\notherwise\n  read \"missing\" as rendered\nrun \"printf\" with arguments [\"%s\", rendered]\n",
        ];
        let values = [
            Value::String("text 雪; $HOME".into()),
            Value::Integer(2),
            Value::Integer(-1),
            Value::Bool(true),
            Value::List(vec![
                Value::String("first".into()),
                Value::String("second".into()),
            ]),
            Value::Record(BTreeMap::from([
                ("n".into(), Value::Integer(42)),
                ("padding".into(), Value::String("x".repeat(512))),
            ])),
            Value::Null,
            Value::Reference("unknown".into()),
        ];
        for source in sources {
            let program = crate::parser::parse(source)?;
            for (index, value) in values.iter().enumerate() {
                for overwrite in [false, true] {
                    let mut supplied = BTreeMap::from([
                        ("record".into(), value.clone()),
                        ("payload".into(), value.clone()),
                        ("count".into(), value.clone()),
                        ("files".into(), value.clone()),
                    ]);
                    if overwrite {
                        supplied.extend([
                            (
                                "rendered".into(),
                                values[(index + 1) % values.len()].clone(),
                            ),
                            ("number".into(), value.clone()),
                            ("item".into(), value.clone()),
                        ]);
                    }
                    let before = supplied.clone();
                    let legacy = legacy_preflight(&program, &registry, &supplied)
                        .map_err(|error| format!("{error:#}"));
                    let candidate = preflight(&program, &registry, &supplied)
                        .map_err(|error| format!("{error:#}"));
                    assert_eq!(
                        candidate, legacy,
                        "source={source},case={index},overwrite={overwrite}"
                    );
                    assert_eq!(supplied, before, "caller values mutated");
                }
            }
        }
        Ok(())
    }
}
