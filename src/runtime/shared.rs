// Platform-independent validation, values and argv mapping. Included by both hosts.
fn check_value(value: &Value, names: &BTreeSet<String>) -> Result<()> {
    match value {
        Value::Reference(name) if !names.contains(name) => bail!("undefined result {name:?}"),
        Value::List(values) => {
            for value in values {
                check_value(value, names)?;
            }
        }
        Value::Record(values) => {
            for value in values.values() {
                check_value(value, names)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn check_path(value: &Value, names: &BTreeSet<String>) -> Result<()> {
    check_value(value, names)?;
    if matches!(value, Value::String(path) if path.contains('\0')) {
        bail!("a path cannot contain a NUL byte");
    }
    if !matches!(value, Value::String(_) | Value::Reference(_)) {
        bail!("a path must be a quoted string or named text result");
    }
    Ok(())
}

pub(crate) fn preflight_inputs(
    program: &Program,
    registry: &Registry,
    bindings: &BTreeMap<String, Value>,
) -> Result<()> {
    if program.inputs.is_empty()
        && !program
            .steps
            .iter()
            .any(|step| matches!(step.kind, StepKind::Loop { .. }))
    {
        return Ok(());
    }
    calls::preflight(program, registry, bindings)?;
    Ok(())
}

fn binding(
    name: &Option<String>,
    names: &mut BTreeSet<String>,
    allow_rebinding: bool,
) -> Result<()> {
    if let Some(name) = name {
        if !names.insert(name.clone()) && !allow_rebinding {
            bail!("result {name:?} is already defined");
        }
    }
    Ok(())
}
#[cfg(not(target_arch = "wasm32"))]
fn pipe_at(program: &Program, index: usize) -> bool {
    pipe_in_steps(&program.steps, index)
}
fn pipe_in_steps(steps: &[crate::ast::Step], index: usize) -> bool {
    matches!(
        steps.get(index).map(|s| &s.kind),
        Some(StepKind::Invoke {
            connection: Connection::Pipe,
            ..
        })
    )
}

/// Validate the complete script before launching any command or saving files.
pub fn validate(program: &Program, registry: &Registry) -> Result<()> {
    crate::inputs::validate(&program.inputs)?;
    #[cfg(target_arch = "wasm32")]
    if program
        .steps
        .iter()
        .chain(
            program
                .definitions
                .values()
                .flat_map(|definition| &definition.steps),
        )
        .any(|step| matches!(step.kind, StepKind::TaskGroup { .. }))
    {
        bail!("browser host does not support task groups");
    }
    #[cfg(not(target_arch = "wasm32"))]
    validate_worker_support(program, registry)?;
    #[cfg(target_arch = "wasm32")]
    if program.trigger.is_some() {
        bail!("browser host supports finite programs; use the native runtime for schedules and listeners");
    }
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(trigger) = &program.trigger {
        let (setup, body) = service::split_program(program);
        if let Some(step) = body
            .steps
            .iter()
            .find(|step| matches!(step.kind, StepKind::ReadStdin { .. }))
        {
            bail!("line {}: read from stdin is for finite programs; use listen to stdin for service records", step.line);
        }
        validate_finite(&setup, registry, None)?;
        for step in &setup.steps {
            match &step.kind {
                StepKind::UseFolder { .. } => {}
                StepKind::Invoke { call, .. }
                    if registry.resolve_call(call)?.builtin == Some(Builtin::Cd) => {}
                _ => bail!(
                    "line {}: service setup requires a folder directive or the cd builtin",
                    step.line
                ),
            }
        }
        return match trigger {
            Trigger::Every { interval_ms, .. } => {
                if *interval_ms == 0 {
                    bail!("schedule interval must be positive");
                }
                validate_finite(&body, registry, None)
            }
            Trigger::Cron { expression, line } => {
                crate::schedule::CronSchedule::parse(expression)
                    .with_context(|| format!("line {line}"))?;
                validate_finite(&body, registry, None)
            }
            Trigger::Listen {
                source,
                binding,
                line,
            } => {
                if let ListenSource::Applet(call) = source {
                    let manifest = registry
                        .resolve_call(call)
                        .with_context(|| format!("line {line}"))?;
                    manifest
                        .validate_with_types(
                            call,
                            &program
                                .inputs
                                .iter()
                                .map(|d| (d.name.clone(), d.kind))
                                .collect(),
                        )
                        .with_context(|| format!("line {line}"))?;
                    if manifest.builtin.is_some() {
                        bail!("listener source must be a producer binary, not a builtin");
                    }
                    for value in call.parameters.values() {
                        check_value(
                            value,
                            &program.inputs.iter().map(|d| d.name.clone()).collect(),
                        )?;
                    }
                    if call.cache || call.binding.is_some() || call.status_binding.is_some() {
                        bail!("listener sources cannot be cached or bind output/status");
                    }
                }
                if program.inputs.iter().any(|d| &d.name == binding) {
                    bail!("listener binding {binding:?} conflicts with a program input");
                }
                validate_finite(&body, registry, Some(binding))
            }
        };
    }
    validate_finite(program, registry, None)
}

#[cfg(not(target_arch = "wasm32"))]
fn validate_worker_support(program: &Program, registry: &Registry) -> Result<()> {
    if worker_backend::WorkerPool::supported() {
        return Ok(());
    }
    fn collect<'a>(steps: &'a [crate::ast::Step], calls: &mut Vec<&'a Invocation>) {
        for step in steps {
            match &step.kind {
                StepKind::Invoke { call, .. } => calls.push(call),
                StepKind::TaskGroup { tasks, .. } => calls.extend(tasks),
                _ => {}
            }
        }
    }
    let mut invocations = Vec::new();
    collect(&program.steps, &mut invocations);
    for definition in program.definitions.values() {
        collect(&definition.steps, &mut invocations);
    }
    if let Some(Trigger::Listen {
        source: ListenSource::Applet(call),
        ..
    }) = &program.trigger
    {
        invocations.push(call);
    }
    for call in invocations {
        if registry.resolve_call(call)?.execution_backend()
            == crate::contracts::AppletBackend::JsonLinesWorker
        {
            bail!("json_lines_worker backend is unsupported on this platform");
        }
    }
    Ok(())
}

fn validate_finite(
    program: &Program,
    registry: &Registry,
    input_binding: Option<&str>,
) -> Result<()> {
    let names = input_binding.into_iter().map(str::to_owned).collect();
    validate_with_inputs(program, registry, names, input_binding.is_some())
}

pub fn validate_with_inputs(
    program: &Program,
    registry: &Registry,
    names: BTreeSet<String>,
    has_data: bool,
) -> Result<()> {
    validate_inputs(program, registry, names, has_data, false)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn validate_live(
    program: &Program,
    registry: &Registry,
    names: BTreeSet<String>,
    has_data: bool,
) -> Result<()> {
    validate_inputs(program, registry, names, has_data, true)
}

fn validate_inputs(
    program: &Program,
    registry: &Registry,
    mut names: BTreeSet<String>,
    has_data: bool,
    allow_rebinding: bool,
) -> Result<()> {
    calls::validate_definitions(program, registry)?;
    crate::inputs::validate(&program.inputs)?;
    let types = program
        .inputs
        .iter()
        .map(|d| {
            names.insert(d.name.clone());
            (d.name.clone(), d.kind)
        })
        .collect::<BTreeMap<_, _>>();
    control::validate_shape(&program.steps, false)?;
    let initial_has_data = has_data;
    let mut flow = control::FlowState {
        names,
        types,
        has_data,
        stdout_unavailable: false,
        started: has_data && !allow_rebinding,
        invariant: BTreeMap::new(),
        invariant_names: BTreeSet::new(),
        loop_depth: 0,
    };
    validate_range(
        &program.steps,
        registry,
        &mut flow,
        0..program.steps.len(),
        initial_has_data,
        allow_rebinding,
        false,
    )?;
    if program.steps.iter().any(|step| {
        matches!(
            step.kind,
            StepKind::CallProgram { .. } | StepKind::Loop { .. }
        )
    }) {
        calls::preflight(program, registry, &BTreeMap::new())?;
    }
    Ok(())
}

fn validate_range(
    steps: &[crate::ast::Step],
    registry: &Registry,
    flow: &mut control::FlowState,
    range: std::ops::Range<usize>,
    initial_has_data: bool,
    allow_rebinding: bool,
    return_allowed: bool,
) -> Result<()> {
    let mut index = range.start;
    while index < range.end {
        let step = &steps[index];
        if let StepKind::Try {
            catch,
            finally,
            end,
        } = &step.kind
        {
            let finish = index + end;
            let catch_at = catch.map(|offset| index + offset);
            let finally_at = finally.map(|offset| index + offset);
            let try_body_end = catch_at.or(finally_at).unwrap_or(finish - 1);
            let entry = flow.clone();
            let mut try_flow = entry.clone();
            validate_range(
                steps,
                registry,
                &mut try_flow,
                index + 1..try_body_end,
                initial_has_data,
                allow_rebinding,
                return_allowed,
            )
            .with_context(|| format!("try body at line {}", step.line))?;

            let mut after_try = if let Some(catch_index) = catch_at {
                let StepKind::Catch { binding: name, .. } = &steps[catch_index].kind else {
                    bail!("line {}: invalid catch marker", step.line);
                };
                if entry.names.contains(name) {
                    bail!(
                        "line {}: catch binding {name:?} is already defined",
                        step.line
                    );
                }
                let mut catch_flow = entry.clone();
                catch_flow.names.insert(name.clone());
                catch_flow
                    .types
                    .insert(name.clone(), crate::ast::InputType::Json);
                let catch_end = finally_at.unwrap_or(finish - 1);
                validate_range(
                    steps,
                    registry,
                    &mut catch_flow,
                    catch_index + 1..catch_end,
                    initial_has_data,
                    allow_rebinding,
                    return_allowed,
                )
                .with_context(|| format!("catch body at line {}", steps[catch_index].line))?;
                catch_flow.names.remove(name);
                catch_flow.types.remove(name);
                control::FlowState::merge(try_flow, &catch_flow)
                    .with_context(|| format!("try/catch at line {}", step.line))?
            } else {
                try_flow
            };

            if let Some(finally_index) = finally_at {
                // Cancellation skips catch and may arrive at any point in the
                // try body, so finally may rely only on names present on entry.
                // Its definitely-bound outputs are then added to normal paths.
                let mut finalizer_flow = entry.clone();
                validate_range(
                    steps,
                    registry,
                    &mut finalizer_flow,
                    finally_index + 1..finish - 1,
                    initial_has_data,
                    allow_rebinding,
                    return_allowed,
                )
                .with_context(|| format!("finally body at line {}", steps[finally_index].line))?;
                for name in finalizer_flow.names.difference(&entry.names) {
                    if let Some(existing) = after_try.types.get(name) {
                        let produced = finalizer_flow.types.get(name);
                        if produced.is_some_and(|kind| kind != existing) {
                            bail!(
                                "line {}: finally binding {name:?} conflicts with its try/catch type",
                                steps[finally_index].line
                            );
                        }
                    }
                    after_try.names.insert(name.clone());
                    if let Some(kind) = finalizer_flow.types.get(name) {
                        after_try.types.insert(name.clone(), *kind);
                    }
                }
                for (name, kind) in &entry.types {
                    if finalizer_flow.types.get(name) != Some(kind) {
                        if let Some(path_kind) = after_try.types.get(name) {
                            if path_kind != finalizer_flow.types.get(name).unwrap_or(kind) {
                                bail!(
                                    "line {}: finally changes {name:?} to a type incompatible with the try/catch path",
                                    steps[finally_index].line
                                );
                            }
                        }
                        if let Some(final_kind) = finalizer_flow.types.get(name) {
                            after_try.types.insert(name.clone(), *final_kind);
                        }
                    }
                }
                after_try.has_data |= finalizer_flow.has_data;
                after_try.stdout_unavailable |= finalizer_flow.stdout_unavailable;
                after_try.started |= finalizer_flow.started;
            }
            *flow = after_try;
            index = finish;
            continue;
        }
        if let StepKind::Loop { kind, end } = &step.kind {
            loops::validate(kind, flow).with_context(|| format!("line {}", step.line))?;
            let finish = index + end;
            let mut body = flow.clone();
            body.invariant
                .extend(flow.types.iter().map(|(name, kind)| (name.clone(), *kind)));
            body.invariant_names.extend(flow.names.iter().cloned());
            body.loop_depth += 1;
            body.has_data = true;
            body.started = true;
            if let crate::ast::LoopKind::ForEach { binding, values } = kind {
                let kind = loops::element_type(values, &flow.types)?;
                body.names.insert(binding.clone());
                body.types.insert(binding.clone(), kind);
                body.invariant.insert(binding.clone(), kind);
            }
            validate_range(
                steps,
                registry,
                &mut body,
                index + 1..finish - 1,
                true,
                allow_rebinding,
                return_allowed,
            )
            .with_context(|| format!("loop at line {}", step.line))?;
            // A loop may execute zero times. Its existing bindings retain their
            // types, and body-local names never become available afterwards.
            flow.has_data = true;
            flow.stdout_unavailable |= body.stdout_unavailable;
            flow.started = true;
            index = finish;
            continue;
        }
        if let StepKind::CallProgram {
            call,
            inputs,
            end,
            connection,
        } = &step.kind
        {
            let finish = index + end;
            calls::validate_arguments(call, inputs, flow)
                .with_context(|| format!("line {}", step.line))?;
            if *connection == Connection::Pipe || pipe_in_steps(steps, finish) {
                bail!(
                    "line {}: program calls use run or then outside native pipe groups",
                    step.line
                );
            }
            let has_data = *connection == Connection::Then && flow.has_data;
            let mut child = control::FlowState {
                names: inputs.iter().map(|input| input.name.clone()).collect(),
                types: inputs
                    .iter()
                    .map(|input| (input.name.clone(), input.kind))
                    .collect(),
                has_data,
                stdout_unavailable: *connection == Connection::Then && flow.stdout_unavailable,
                started: false,
                invariant: BTreeMap::new(),
                invariant_names: BTreeSet::new(),
                loop_depth: 0,
            };
            validate_range(
                steps,
                registry,
                &mut child,
                index + 1..finish - 1,
                has_data,
                false,
                true,
            )
            .with_context(|| {
                format!("program {:?}, called at line {}", call.operation, step.line)
            })?;
            binding(
                &call.binding,
                &mut flow.names,
                allow_rebinding || flow.loop_depth > 0,
            )
            .with_context(|| format!("line {}", step.line))?;
            if let Some(name) = &call.binding {
                flow.bind_type(name, crate::ast::InputType::Text)?;
            }
            flow.has_data = child.has_data;
            flow.stdout_unavailable = child.stdout_unavailable;
            flow.started = true;
            index = finish;
            continue;
        }
        if let StepKind::If {
            condition,
            otherwise,
            end,
        } = &step.kind
        {
            control::validate_condition(condition, &flow.names, &flow.types)
                .with_context(|| format!("line {}", step.line))?;
            let finish = index + end;
            let has_otherwise = otherwise != end;
            let yes_end = if has_otherwise {
                index + otherwise - 1
            } else {
                finish - 1
            };
            // Most command branches only read bindings. Validate those in place,
            // restoring the two data-flow flags between paths, without copying
            // the entire input/name/type environment for every condition.
            let defines_names = steps[index + 1..finish - 1]
                .iter()
                .any(|step| match &step.kind {
                    StepKind::Read { binding, .. }
                    | StepKind::ReadStdin { binding }
                    | StepKind::ReadCallInput { binding } => binding.is_some(),
                    StepKind::Invoke { call, .. } => {
                        call.binding.is_some() || call.status_binding.is_some()
                    }
                    StepKind::ParseJson { .. }
                    | StepKind::TakeJson { .. }
                    | StepKind::TaskGroup { .. } => true,
                    StepKind::CallProgram { .. } | StepKind::Loop { .. } => true,
                    _ => false,
                });
            if !defines_names {
                let before = (flow.has_data, flow.stdout_unavailable, flow.started);
                validate_range(
                    steps,
                    registry,
                    flow,
                    index + 1..yes_end,
                    initial_has_data,
                    allow_rebinding,
                    return_allowed,
                )?;
                let yes = (flow.has_data, flow.stdout_unavailable, flow.started);
                (flow.has_data, flow.stdout_unavailable, flow.started) = before;
                if has_otherwise {
                    validate_range(
                        steps,
                        registry,
                        flow,
                        index + otherwise..finish - 1,
                        initial_has_data,
                        allow_rebinding,
                        return_allowed,
                    )?;
                }
                flow.has_data &= yes.0;
                flow.stdout_unavailable |= yes.1;
                flow.started |= yes.2;
                index = finish;
                continue;
            }
            let mut yes = flow.clone();
            validate_range(
                steps,
                registry,
                &mut yes,
                index + 1..yes_end,
                initial_has_data,
                allow_rebinding,
                return_allowed,
            )?;
            if has_otherwise {
                validate_range(
                    steps,
                    registry,
                    flow,
                    index + otherwise..finish - 1,
                    initial_has_data,
                    allow_rebinding,
                    return_allowed,
                )?;
            }
            *flow = control::FlowState::merge(yes, flow)
                .with_context(|| format!("line {}", step.line))?;
            index = finish;
            continue;
        }
        let checked = (|| -> Result<()> {
            let path = match &step.kind {
                StepKind::UseFolder { path }
                | StepKind::Read { path, .. }
                | StepKind::Save { path, .. } => Some(path),
                _ => None,
            };
            if let Some(Value::Reference(name)) = path {
                if flow
                    .types
                    .get(name)
                    .is_some_and(|kind| *kind != crate::ast::InputType::Text)
                {
                    bail!("path {name:?} requires text, received {}", flow.types[name]);
                }
            }
            match &step.kind {
                StepKind::Loop { .. }
                | StepKind::EndLoop { .. }
                | StepKind::If { .. }
                | StepKind::Otherwise { .. }
                | StepKind::EndIf
                | StepKind::CallProgram { .. }
                | StepKind::EndCall
                | StepKind::Try { .. }
                | StepKind::Catch { .. }
                | StepKind::Finally
                | StepKind::EndTry => {
                    bail!("line {}: orphaned control-flow marker", step.line)
                }
                StepKind::BreakLoop | StepKind::ContinueLoop => {
                    if flow.loop_depth == 0 {
                        bail!(
                            "line {}: loop transfer is only valid inside a loop",
                            step.line
                        );
                    }
                }
                StepKind::Return => {
                    if !return_allowed {
                        bail!(
                            "line {}: return is only valid inside a reusable program",
                            step.line
                        );
                    }
                }
                StepKind::UseFolder { path } => {
                    if flow.started {
                        bail!("use folder must precede data-producing steps");
                    }
                    check_path(path, &flow.names)?;
                }
                StepKind::Read { binding: name, .. }
                | StepKind::ReadStdin { binding: name }
                | StepKind::ReadCallInput { binding: name } => {
                    if let StepKind::Read { path, .. } = &step.kind {
                        check_path(path, &flow.names)?;
                    }
                    if allow_rebinding && matches!(step.kind, StepKind::ReadStdin { .. }) {
                        bail!("managed programs use /input or listen to stdin via the inbox, not read from stdin");
                    }
                    binding(
                        name,
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    if let Some(name) = name {
                        flow.bind_type(name, crate::ast::InputType::Text)?;
                    }
                    flow.has_data = true;
                    flow.stdout_unavailable = false;
                    flow.started = true;
                }
                StepKind::ParseJson { binding: name } => {
                    if !flow.has_data {
                        bail!("parse json requires preceding output");
                    }
                    binding(
                        &Some(name.clone()),
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    flow.bind_type(name, crate::ast::InputType::Json)?;
                    flow.started = true;
                }
                StepKind::TakeJson {
                    pointer,
                    source,
                    kind,
                    binding: name,
                } => {
                    crate::structured::validate_pointer(pointer)?;
                    check_value(source, &flow.names)?;
                    // A named JSON source has dynamic contents, checked when selected.
                    // Literal sources can be checked now, before earlier native effects.
                    if !contains_reference(source) {
                        crate::structured::take(pointer, source, *kind, step.line)?;
                    }
                    binding(
                        &Some(name.clone()),
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    flow.bind_type(name, *kind)?;
                }
                StepKind::TakeJsonDynamic {
                    pointer,
                    source,
                    kind,
                    binding: name,
                } => {
                    check_value(pointer, &flow.names)?;
                    if let Value::Reference(reference) = pointer {
                        if flow
                            .types
                            .get(reference)
                            .is_some_and(|kind| *kind != crate::ast::InputType::Text)
                        {
                            bail!("dynamic JSON pointer {reference:?} requires text");
                        }
                    }
                    if let Value::String(pointer) = pointer {
                        crate::structured::validate_pointer(pointer)?;
                    }
                    check_value(source, &flow.names)?;
                    if !contains_reference(source) && !contains_reference(pointer) {
                        if let Value::String(pointer) = pointer {
                            crate::structured::take(pointer, source, *kind, step.line)?;
                        }
                    }
                    binding(
                        &Some(name.clone()),
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    flow.bind_type(name, *kind)?;
                }
                StepKind::EncodeJson { source } => {
                    check_value(source, &flow.names)?;
                    flow.has_data = true;
                    flow.stdout_unavailable = false;
                    flow.started = true;
                }
                StepKind::TaskGroup {
                    limit,
                    binding: name,
                    tasks,
                    ..
                } => {
                    if *limit == 0 {
                        bail!("task group limit must be greater than zero");
                    }
                    if tasks.is_empty() {
                        bail!("task group requires at least one task");
                    }
                    let mut task_names = BTreeSet::new();
                    for task in tasks {
                        if task.direct || task.cache || task.status_binding.is_some() {
                            bail!("task groups require named, uncached applet calls without status bindings");
                        }
                        let task_name = task
                            .binding
                            .as_deref()
                            .context("task group entries require an output name")?;
                        if task_name == name || !task_names.insert(task_name) {
                            bail!("task group output names must be distinct");
                        }
                        let manifest = registry.resolve_call(task)?;
                        if manifest.builtin.is_some() {
                            bail!("task groups cannot invoke interpreter builtins");
                        }
                        manifest.validate_with_types(task, &flow.types)?;
                        for value in task.parameters.values() {
                            check_value(value, &flow.names)?;
                        }
                    }
                    binding(
                        &Some(name.clone()),
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    flow.bind_type(name, crate::ast::InputType::Json)?;
                    flow.has_data = true;
                    flow.stdout_unavailable = false;
                    flow.started = true;
                }
                StepKind::Invoke { call, connection } => {
                    if *connection == Connection::Then && index == 0 && !initial_has_data {
                        bail!("then requires a preceding step");
                    }
                    let manifest = registry.resolve_call(call)?;
                    manifest.validate_with_types(call, &flow.types)?;
                    if call.binding.is_some()
                        && manifest.execution_contract().is_some()
                        && manifest.output_kind().is_none()
                    {
                        bail!(
                            "line {}: raw byte applet output cannot be named as a typed result",
                            step.line
                        );
                    }
                    if *connection == Connection::Then
                        && manifest.accepts_stdin
                        && flow.stdout_unavailable
                    {
                        bail!("line {}: then cannot consume stdout explicitly discarded or inherited by the preceding applet", step.line);
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    if manifest.execution_backend() != crate::contracts::AppletBackend::Command
                        && manifest.stdio().is_some_and(|stdio| {
                            stdio.stdout.is_some_and(|mode| {
                                mode != crate::contracts::StdioDisposition::Capture
                            }) || stdio.stderr.is_some_and(|mode| {
                                mode != crate::contracts::StdioDisposition::Capture
                            })
                        })
                    {
                        bail!(
                            "line {}: non-command backends support captured contract output only",
                            step.line
                        );
                    }
                    // Native pipe groups wire child descriptors directly and
                    // cannot host buffered in-process or JSON-lines stages.
                    // Reject the whole unsupported shape during validation,
                    // before any earlier workflow effects can run.
                    if *connection == Connection::Pipe || pipe_in_steps(steps, index + 1) {
                        let mut cursor = index;
                        while let Some(crate::ast::Step {
                            kind: StepKind::Invoke { call: stage, .. },
                            ..
                        }) = steps.get(cursor)
                        {
                            if cursor >= range.end {
                                break;
                            }
                            let stage_manifest = registry.resolve_call(stage)?;
                            if stage_manifest.execution_backend()
                                != crate::contracts::AppletBackend::Command
                            {
                                bail!(
                                    "line {}: {} backend cannot participate in a native pipe group",
                                    steps[cursor].line,
                                    match stage_manifest.execution_backend() {
                                        crate::contracts::AppletBackend::InProcess => "in_process",
                                        crate::contracts::AppletBackend::JsonLinesWorker =>
                                            "json_lines_worker",
                                        crate::contracts::AppletBackend::Command => "command",
                                        crate::contracts::AppletBackend::ProgramModule =>
                                            "program_module",
                                        crate::contracts::AppletBackend::Remote => "remote",
                                    }
                                );
                            }
                            cursor += 1;
                            if cursor >= range.end || !pipe_in_steps(steps, cursor) {
                                break;
                            }
                        }
                    }
                    for value in call.parameters.values() {
                        check_value(value, &flow.names)?;
                    }
                    if manifest.builtin == Some(Builtin::Cd) {
                        if *connection == Connection::Pipe || pipe_in_steps(steps, index + 1) {
                            bail!("cd changes the program directory and cannot belong to a pipe group");
                        }
                        if call.binding.is_some() {
                            bail!("cd has no output to name");
                        }
                    }
                    if *connection == Connection::Pipe {
                        if !flow.has_data {
                            bail!("pipe to requires a preceding data-producing step");
                        }
                        if !manifest.accepts_stdin {
                            bail!("applet {:?} does not accept piped input", manifest.name);
                        }
                        if (index == 0 && !initial_has_data)
                            || !matches!(
                                steps[index.saturating_sub(1)].kind,
                                StepKind::Read { .. }
                                    | StepKind::ReadStdin { .. }
                                    | StepKind::ReadCallInput { .. }
                                    | StepKind::Invoke { .. }
                                    | StepKind::EncodeJson { .. }
                            )
                        {
                            bail!("pipe to must immediately follow read or an applet");
                        }
                    }
                    if call.status_binding.is_some() {
                        if manifest.builtin.is_some() {
                            bail!("status capture requires an external native command");
                        }
                        if call.cache
                            || *connection == Connection::Pipe
                            || pipe_in_steps(steps, index + 1)
                        {
                            bail!("status capture requires a single uncached command outside a pipe group");
                        }
                        if call.status_binding == call.binding {
                            bail!("output and status require distinct binding names");
                        }
                    }
                    if call.cache {
                        if manifest.execution_backend()
                            == crate::contracts::AppletBackend::JsonLinesWorker
                        {
                            bail!("json_lines_worker applets cannot be cached");
                        }
                        if !manifest.cacheable {
                            bail!("applet {:?} is not declared cacheable", manifest.name);
                        }
                        if *connection == Connection::Pipe || pipe_in_steps(steps, index + 1) {
                            bail!("cache requires a finite then step, outside a live pipe group");
                        }
                    }
                    if call.binding.is_some() && pipe_in_steps(steps, index + 1) {
                        bail!("name results at the end of a pipe group; intermediate naming requires buffering");
                    }
                    let terminal_call = !pipe_in_steps(steps, index + 1);
                    let terminal_stdout_captured = !terminal_call
                        || manifest.stdio().is_none_or(|stdio| {
                            stdio.stdout.is_none_or(|mode| {
                                mode == crate::contracts::StdioDisposition::Capture
                            })
                        });
                    if terminal_call
                        && !terminal_stdout_captured
                        && (call.binding.is_some()
                            || call.cache
                            || manifest.output_kind().is_some())
                    {
                        bail!("line {}: discarded or inherited stdout cannot be bound, cached, or decoded as a typed output", step.line);
                    }
                    binding(
                        &call.binding,
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    if let Some(name) = &call.binding {
                        // Legacy applets still bind UTF-8 text. Versioned
                        // contracts can make this a statically known scalar or
                        // JSON value, which lets later conditions and JSON
                        // selections be checked before any command runs.
                        flow.bind_type(
                            name,
                            manifest
                                .output_kind()
                                .unwrap_or(crate::ast::InputType::Text),
                        )?;
                    }
                    binding(
                        &call.status_binding,
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    if let Some(name) = &call.status_binding {
                        flow.bind_type(name, crate::ast::InputType::Integer)?;
                    }
                    flow.has_data = manifest.builtin.is_none() && terminal_stdout_captured;
                    flow.stdout_unavailable = manifest.builtin.is_none()
                        && *connection == Connection::Then
                        && terminal_call
                        && !terminal_stdout_captured;
                    flow.started |= manifest.builtin.is_none();
                }
                StepKind::Save { path, connection } => {
                    check_path(path, &flow.names)?;
                    if !flow.has_data {
                        bail!("save requires preceding output");
                    }
                    if *connection == Connection::Pipe {
                        bail!("use then save for finite output");
                    }
                }
            }
            Ok(())
        })();
        checked.with_context(|| format!("line {}", step.line))?;
        index += 1;
    }
    Ok(())
}

pub fn validate_with_arguments(
    program: &Program,
    registry: &Registry,
    supplied: &BTreeMap<String, Value>,
) -> Result<()> {
    validate(program, registry)?;
    let known = crate::inputs::known_values(&program.inputs, supplied)?;
    preflight_inputs(program, registry, &known)
}

pub fn explain(program: &Program, registry: &Registry) -> Result<String> {
    explain_with_inputs(program, registry, &BTreeMap::new())
}

pub fn explain_with_inputs(
    program: &Program,
    registry: &Registry,
    supplied: &BTreeMap<String, Value>,
) -> Result<String> {
    validate(program, registry)?;
    let known = crate::inputs::known_values(&program.inputs, supplied)?;
    preflight_inputs(program, registry, &known)?;
    let mut rows = Vec::new();
    for input in &program.inputs {
        rows.push(format!(
            "line {}: accept {} as {}; {}",
            input.line,
            input.name,
            input.kind,
            input
                .default
                .as_ref()
                .map(|v| format!("default {v:?}"))
                .unwrap_or_else(|| "required input".into())
        ));
    }
    if let Some(trigger) = &program.trigger {
        let description = match trigger {
            Trigger::Every { interval_ms, .. } => {
                format!("every {interval_ms} milliseconds; monotonic, missed ticks skipped")
            }
            Trigger::Cron { expression, .. } => {
                format!("on schedule {expression:?}; local calendar, five fields")
            }
            Trigger::Listen {
                source: ListenSource::Stdin,
                binding,
                ..
            } => format!("listen to stdin; UTF-8 lines as {binding}"),
            Trigger::Listen {
                source: ListenSource::Applet(call),
                binding,
                ..
            } => {
                let manifest = registry.resolve_call(call)?;
                let argv = match arguments_for_call(call, manifest, &known) {
                    Ok(args) => serde_json::to_string(&args)?,
                    Err(_) => format!(
                        "parameters {:?} (resolve program inputs at runtime)",
                        call.parameters
                    ),
                };
                format!(
                    "listen to {:?} argv={}; UTF-8 lines as {binding}",
                    manifest.binary_for(call),
                    argv
                )
            }
        };
        rows.push(format!(
            "trigger: {description}; reuse parsed body, sequential handlers"
        ));
        if matches!(trigger, Trigger::Listen { .. }) {
            rows.push("listener: producer stays alive; native handler commands launch per record; emitted handler output goes to stdout; EOF, signal, failure or --max-runs stops the service".into());
        }
    }
    for step in &program.steps {
        let description = match &step.kind {
            StepKind::Loop { kind, .. } => format!(
                "loop {kind:?}; shared {} entered-iteration limit",
                loops::MAX_LOOP_ITERATIONS
            ),
            StepKind::EndLoop { .. } => "end iteration; remove body-local names".into(),
            StepKind::If { condition, .. } => format!(
                "if {:?} {}{}{:?}",
                condition.left,
                if condition.negated { "not " } else { "" },
                match condition.operator {
                    crate::ast::ConditionOperator::Equals => "is ",
                    crate::ast::ConditionOperator::Contains => "contains ",
                },
                condition.right
            ),
            StepKind::Otherwise { .. } => "otherwise".into(),
            StepKind::EndIf => "end conditional".into(),
            StepKind::CallProgram { call, .. } => format!(
                "call program {:?}; explicit inputs {:?}; local scope",
                call.operation, call.parameters
            ),
            StepKind::EndCall => "return program output; restore caller scope and cwd".into(),
            StepKind::BreakLoop => "break the innermost loop; restore its outer bindings".into(),
            StepKind::ContinueLoop => {
                "finish the current iteration and continue the innermost loop".into()
            }
            StepKind::Return => "return from the current reusable program".into(),
            StepKind::Try { catch, finally, .. } => format!(
                "try body; catch={}; finally={}; cleanup is bounded",
                catch.is_some(),
                finally.is_some()
            ),
            StepKind::Catch { binding, .. } => {
                format!("catch a structured error record as {binding}; skip on success")
            }
            StepKind::Finally => {
                "run bounded cleanup on normal exit, transfer, failure or cancellation".into()
            }
            StepKind::EndTry => "complete try block; resume pending transfer or error".into(),
            StepKind::ReadCallInput { .. } => {
                "read reusable program input; preserve byte transport".into()
            }
            StepKind::UseFolder { path } => format!(
                "use folder {:?}",
                resolve(path, &known).unwrap_or_else(|_| path.clone())
            ),
            StepKind::Read { path, .. } => format!(
                "read {:?}",
                resolve(path, &known).unwrap_or_else(|_| path.clone())
            ),
            StepKind::ReadStdin { .. } => {
                "read inherited stdin; native pipe input or bounded finite capture".into()
            }
            StepKind::ParseJson { binding } => format!(
                "parse bounded finite output as JSON, named {binding}; preserve original bytes"
            ),
            StepKind::TakeJson {
                pointer,
                source,
                kind,
                binding,
            } => format!("take {kind} at {pointer:?} from {source:?} as {binding}"),
            StepKind::TakeJsonDynamic {
                pointer,
                source,
                kind,
                binding,
            } => format!("take {kind} at dynamic pointer {pointer:?} from {source:?} as {binding}"),
            StepKind::EncodeJson { source } => {
                format!("encode {source:?} as compact JSON; bounded finite output")
            }
            StepKind::TaskGroup {
                mode,
                limit,
                binding,
                tasks,
            } => format!(
                "{mode:?} task group (limit {limit}) with {} applets as {binding}",
                tasks.len()
            ),
            StepKind::Save { path, .. } => format!(
                "save finite output to {:?}",
                resolve(path, &known).unwrap_or_else(|_| path.clone())
            ),
            StepKind::Invoke { call, connection } => {
                let manifest = registry.resolve_call(call)?;
                let argv = match arguments_for_call(call, manifest, &known) {
                    Ok(argv) => serde_json::to_string(&argv)?,
                    Err(_) => format!(
                        "parameters {:?} (resolve named results at runtime)",
                        call.parameters
                    ),
                };
                format!(
                    "{connection:?}: {:?} argv={argv}{}{}{}{}",
                    manifest.binary_for(call),
                    if manifest.builtin.is_some() {
                        " [interpreter builtin]"
                    } else {
                        ""
                    },
                    if call.cache { " [cache]" } else { "" },
                    call.binding
                        .as_ref()
                        .map(|name| format!(" as {name}"))
                        .unwrap_or_default(),
                    call.status_binding
                        .as_ref()
                        .map(|name| format!(" and keep status as {name}"))
                        .unwrap_or_default()
                )
            }
        };
        rows.push(format!("line {}: {description}", step.line));
    }
    Ok(rows.join("\n"))
}

fn resolve(value: &Value, bindings: &BTreeMap<String, Value>) -> Result<Value> {
    Ok(match value {
        Value::Reference(name) => bindings
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("undefined result {name:?}"))?,
        Value::List(values) => Value::List(
            values
                .iter()
                .map(|v| resolve(v, bindings))
                .collect::<Result<_>>()?,
        ),
        Value::Record(values) => Value::Record(
            values
                .iter()
                .map(|(name, value)| Ok((name.clone(), resolve(value, bindings)?)))
                .collect::<Result<_>>()?,
        ),
        value => value.clone(),
    })
}
fn contains_reference(value: &Value) -> bool {
    match value {
        Value::Reference(_) => true,
        Value::List(values) => values.iter().any(contains_reference),
        Value::Record(values) => values.values().any(contains_reference),
        _ => false,
    }
}
fn structured_source<'a>(
    value: &'a Value,
    bindings: &'a BTreeMap<String, Value>,
) -> Result<Cow<'a, Value>> {
    if matches!(value, Value::Reference(_)) || !contains_reference(value) {
        Ok(Cow::Borrowed(crate::structured::source(value, bindings)?))
    } else {
        Ok(Cow::Owned(resolve(value, bindings)?))
    }
}
#[cfg(not(target_arch = "wasm32"))]
enum GroupArguments<'a> {
    Owned(Vec<String>),
    Borrowed(Vec<Cow<'a, str>>),
}

#[cfg(not(target_arch = "wasm32"))]
fn arguments_for_group_call<'a>(
    call: &'a Invocation,
    manifest: &'a Manifest,
    bindings: &BTreeMap<String, Value>,
) -> Result<GroupArguments<'a>> {
    if call.cache || call.parameters.values().any(contains_reference) {
        Ok(GroupArguments::Owned(arguments_for_call(
            call, manifest, bindings,
        )?))
    } else {
        Ok(GroupArguments::Borrowed(
            manifest.build_arguments_borrowed(&call.parameters)?,
        ))
    }
}

fn arguments_for_call(
    call: &Invocation,
    manifest: &Manifest,
    bindings: &BTreeMap<String, Value>,
) -> Result<Vec<String>> {
    // The common literal-only case can borrow the invocation map directly.
    // Resolving every value into a second BTreeMap cloned keys, strings and
    // lists even when there was nothing to substitute.
    if !call.parameters.values().any(contains_reference) {
        return manifest.build_arguments(&call.parameters);
    }
    let params = call
        .parameters
        .iter()
        .map(|(key, value)| Ok((key.clone(), resolve(value, bindings)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    manifest.build_arguments(&params)
}
fn path_value(value: &Value, bindings: &BTreeMap<String, Value>, cwd: &Path) -> Result<PathBuf> {
    let path = match value {
        Value::String(path) => path.as_str(),
        Value::Reference(name) => match bindings.get(name) {
            Some(Value::String(path)) => path.as_str(),
            Some(_) => bail!("path result must contain text"),
            None => return Err(anyhow!("undefined result {name:?}")),
        },
        _ => bail!("path result must contain text"),
    };
    Ok(cwd.join(path))
}
