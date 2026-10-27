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

fn polymorphic_output_kind(
    manifest: &crate::manifest::Manifest,
    call: &Invocation,
    types: &BTreeMap<String, crate::ast::InputType>,
) -> Option<crate::ast::InputType> {
    let output = manifest.execution_contract()?.output.as_ref()?;
    let parameter = output.same_as_parameter.as_deref()?;
    let value = call.parameters.get(parameter)?;
    let item_kind = match value {
        Value::Reference(name) => match types.get(name).copied()? {
            crate::ast::InputType::TextList => crate::ast::InputType::Text,
            crate::ast::InputType::IntegerList => crate::ast::InputType::Integer,
            crate::ast::InputType::UnsignedIntegerList => crate::ast::InputType::UnsignedInteger,
            crate::ast::InputType::UsizeList => crate::ast::InputType::Usize,
            _ => crate::ast::InputType::Json,
        },
        Value::List(items) => {
            let item_type = |item: &Value| match item {
                Value::String(text) if text.contains('\0') => Some(crate::ast::InputType::Json),
                Value::String(_) => Some(crate::ast::InputType::Text),
                Value::Integer(_) => Some(crate::ast::InputType::Integer),
                Value::Number(number) if number.as_i64().is_some() => {
                    Some(crate::ast::InputType::Integer)
                }
                Value::Number(number)
                    if number
                        .as_u64()
                        .is_some_and(|value| value <= usize::MAX as u64) =>
                {
                    Some(crate::ast::InputType::Usize)
                }
                Value::Number(number) if number.as_u64().is_some() => {
                    Some(crate::ast::InputType::UnsignedInteger)
                }
                Value::Bool(_) => Some(crate::ast::InputType::Boolean),
                Value::Reference(name) => Some(
                    types
                        .get(name)
                        .copied()
                        .unwrap_or(crate::ast::InputType::Json),
                ),
                _ => Some(crate::ast::InputType::Json),
            };
            let mut kinds = items.iter().filter_map(item_type);
            let first = kinds.next().unwrap_or(crate::ast::InputType::Json);
            if kinds.all(|kind| kind == first) {
                first
            } else {
                crate::ast::InputType::Json
            }
        }
        _ => return None,
    };
    Some(match output.kind {
        crate::contracts::OutputType::List => match item_kind {
            crate::ast::InputType::Text => crate::ast::InputType::TextList,
            crate::ast::InputType::Integer => crate::ast::InputType::IntegerList,
            crate::ast::InputType::UnsignedInteger => crate::ast::InputType::UnsignedIntegerList,
            crate::ast::InputType::Usize => crate::ast::InputType::UsizeList,
            _ => crate::ast::InputType::JsonList,
        },
        crate::contracts::OutputType::Json => item_kind,
        _ => return None,
    })
}
/// Exit status operands are literal integers in range or integer bindings.
/// The range check happens before any effect; a binding is type-checked here.
fn validate_exit_operand(
    value: &Value,
    flow: &control::FlowState,
    minimum: i64,
    what: &str,
) -> Result<()> {
    check_value(value, &flow.names)?;
    match value {
        Value::Integer(status) if (minimum..=255).contains(status) => Ok(()),
        Value::Integer(status) => {
            bail!("{what} must be between {minimum} and 255, received {status}")
        }
        Value::Reference(name) => match flow.types.get(name) {
            Some(crate::ast::InputType::Integer | crate::ast::InputType::Never) => Ok(()),
            Some(kind) => bail!("{what} {name:?} must be integer, received {kind}"),
            None => bail!("{what} {name:?} has no known integer type"),
        },
        _ => bail!("{what} must be an integer literal or integer binding"),
    }
}
/// The outcome of a condition whose operands are both literals, or `None` when it
/// depends on a run-time value. Mirrors the run's equality and containment rules;
/// a type mismatch is `None`, so the run reports it when it reaches the condition.
#[cfg(not(target_arch = "wasm32"))]
fn static_condition(condition: &crate::ast::Condition) -> Option<bool> {
    let is_scalar = |value: &Value| {
        matches!(
            value,
            Value::String(_) | Value::Integer(_) | Value::Bool(_) | Value::Number(_) | Value::Null
        )
    };
    let matched = match (&condition.left, &condition.right, &condition.operator) {
        (
            Value::String(haystack),
            Value::String(needle),
            crate::ast::ConditionOperator::Contains,
        ) => haystack.contains(needle.as_str()),
        (left, right, crate::ast::ConditionOperator::Equals)
            if is_scalar(left)
                && is_scalar(right)
                && (matches!(left, Value::Null)
                    || matches!(right, Value::Null)
                    || std::mem::discriminant(left) == std::mem::discriminant(right)) =>
        {
            left == right
        }
        _ => return None,
    };
    Some(matched != condition.negated)
}

/// Whether a loop's body runs at least once when the loop is reached, so its
/// executables are needed before the run starts.
#[cfg(not(target_arch = "wasm32"))]
fn loop_runs_once(kind: &crate::ast::LoopKind) -> bool {
    use crate::ast::LoopKind;
    match kind {
        LoopKind::Repeat {
            count: Value::Integer(count),
        } => *count >= 1,
        LoopKind::ForEach {
            values: Value::List(items),
            ..
        } => !items.is_empty(),
        LoopKind::While { condition } => static_condition(condition) == Some(true),
        _ => false,
    }
}

/// Whether a region of steps changes the working directory, directly or through
/// a `use folder`, when it runs.
#[cfg(not(target_arch = "wasm32"))]
fn region_changes_directory(registry: &Registry, steps: &[crate::ast::Step]) -> Result<bool> {
    for step in steps {
        match &step.kind {
            StepKind::UseFolder { .. } => return Ok(true),
            StepKind::Invoke { call, .. }
                if registry.resolve_call(call)?.builtin == Some(Builtin::Cd) =>
            {
                return Ok(true);
            }
            _ => {}
        }
    }
    Ok(false)
}

#[cfg(not(target_arch = "wasm32"))]
fn region_changes_workflow_environment(steps: &[crate::ast::Step]) -> bool {
    steps.iter().any(|step| {
        matches!(
            &step.kind,
            StepKind::Environment {
                workflow_scope: true,
                ..
            }
        )
    })
}

/// Skip a branch or loop body that may not run. Its executables are resolved when
/// the run reaches it, so a missing one there is reported at that point. A
/// working-directory change inside it still makes later relative names unknown.
#[cfg(not(target_arch = "wasm32"))]
fn skip_unknown_region(
    registry: &Registry,
    steps: &[crate::ast::Step],
    start: usize,
    end: usize,
    known_cwd: &mut bool,
) -> Result<usize> {
    if region_changes_directory(registry, &steps[start..end])? {
        *known_cwd = false;
    }
    Ok(end)
}

#[cfg(not(target_arch = "wasm32"))]
fn known_folder(value: &crate::ast::Value, cwd: &Path, cwd_known: bool) -> Option<PathBuf> {
    let crate::ast::Value::String(path) = value else {
        return None;
    };
    let path = Path::new(path);
    if !path.is_absolute() && !cwd_known {
        return None;
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let path = std::fs::canonicalize(path).ok()?;
    path.is_dir().then_some(path)
}

/// Resolve every executable the run is certain to reach, before the first effect,
/// and report all unresolved ones together. Top-level commands, task groups and
/// jobs count, and so do branches and loop bodies whose condition is statically
/// known to take them. A branch or loop body that may not run is left to the run,
/// so a tool needed only on another path does not block this one. Literal folder
/// changes are followed; dynamic or conditional changes leave later relative names
/// to their run-time resolution.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn preflight_executables(
    program: &Program,
    registry: &Registry,
    cwd: &Path,
) -> Result<()> {
    let mut known_cwd = true;
    let mut current_cwd = cwd.to_path_buf();
    let mut cwd_scopes = Vec::<(PathBuf, bool)>::new();
    let mut missing = Vec::new();
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    // `None` means no per-command PATH change; `Some(None)` means PATH comes
    // from a runtime binding and cannot be checked before that binding exists.
    let mut command_path: Option<Option<std::ffi::OsString>> = None;
    let mut workflow_path: Option<Option<std::ffi::OsString>> = None;
    let mut workflow_path_scopes = Vec::new();
    let check = |call: &Invocation,
                 line: usize,
                 current_cwd: &Path,
                 path: &std::ffi::OsStr,
                 missing: &mut Vec<String>|
     -> Result<()> {
        let manifest = registry.resolve_call(call)?;
        if manifest.builtin.is_some()
            || manifest.execution_backend() != crate::contracts::AppletBackend::Command
        {
            return Ok(());
        }
        let binary = manifest.binary_for(call);
        if binary.starts_with('@') {
            return Ok(());
        }
        let resolution = if binary.contains('/')
            || (cfg!(windows) && binary.contains('\\'))
            || Path::new(binary).is_absolute()
        {
            binary_path(binary, current_cwd)
        } else {
            binary_path_from_path(binary, current_cwd, path)
        };
        if let Err(error) = resolution {
            let suggestion = if call.direct {
                executable_suggestion(binary, current_cwd)
            } else {
                None
            };
            let message = match suggestion {
                Some(name) => format!("line {line}: {error:#}; did you mean {name:?}?"),
                None => format!("line {line}: {error:#}"),
            };
            missing.push(message);
        }
        Ok(())
    };
    let steps = &program.steps;
    let mut index = 0;
    while index < steps.len() {
        let step = &steps[index];
        index = match &step.kind {
            StepKind::UseFolder { path } => {
                if let Some(folder) = known_folder(path, &current_cwd, known_cwd) {
                    current_cwd = folder;
                    known_cwd = true;
                } else {
                    known_cwd = false;
                }
                index + 1
            }
            StepKind::Invoke { call, .. } => {
                if registry.resolve_call(call)?.builtin == Some(Builtin::Cd) {
                    if let Some(path) = call.parameters.get("path") {
                        if let Some(folder) = known_folder(path, &current_cwd, known_cwd) {
                            current_cwd = folder;
                            known_cwd = true;
                        } else {
                            known_cwd = false;
                        }
                    } else {
                        known_cwd = false;
                    }
                } else if known_cwd {
                    let effective_path = command_path.take().or_else(|| workflow_path.clone());
                    if let Some(Some(path)) = effective_path.as_ref() {
                        check(call, step.line, &current_cwd, path, &mut missing)?;
                    } else if effective_path.is_none() {
                        check(call, step.line, &current_cwd, &inherited_path, &mut missing)?;
                    }
                } else {
                    command_path = None;
                }
                index + 1
            }
            StepKind::Environment {
                set,
                unset,
                workflow_scope,
            } => {
                let path_update = if let Some((_, value)) = set.iter().rev().find(|(name, _)| {
                    name.as_str() == "PATH" || (cfg!(windows) && name.eq_ignore_ascii_case("PATH"))
                }) {
                    Some(match value {
                        crate::ast::Value::String(path) => Some(std::ffi::OsString::from(path)),
                        _ => None,
                    })
                } else if unset.iter().any(|name| {
                    name.as_str() == "PATH" || (cfg!(windows) && name.eq_ignore_ascii_case("PATH"))
                }) {
                    Some(Some(std::ffi::OsString::new()))
                } else {
                    None
                };
                if *workflow_scope {
                    if let Some(update) = path_update {
                        workflow_path = Some(update);
                    }
                } else {
                    command_path = path_update;
                }
                index + 1
            }
            StepKind::TaskGroup { tasks, .. } => {
                if known_cwd {
                    if let Some(Some(path)) = workflow_path.as_ref() {
                        for call in tasks {
                            check(call, step.line, &current_cwd, path, &mut missing)?;
                        }
                    } else if workflow_path.is_none() {
                        for call in tasks {
                            check(call, step.line, &current_cwd, &inherited_path, &mut missing)?;
                        }
                    }
                }
                index + 1
            }
            StepKind::SpawnJob { stages, .. } => {
                if known_cwd {
                    if let Some(Some(path)) = workflow_path.as_ref() {
                        for stage in stages {
                            check(&stage.call, stage.line, &current_cwd, path, &mut missing)?;
                        }
                    } else if workflow_path.is_none() {
                        for stage in stages {
                            check(
                                &stage.call,
                                stage.line,
                                &current_cwd,
                                &inherited_path,
                                &mut missing,
                            )?;
                        }
                    }
                }
                index + 1
            }
            // The command always runs; its branches depend on its outcome.
            StepKind::IfCommand { call, end, .. } => {
                if known_cwd {
                    if let Some(Some(path)) = workflow_path.as_ref() {
                        check(call, step.line, &current_cwd, path, &mut missing)?;
                    } else if workflow_path.is_none() {
                        check(call, step.line, &current_cwd, &inherited_path, &mut missing)?;
                    }
                }
                let region = &steps[index + 1..index + end];
                let next =
                    skip_unknown_region(registry, steps, index + 1, index + end, &mut known_cwd)?;
                if region_changes_workflow_environment(region) {
                    workflow_path = Some(None);
                }
                next
            }
            StepKind::If {
                condition,
                otherwise,
                end,
            } => match static_condition(condition) {
                Some(true) => index + 1,
                Some(false) => index + otherwise,
                None => {
                    let region = &steps[index + 1..index + end];
                    let next = skip_unknown_region(
                        registry,
                        steps,
                        index + 1,
                        index + end,
                        &mut known_cwd,
                    )?;
                    if region_changes_workflow_environment(region) {
                        workflow_path = Some(None);
                    }
                    next
                }
            },
            // Reached only after a taken `then` body, so the `otherwise` body is not run.
            StepKind::Otherwise { end } => index + end,
            // The catch body runs only after a failure in the try body.
            StepKind::Catch { end, .. } => {
                let region = &steps[index..index + end];
                let next =
                    skip_unknown_region(registry, steps, index, index + end, &mut known_cwd)?;
                if region_changes_workflow_environment(region) {
                    workflow_path = Some(None);
                }
                next
            }
            StepKind::Loop { kind, end } => {
                if let crate::ast::LoopKind::WhileCommand { call, .. } = kind {
                    if known_cwd {
                        if let Some(Some(path)) = workflow_path.as_ref() {
                            check(call, step.line, &current_cwd, path, &mut missing)?;
                        } else if workflow_path.is_none() {
                            check(call, step.line, &current_cwd, &inherited_path, &mut missing)?;
                        }
                    }
                }
                if loop_runs_once(kind) {
                    index + 1
                } else {
                    let region = &steps[index + 1..index + end];
                    let next = skip_unknown_region(
                        registry,
                        steps,
                        index + 1,
                        index + end,
                        &mut known_cwd,
                    )?;
                    if region_changes_workflow_environment(region) {
                        workflow_path = Some(None);
                    }
                    next
                }
            }
            StepKind::CallProgram { .. } => {
                cwd_scopes.push((current_cwd.clone(), known_cwd));
                workflow_path_scopes.push(workflow_path.clone());
                index + 1
            }
            StepKind::EndCall => {
                if let Some((saved_cwd, saved_known)) = cwd_scopes.pop() {
                    current_cwd = saved_cwd;
                    known_cwd = saved_known;
                }
                if let Some(saved_path) = workflow_path_scopes.pop() {
                    workflow_path = saved_path;
                }
                index + 1
            }
            _ => index + 1,
        };
    }
    if !missing.is_empty() {
        bail!(
            "executables unavailable before the run started: {}",
            missing.join("; ")
        );
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn executable_suggestion(binary: &str, cwd: &Path) -> Option<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut names = std::collections::BTreeSet::<String>::new();
    for directory in std::env::split_paths(&path) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        };
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let executable = entry
                .metadata()
                .is_ok_and(|metadata| metadata.is_file() && crate::platform::executable(&metadata));
            if executable {
                if let Some(name) = executable_candidate_name(&file_name) {
                    names.insert(name);
                }
            }
        }
    }
    crate::suggest::closest(binary, names.iter().map(String::as_str)).map(str::to_owned)
}

#[cfg(not(target_arch = "wasm32"))]
fn executable_candidate_name(file_name: &std::ffi::OsStr) -> Option<String> {
    let name = file_name.to_str()?;
    if cfg!(windows) {
        let extension = Path::new(name)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("");
        if extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd") {
            return None;
        }
        if extension.eq_ignore_ascii_case("exe") {
            return Path::new(name)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned);
        }
    }
    Some(name.to_owned())
}

#[cfg(all(test, windows))]
mod executable_candidate_tests {
    use super::executable_candidate_name;
    use std::ffi::OsStr;

    #[test]
    fn skips_batch_files_and_uses_exe_stems() {
        assert_eq!(executable_candidate_name(OsStr::new("tool.bat")), None);
        assert_eq!(executable_candidate_name(OsStr::new("tool.cmd")), None);
        assert_eq!(
            executable_candidate_name(OsStr::new("printf.exe")),
            Some("printf".into())
        );
    }
}

/// `interactively` (terminal handoff) is valid only for one fresh native command
/// at the top level of a finite run. Loops, try blocks, and reusable-program bodies
/// nest it, so they are rejected here, before any effect.
fn validate_terminal_handoffs(program: &Program, registry: &Registry) -> Result<()> {
    let mut nesting = 0usize;
    for (index, step) in program.steps.iter().enumerate() {
        match &step.kind {
            StepKind::Loop { .. } | StepKind::Try { .. } | StepKind::CallProgram { .. } => {
                nesting += 1;
            }
            StepKind::EndLoop { .. } | StepKind::EndTry | StepKind::EndCall => {
                nesting = nesting.saturating_sub(1);
            }
            StepKind::Interactive => {
                if nesting > 0 {
                    bail!(
                        "line {}: interactively is valid only at the top level of a finite run, not inside a loop, try block, or reusable program",
                        step.line
                    );
                }
                let Some(crate::ast::Step {
                    kind: StepKind::Invoke { call, connection },
                    ..
                }) = program.steps.get(index + 1)
                else {
                    bail!(
                        "line {}: interactively must precede one native command",
                        step.line
                    );
                };
                if *connection != Connection::Start || pipe_in_steps(&program.steps, index + 2) {
                    bail!(
                        "line {}: interactively applies to a single fresh command",
                        step.line
                    );
                }
                let manifest = registry.resolve_call(call)?;
                if manifest.builtin.is_some()
                    || manifest.execution_backend() != crate::contracts::AppletBackend::Command
                {
                    bail!(
                        "line {}: interactively applies to a native command, not a builtin or in-process applet",
                        step.line
                    );
                }
                if call.cache
                    || call.binding.is_some()
                    || call.status_binding.is_some()
                    || call.result_binding.is_some()
                    || call.discard_stderr
                {
                    bail!(
                        "line {}: interactively cannot be combined with cache, as, keep status, keep result, or discard stderr",
                        step.line
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// A command condition is an ordinary applet or executable call. It is type
/// checked like any call; builtins change shell state and are not conditions.
fn validate_command_condition(
    call: &Invocation,
    registry: &Registry,
    flow: &control::FlowState,
) -> Result<()> {
    let manifest = registry.resolve_call(call)?;
    if manifest.builtin.is_some() {
        bail!(
            "a command condition cannot run the builtin {:?}",
            manifest.name_for(call)
        );
    }
    manifest.validate_with_types(call, &flow.types)
}
/// Render a value in the stable, source-like form used by text `explain`.
/// References intentionally stay bare so unresolved bindings are easy to spot.
fn describe_value(value: &Value) -> String {
    match value {
        Value::String(text) => serde_json::to_string(text).expect("strings always serialize"),
        Value::Integer(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::List(values) => format!(
            "[{}]",
            values
                .iter()
                .map(describe_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Record(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(name, value)| format!(
                    "{}: {}",
                    describe_value(&Value::String(name.clone())),
                    describe_value(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Null => "null".into(),
        Value::Number(number) => number.to_string(),
        Value::Reference(name) => name.clone(),
    }
}

fn describe_resolved_value(value: &Value, known: &BTreeMap<String, Value>) -> String {
    describe_value(&resolve(value, known).unwrap_or_else(|_| value.clone()))
}

/// A file path as `explain` shows it: resolved text, or the unresolved value.
fn describe_path(value: &Value, known: &BTreeMap<String, Value>) -> String {
    describe_resolved_value(value, known)
}

fn describe_condition(condition: &crate::ast::Condition) -> String {
    format!(
        "{} {}{}{}",
        describe_value(&condition.left),
        if condition.negated { "not " } else { "" },
        match condition.operator {
            crate::ast::ConditionOperator::Equals => "is ",
            crate::ast::ConditionOperator::Contains => "contains ",
        },
        describe_value(&condition.right)
    )
}

fn describe_loop_kind(kind: &crate::ast::LoopKind) -> String {
    use crate::ast::LoopKind;
    match kind {
        LoopKind::Repeat { count } => format!("repeat {}", describe_value(count)),
        LoopKind::ForEach { binding, values } => {
            format!("for each {binding} in {}", describe_value(values))
        }
        LoopKind::While { condition } => format!("while {}", describe_condition(condition)),
        LoopKind::WhileCommand {
            call,
            expect_success,
        } => format!(
            "while run {:?} {}",
            call.operation,
            if *expect_success { "succeeds" } else { "fails" }
        ),
    }
}

fn describe_parameters(parameters: &BTreeMap<String, Value>) -> String {
    format!(
        "{{{}}}",
        parameters
            .iter()
            .map(|(name, value)| format!("{name}: {}", describe_value(value)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}
/// Short, source-like rendering of an exit or fail operand for `explain`.
fn describe_operand(value: &Value) -> String {
    match value {
        Value::Integer(number) => number.to_string(),
        Value::String(text) => format!("{text:?}"),
        Value::Reference(name) => format!("binding {name}"),
        other => format!("{other:?}"),
    }
}
/// Fail messages are quoted text or text bindings; nothing is coerced.
fn validate_text_operand(value: &Value, flow: &control::FlowState, what: &str) -> Result<()> {
    check_value(value, &flow.names)?;
    match value {
        Value::String(_) => Ok(()),
        Value::Reference(name) => match flow.types.get(name) {
            Some(crate::ast::InputType::Text | crate::ast::InputType::Never) => Ok(()),
            Some(kind) => bail!("{what} {name:?} must be text, received {kind}"),
            None => bail!("{what} {name:?} has no known text type"),
        },
        _ => bail!("{what} must be a quoted text literal or text binding"),
    }
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
    calls::preflight(program, registry, bindings, None)?;
    Ok(())
}

fn preflight_inputs_with_loop_limit(
    program: &Program,
    registry: &Registry,
    bindings: &BTreeMap<String, Value>,
    limit: usize,
) -> Result<()> {
    if program.inputs.is_empty()
        && !program.steps.iter().any(|step| {
            matches!(
                step.kind,
                StepKind::Loop { .. } | StepKind::CallProgram { .. }
            )
        })
    {
        return Ok(());
    }
    calls::preflight(program, registry, bindings, Some(limit))
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

/// Whether this loop body contains a transfer that can bypass the rest of the
/// current iteration. Nested loops and reusable-program bodies own their own
/// break/continue scope, so their transfers do not make this loop's summary
/// unreachable.
fn loop_body_has_early_transfer(steps: &[crate::ast::Step], range: std::ops::Range<usize>) -> bool {
    fn scan(
        steps: &[crate::ast::Step],
        range: std::ops::Range<usize>,
        nested_loops: usize,
        called_programs: usize,
    ) -> bool {
        let mut index = range.start;
        while index < range.end {
            let step = &steps[index];
            match &step.kind {
                StepKind::Loop { end, .. } => {
                    let after_block = index.saturating_add(*end).min(range.end);
                    if after_block > index + 1
                        && scan(
                            steps,
                            index + 1..after_block - 1,
                            nested_loops + 1,
                            called_programs,
                        )
                    {
                        return true;
                    }
                    index = if after_block > index {
                        after_block
                    } else {
                        index + 1
                    };
                }
                StepKind::CallProgram { end, .. } => {
                    let after_block = index.saturating_add(*end).min(range.end);
                    if after_block > index + 1
                        && scan(
                            steps,
                            index + 1..after_block - 1,
                            nested_loops,
                            called_programs + 1,
                        )
                    {
                        return true;
                    }
                    index = if after_block > index {
                        after_block
                    } else {
                        index + 1
                    };
                }
                StepKind::BreakLoop | StepKind::ContinueLoop
                    if nested_loops == 0 && called_programs == 0 =>
                {
                    return true;
                }
                StepKind::Return if called_programs == 0 => return true,
                StepKind::Exit { .. } | StepKind::Fail { .. } => return true,
                _ => index += 1,
            }
        }
        false
    }

    scan(steps, range, 0, 0)
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

pub(super) fn has_job_operations(program: &Program) -> bool {
    program
        .steps
        .iter()
        .chain(
            program
                .definitions
                .values()
                .flat_map(|definition| &definition.steps),
        )
        .any(|step| {
            matches!(
                step.kind,
                StepKind::SpawnJob { .. }
                    | StepKind::InspectJob { .. }
                    | StepKind::AwaitJob { .. }
                    | StepKind::CancelJob { .. }
            )
        })
}

/// Validate the complete script before launching any command or saving files.
pub fn validate(program: &Program, registry: &Registry) -> Result<()> {
    validate_discard_stderr_usage(program)?;
    validate_secret_arguments(program, registry)?;
    crate::inputs::validate(&program.inputs)?;
    let has_jobs = has_job_operations(program);
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
    #[cfg(target_arch = "wasm32")]
    if has_jobs {
        bail!("browser host does not support native background jobs");
    }
    #[cfg(not(target_arch = "wasm32"))]
    validate_worker_support(program, registry)?;
    #[cfg(target_arch = "wasm32")]
    if program.trigger.is_some() {
        bail!(
            "browser host supports finite programs; use the native runtime for schedules and listeners"
        );
    }
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(trigger) = &program.trigger {
        if has_jobs {
            bail!(
                "native background jobs are supported only in finite programs; services and listeners have separate lifetimes"
            );
        }
        let (setup, body) = service::split_program(program);
        if let Some(step) = body
            .steps
            .iter()
            .find(|step| matches!(step.kind, StepKind::Interactive))
        {
            bail!(
                "line {}: interactively is not supported in service or listener handlers",
                step.line
            );
        }
        // A handler failure is recoverable per record, and exit would end the
        // whole service. Keep those lifetimes separate: fail the program here.
        if let Some(step) = body
            .steps
            .iter()
            .find(|step| matches!(step.kind, StepKind::Exit { .. } | StepKind::Fail { .. }))
        {
            bail!(
                "line {}: exit and fail end a finite run; service and listener handlers cannot use them",
                step.line
            );
        }
        if let Some(step) = body
            .steps
            .iter()
            .find(|step| matches!(step.kind, StepKind::ReadStdin { .. }))
        {
            bail!(
                "line {}: read from stdin is for finite programs; use listen to stdin for service records",
                step.line
            );
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
                    if call.cache
                        || call.binding.is_some()
                        || call.status_binding.is_some()
                        || call.result_binding.is_some()
                    {
                        bail!("listener sources cannot be cached or bind output/status/result");
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

/// Secret bindings may be delivered through stdin or a command environment,
/// but a value in an invocation parameter becomes an argv value. Reject that
/// flow before execution, where process listings and applet diagnostics could
/// expose it.
fn validate_secret_arguments(program: &Program, registry: &Registry) -> Result<()> {
    fn references(value: &Value, secrets: &BTreeSet<String>) -> Option<String> {
        match value {
            Value::Reference(name) if secrets.contains(name) => Some(name.clone()),
            Value::List(values) => values.iter().find_map(|value| references(value, secrets)),
            Value::Record(values) => values.values().find_map(|value| references(value, secrets)),
            _ => None,
        }
    }

    fn check_call(
        call: &Invocation,
        line: usize,
        program_name: Option<&str>,
        secrets: &BTreeSet<String>,
    ) -> Result<()> {
        if let Some(name) = call
            .parameters
            .values()
            .find_map(|value| references(value, secrets))
        {
            let context =
                program_name.map_or_else(String::new, |name| format!("program {name:?}, "));
            bail!(
                "{context}line {line}: secret binding {name:?} cannot be passed as a command argument; use stdin or `and environment`"
            );
        }
        Ok(())
    }

    let scopes = std::iter::once((None, &program.inputs, program.steps.as_slice())).chain(
        program.definitions.iter().map(|(name, definition)| {
            (
                Some(name.as_str()),
                &definition.inputs,
                definition.steps.as_slice(),
            )
        }),
    );
    for (program_name, inputs, steps) in scopes {
        let mut secrets = inputs
            .iter()
            .filter(|input| crate::ast::is_sensitive(input))
            .map(|input| input.name.clone())
            .collect::<BTreeSet<_>>();
        let mut pipeline_secret = false;
        let mut pending_environment_secret = false;
        let mut workflow_environment_secret = false;
        let mut workflow_environment_scopes = Vec::new();
        for step in steps {
            match &step.kind {
                StepKind::Invoke { call, connection } => {
                    if call.marks_sensitive_output() && call.binding.is_none() {
                        bail!(
                            "line {}: sensitive output requires a named binding",
                            step.line
                        );
                    }
                    if call.marks_sensitive_output() && call.cache {
                        bail!("line {}: sensitive output cannot be cached", step.line);
                    }
                    if call.marks_sensitive_output() && call.result_binding.is_some() {
                        bail!(
                            "line {}: sensitive output cannot be retained as an execution result",
                            step.line
                        );
                    }
                    check_call(call, step.line, program_name, &secrets)?;
                    let consumes_secret_pipeline =
                        *connection != crate::ast::Connection::Start && pipeline_secret;
                    let declared_secret_environment =
                        registry.resolve_call(call).ok().is_some_and(|manifest| {
                            crate::security::applet_has_declared_secret_environment(
                                registry,
                                manifest.name_for(call),
                            )
                        });
                    let sensitive_output = call.marks_sensitive_output()
                        || consumes_secret_pipeline
                        || pending_environment_secret
                        || workflow_environment_secret
                        || declared_secret_environment;
                    if sensitive_output && call.cache {
                        bail!("line {}: secret-derived output cannot be cached", step.line);
                    }
                    if sensitive_output && call.result_binding.is_some() {
                        bail!(
                            "line {}: secret-derived output cannot be retained as an execution result",
                            step.line
                        );
                    }
                    if sensitive_output {
                        pipeline_secret = true;
                        if let Some(name) = &call.binding {
                            secrets.insert(name.clone());
                        }
                        secrets.extend(call.result_binding.clone());
                    } else if *connection == crate::ast::Connection::Start {
                        pipeline_secret = false;
                    }
                    pending_environment_secret = false;
                }
                StepKind::CallProgram {
                    call, connection, ..
                } => {
                    workflow_environment_scopes.push(workflow_environment_secret);
                    if call.marks_sensitive_output() && call.binding.is_none() {
                        bail!(
                            "line {}: sensitive output requires a named binding",
                            step.line
                        );
                    }
                    if call.marks_sensitive_output() && call.cache {
                        bail!("line {}: sensitive output cannot be cached", step.line);
                    }
                    if call.marks_sensitive_output() && call.result_binding.is_some() {
                        bail!(
                            "line {}: sensitive output cannot be retained as an execution result",
                            step.line
                        );
                    }
                    check_call(call, step.line, program_name, &secrets)?;
                    let consumes_secret_pipeline =
                        *connection != crate::ast::Connection::Start && pipeline_secret;
                    if call.marks_sensitive_output()
                        || consumes_secret_pipeline
                        || pending_environment_secret
                        || workflow_environment_secret
                    {
                        pipeline_secret = true;
                        secrets.extend(call.binding.clone());
                        secrets.extend(call.result_binding.clone());
                    } else if *connection == crate::ast::Connection::Start {
                        pipeline_secret = false;
                    }
                    pending_environment_secret = false;
                }
                StepKind::EndCall => {
                    workflow_environment_secret =
                        workflow_environment_scopes.pop().unwrap_or(false);
                }
                StepKind::TaskGroup {
                    tasks,
                    binding,
                    each,
                    ..
                } => {
                    let mut sensitive_task =
                        workflow_environment_secret || pending_environment_secret;
                    let sensitive_items = each
                        .as_ref()
                        .is_some_and(|each| references(&each.values, &secrets).is_some());
                    if sensitive_items {
                        if let Some(each) = each {
                            secrets.insert(each.binding.clone());
                        }
                    }
                    for call in tasks {
                        check_call(call, step.line, program_name, &secrets)?;
                        let declared_secret_environment =
                            registry.resolve_call(call).ok().is_some_and(|manifest| {
                                crate::security::applet_has_declared_secret_environment(
                                    registry,
                                    manifest.name_for(call),
                                )
                            });
                        if call.marks_sensitive_output() || declared_secret_environment {
                            sensitive_task = true;
                            if let Some(name) = &call.binding {
                                secrets.insert(name.clone());
                            }
                        }
                    }
                    if sensitive_task || sensitive_items {
                        secrets.insert(binding.clone());
                        pipeline_secret = true;
                    } else {
                        pipeline_secret = false;
                    }
                }
                StepKind::SpawnJob { stages, .. } => {
                    for stage in stages {
                        check_call(&stage.call, stage.line, program_name, &secrets)?;
                    }
                }
                StepKind::IfCommand { call, .. }
                | StepKind::Loop {
                    kind: crate::ast::LoopKind::WhileCommand { call, .. },
                    ..
                } => check_call(call, step.line, program_name, &secrets)?,
                StepKind::TakeJson {
                    source, binding, ..
                }
                | StepKind::TakeJsonDynamic {
                    source, binding, ..
                } => {
                    if references(source, &secrets).is_some() {
                        secrets.insert(binding.clone());
                    }
                }
                StepKind::UseValue { source } | StepKind::EncodeJson { source } => {
                    pipeline_secret = references(source, &secrets).is_some();
                }
                StepKind::BindValue { source, binding } => {
                    let secret_source = references(source, &secrets).is_some();
                    if secret_source {
                        secrets.insert(binding.clone());
                    }
                    pipeline_secret = secret_source;
                }
                StepKind::Read { .. } | StepKind::ReadStdin { .. } => {
                    pipeline_secret = false;
                }
                StepKind::UseBytes { binding } => {
                    pipeline_secret |= secrets.contains(binding);
                }
                StepKind::Environment {
                    set,
                    workflow_scope,
                    ..
                } => {
                    let environment_secret = set
                        .values()
                        .any(|value| references(value, &secrets).is_some());
                    if *workflow_scope {
                        workflow_environment_secret |= environment_secret;
                    } else {
                        pending_environment_secret = environment_secret;
                    }
                }
                StepKind::CaptureBytes { binding } => {
                    if pipeline_secret {
                        secrets.insert(binding.clone());
                    }
                }
                StepKind::ReadCallInput { binding } => {
                    if pipeline_secret {
                        if let Some(binding) = binding {
                            secrets.insert(binding.clone());
                        }
                    }
                }
                StepKind::ParseJson { binding } => {
                    if pipeline_secret {
                        secrets.insert(binding.clone());
                    }
                }
                StepKind::Loop {
                    kind: crate::ast::LoopKind::ForEach { binding, values },
                    ..
                } => {
                    if references(values, &secrets).is_some() {
                        secrets.insert(binding.clone());
                    }
                }
                StepKind::Catch { binding, .. } if !secrets.is_empty() || pipeline_secret => {
                    secrets.insert(binding.clone());
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Statically known bindings that may contain secret data. Runtime-facing
/// serializers use this conservative set to withhold derived values too.
pub(crate) fn sensitive_binding_names(program: &Program, registry: &Registry) -> BTreeSet<String> {
    fn references(value: &Value, names: &BTreeSet<String>) -> bool {
        match value {
            Value::Reference(name) => names.contains(name),
            Value::List(values) => values.iter().any(|value| references(value, names)),
            Value::Record(values) => values.values().any(|value| references(value, names)),
            _ => false,
        }
    }
    fn scope(
        inputs: &[crate::ast::InputDeclaration],
        steps: &[crate::ast::Step],
        registry: &Registry,
    ) -> BTreeSet<String> {
        let mut names = inputs
            .iter()
            .filter(|input| crate::ast::is_sensitive(input))
            .map(|input| input.name.clone())
            .collect::<BTreeSet<_>>();
        let mut pipeline_secret = false;
        let mut pending_environment_secret = false;
        let mut workflow_environment_secret = false;
        let mut workflow_environment_scopes = Vec::new();
        for step in steps {
            match &step.kind {
                StepKind::Invoke { call, connection } => {
                    let declared_environment_secret =
                        registry.resolve_call(call).ok().is_some_and(|manifest| {
                            crate::security::applet_has_declared_secret_environment(
                                registry,
                                manifest.name_for(call),
                            )
                        });
                    if call.marks_sensitive_output()
                        || (*connection != crate::ast::Connection::Start && pipeline_secret)
                        || pending_environment_secret
                        || workflow_environment_secret
                        || declared_environment_secret
                    {
                        pipeline_secret = true;
                        names.extend(call.binding.clone());
                        names.extend(call.result_binding.clone());
                    } else if *connection == crate::ast::Connection::Start {
                        pipeline_secret = false;
                    }
                    pending_environment_secret = false;
                }
                StepKind::CallProgram {
                    call, connection, ..
                } => {
                    workflow_environment_scopes.push(workflow_environment_secret);
                    if call.marks_sensitive_output()
                        || (*connection != crate::ast::Connection::Start && pipeline_secret)
                        || pending_environment_secret
                        || workflow_environment_secret
                    {
                        pipeline_secret = true;
                        names.extend(call.binding.clone());
                        names.extend(call.result_binding.clone());
                    } else if *connection == crate::ast::Connection::Start {
                        pipeline_secret = false;
                    }
                    pending_environment_secret = false;
                }
                StepKind::EndCall => {
                    workflow_environment_secret =
                        workflow_environment_scopes.pop().unwrap_or(false);
                }
                StepKind::TaskGroup {
                    tasks,
                    binding,
                    each,
                    ..
                } => {
                    let each_secret = each
                        .as_ref()
                        .is_some_and(|each| references(&each.values, &names));
                    let task_secret = workflow_environment_secret
                        || pending_environment_secret
                        || tasks.iter().any(|call| {
                            call.marks_sensitive_output()
                                || registry.resolve_call(call).ok().is_some_and(|manifest| {
                                    crate::security::applet_has_declared_secret_environment(
                                        registry,
                                        manifest.name_for(call),
                                    )
                                })
                        });
                    if each_secret {
                        if let Some(each) = each {
                            names.insert(each.binding.clone());
                        }
                    }
                    if task_secret || each_secret {
                        names.insert(binding.clone());
                        names.extend(
                            tasks
                                .iter()
                                .filter(|call| {
                                    call.marks_sensitive_output()
                                        || registry.resolve_call(call).ok().is_some_and(
                                            |manifest| {
                                                crate::security::applet_has_declared_secret_environment(
                                                    registry,
                                                    manifest.name_for(call),
                                                )
                                            },
                                        )
                                })
                                .filter_map(|call| call.binding.clone()),
                        );
                        pipeline_secret = true;
                    } else {
                        pipeline_secret = false;
                    }
                }
                StepKind::SpawnJob { stages, .. } => names.extend(
                    stages
                        .iter()
                        .filter(|stage| stage.call.marks_sensitive_output())
                        .filter_map(|stage| stage.call.binding.clone()),
                ),
                StepKind::IfCommand { call, .. }
                | StepKind::Loop {
                    kind: crate::ast::LoopKind::WhileCommand { call, .. },
                    ..
                } if call.marks_sensitive_output() => names.extend(call.binding.clone()),
                StepKind::TakeJson {
                    source, binding, ..
                }
                | StepKind::TakeJsonDynamic {
                    source, binding, ..
                } if references(source, &names) => {
                    names.insert(binding.clone());
                }
                StepKind::BindValue { source, binding } => {
                    let secret_source = references(source, &names);
                    if secret_source {
                        names.insert(binding.clone());
                    }
                    pipeline_secret = secret_source;
                }
                StepKind::Loop {
                    kind: crate::ast::LoopKind::ForEach { binding, values },
                    ..
                } if references(values, &names) => {
                    names.insert(binding.clone());
                }
                StepKind::UseValue { source } | StepKind::EncodeJson { source } => {
                    pipeline_secret = references(source, &names)
                }
                StepKind::Environment {
                    set,
                    workflow_scope,
                    ..
                } => {
                    let environment_secret = set.values().any(|value| references(value, &names));
                    if *workflow_scope {
                        workflow_environment_secret |= environment_secret;
                    } else {
                        pending_environment_secret = environment_secret;
                    }
                }
                StepKind::UseBytes { binding } => pipeline_secret = names.contains(binding),
                StepKind::Read { .. } | StepKind::ReadStdin { .. } => pipeline_secret = false,
                StepKind::CaptureBytes { binding } if pipeline_secret => {
                    names.insert(binding.clone());
                }
                StepKind::ReadCallInput { binding } if pipeline_secret => {
                    names.extend(binding.clone());
                }
                StepKind::ParseJson { binding } if pipeline_secret => {
                    names.insert(binding.clone());
                }
                StepKind::Catch { binding, .. } if !names.is_empty() || pipeline_secret => {
                    names.insert(binding.clone());
                }
                _ => {}
            }
        }
        names
    }
    let mut names = scope(&program.inputs, &program.steps, registry);
    for definition in program.definitions.values() {
        names.extend(scope(&definition.inputs, &definition.steps, registry));
    }
    names
}

/// Validate the AST-level stderr modifier before any catalog lookup or other
/// preflight can obscure an unsupported context. Parsed source is checked by
/// the parser too; this pass also covers deserialized and library-built ASTs.
fn validate_discard_stderr_usage(program: &Program) -> Result<()> {
    fn check_direct(call: &Invocation, line: usize) -> Result<()> {
        if !call.discard_stderr {
            return Ok(());
        }
        if !call.direct {
            bail!("line {line}: `and discard stderr` requires a quoted direct native invocation");
        }
        if call.cache {
            bail!("line {line}: `and discard stderr` cannot be combined with `and cache`");
        }
        Ok(())
    }

    for (name, steps) in std::iter::once((None, program.steps.as_slice())).chain(
        program
            .definitions
            .iter()
            .map(|(name, definition)| (Some(name.as_str()), definition.steps.as_slice())),
    ) {
        for step in steps {
            let unsupported = match &step.kind {
                StepKind::Invoke { call, .. } => {
                    check_direct(call, step.line)?;
                    None
                }
                StepKind::CallProgram { call, .. } if call.discard_stderr => {
                    Some("program calls cannot use `and discard stderr`")
                }
                StepKind::TaskGroup { tasks, .. }
                    if tasks.iter().any(|call| call.discard_stderr) =>
                {
                    Some("task groups do not support `and discard stderr`")
                }
                StepKind::SpawnJob { stages, .. }
                    if stages.iter().any(|stage| stage.call.discard_stderr) =>
                {
                    Some("background pipeline stages require captured stderr and cannot discard it")
                }
                _ => None,
            };
            if let Some(message) = unsupported {
                if let Some(name) = name {
                    bail!("program {name:?}, line {}: {message}", step.line);
                }
                bail!("line {}: {message}", step.line);
            }
        }
    }

    if let Some(Trigger::Listen {
        source: ListenSource::Applet(call),
        line,
        ..
    }) = &program.trigger
    {
        if call.discard_stderr {
            bail!("line {line}: listener sources cannot use `and discard stderr`");
        }
    }
    Ok(())
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
                StepKind::SpawnJob { stages, .. } => {
                    calls.extend(stages.iter().map(|stage| &stage.call))
                }
                StepKind::IfCommand { call, .. }
                | StepKind::Loop {
                    kind: crate::ast::LoopKind::WhileCommand { call, .. },
                    ..
                } => calls.push(call),
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

#[cfg(all(feature = "live", not(target_arch = "wasm32")))]
pub(crate) fn validate_live(
    program: &Program,
    registry: &Registry,
    names: BTreeSet<String>,
    has_data: bool,
    allow_exit: bool,
) -> Result<()> {
    if let Some(step) = program
        .steps
        .iter()
        .find(|step| matches!(step.kind, StepKind::OnInterrupt { .. }))
    {
        bail!(
            "line {}: on interrupt is not supported in live programs",
            step.line
        );
    }
    if let Some(step) = program.steps.iter().find(|step| {
        matches!(
            step.kind,
            StepKind::CaptureBytes { .. } | StepKind::UseBytes { .. }
        )
    }) {
        bail!(
            "line {}: byte captures are not supported in live programs",
            step.line
        );
    }
    if let Some(step) = program.steps.iter().find(|step| {
        matches!(
            step.kind,
            StepKind::Interactive | StepKind::HandOver | StepKind::LaunchDetached { .. }
        )
    }) {
        bail!(
            "line {}: live programs do not support interactively, hand over, or detached launch",
            step.line
        );
    }
    if has_job_operations(program) {
        bail!("live programs do not support finite-scope background job handles");
    }
    if let Some(step) = program.steps.iter().find(|step| {
        !allow_exit && matches!(step.kind, StepKind::Exit { .. } | StepKind::Fail { .. })
    }) {
        bail!(
            "line {}: live programs keep their state between runs and cannot exit or fail",
            step.line
        );
    }
    if let Some(step) = program.steps.iter().find(|step| {
        matches!(
            step.kind,
            StepKind::Append { .. } | StepKind::Redirect { .. }
        )
    }) {
        bail!(
            "line {}: live programs do not support file redirection or append",
            step.line
        );
    }
    if let Some(step) = program.steps.iter().find(|step| {
        matches!(
            step.kind,
            StepKind::IfCommand { .. }
                | StepKind::Loop {
                    kind: crate::ast::LoopKind::WhileCommand { .. },
                    ..
                }
        )
    }) {
        bail!(
            "line {}: live programs do not support command conditions; use a value condition",
            step.line
        );
    }
    validate_inputs(program, registry, names, has_data, true)
}

fn validate_inputs(
    program: &Program,
    registry: &Registry,
    mut names: BTreeSet<String>,
    has_data: bool,
    allow_rebinding: bool,
) -> Result<()> {
    validate_discard_stderr_usage(program)?;
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
        nul_text_bindings: BTreeSet::new(),
        known_values: BTreeMap::new(),
        mutation_versions: BTreeMap::new(),
        jobs: BTreeSet::new(),
        unresolved_jobs: BTreeSet::new(),
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
    validate_terminal_handoffs(program, registry)?;
    if !flow.unresolved_jobs.is_empty() {
        let names = flow
            .unresolved_jobs
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "every background job must be awaited or cancelled before normal scope exit: {names}"
        );
    }
    if program.steps.iter().any(|step| {
        matches!(
            step.kind,
            StepKind::CallProgram { .. } | StepKind::Loop { .. }
        )
    }) {
        calls::preflight(program, registry, &BTreeMap::new(), None)?;
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
            // A catch could resume normal flow while a job spawned in the body still
            // runs unobserved, so a try with a catch admits no job operations. Without a
            // catch (as for `on exit`), an error in the body always ends the run after
            // cleanup, and the run's end cancels and reaps every job it still owns.
            if catch.is_some()
                && steps[index + 1..finish - 1].iter().any(|step| {
                    matches!(
                        step.kind,
                        StepKind::SpawnJob { .. }
                            | StepKind::InspectJob { .. }
                            | StepKind::AwaitJob { .. }
                            | StepKind::CancelJob { .. }
                    )
                })
            {
                bail!(
                    "line {}: background job operations are unsupported inside try blocks that have a catch",
                    step.line
                );
            }
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
                if steps[finally_index + 1..finish - 1].iter().any(|step| {
                    matches!(
                        step.kind,
                        StepKind::SpawnJob { .. }
                            | StepKind::InspectJob { .. }
                            | StepKind::AwaitJob { .. }
                            | StepKind::CancelJob { .. }
                    )
                }) {
                    bail!(
                        "line {}: background job operations are unsupported in finally blocks and in on exit or on interrupt handlers",
                        steps[finally_index].line
                    );
                }
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
                    after_try.known_values.remove(name);
                    after_try.nul_text_bindings.remove(name);
                    if let Some(value) = finalizer_flow.known_values.get(name) {
                        after_try.known_values.insert(name.clone(), value.clone());
                    }
                    if finalizer_flow.nul_text_bindings.contains(name) {
                        after_try.nul_text_bindings.insert(name.clone());
                    }
                    if let Some(version) = finalizer_flow.mutation_versions.get(name) {
                        after_try
                            .mutation_versions
                            .entry(name.clone())
                            .and_modify(|current| *current = (*current).max(*version))
                            .or_insert(*version);
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
                let finalizer_mutations = finalizer_flow
                    .mutation_versions
                    .iter()
                    .filter(|(name, version)| {
                        entry.names.contains(*name)
                            && **version
                                > entry
                                    .mutation_versions
                                    .get(*name)
                                    .copied()
                                    .unwrap_or_default()
                    })
                    .map(|(name, _)| (*name).clone())
                    .collect::<Vec<_>>();
                for name in &finalizer_mutations {
                    after_try.known_values.remove(name);
                    after_try.nul_text_bindings.remove(name);
                    if let Some(value) = finalizer_flow.known_values.get(name) {
                        after_try.known_values.insert(name.clone(), value.clone());
                    }
                    if finalizer_flow.nul_text_bindings.contains(name) {
                        after_try.nul_text_bindings.insert(name.clone());
                    }
                }
                for name in &finalizer_mutations {
                    if let Some(version) = finalizer_flow.mutation_versions.get(name) {
                        after_try
                            .mutation_versions
                            .entry(name.clone())
                            .and_modify(|current| *current = (*current).max(*version))
                            .or_insert(*version);
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
            loops::validate(kind, flow)
                .map_err(|error| loops::at_line(error, step.line))
                .with_context(|| format!("line {}", step.line))?;
            let known_iterations = match kind {
                crate::ast::LoopKind::Repeat { count } => {
                    if let Ok(value) = resolve(count, &flow.known_values) {
                        loops::repeat_count(&value).ok()
                    } else {
                        None
                    }
                }
                crate::ast::LoopKind::ForEach { values, .. } => resolve(values, &flow.known_values)
                    .ok()
                    .and_then(|value| match value {
                        Value::List(items) => Some(items.len()),
                        _ => None,
                    }),
                crate::ast::LoopKind::While { .. } | crate::ast::LoopKind::WhileCommand { .. } => {
                    None
                }
            };
            if let crate::ast::LoopKind::WhileCommand { call, .. } = kind {
                validate_command_condition(call, registry, flow)
                    .with_context(|| format!("line {}", step.line))?;
            }
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
                body.nul_text_bindings.remove(binding);
                body.known_values.remove(binding);
                body.invariant.insert(binding.clone(), kind);
                if known_iterations == Some(1) {
                    if let Ok(Value::List(items)) = resolve(values, &flow.known_values) {
                        body.bind_known_value(binding, items[0].clone());
                    }
                }
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
            if body.jobs != flow.jobs || body.unresolved_jobs != flow.unresolved_jobs {
                bail!(
                    "line {}: background job operations are unsupported inside loops",
                    step.line
                );
            }
            let loop_body = index + 1..finish - 1;
            let body_has_early_transfer = loop_body_has_early_transfer(steps, loop_body.clone());
            let repeated_body = if matches!(kind, crate::ast::LoopKind::Repeat { .. })
                && known_iterations.is_some_and(|iterations| iterations > 1)
                && !body_has_early_transfer
            {
                let mut repeated_body = body.clone();
                validate_range(
                    steps,
                    registry,
                    &mut repeated_body,
                    loop_body,
                    true,
                    allow_rebinding,
                    return_allowed,
                )
                .with_context(|| format!("second iteration of loop at line {}", step.line))?;
                Some(repeated_body)
            } else {
                None
            };
            if known_iterations != Some(0) {
                let changed = body
                    .mutation_versions
                    .iter()
                    .filter(|(name, version)| {
                        flow.names.contains(*name)
                            && **version
                                > flow
                                    .mutation_versions
                                    .get(*name)
                                    .copied()
                                    .unwrap_or_default()
                    })
                    .map(|(name, _)| (*name).clone())
                    .collect::<Vec<_>>();
                let repeated_is_stable = repeated_body.as_ref().is_some_and(|repeated| {
                    changed.iter().all(|name| {
                        body.known_values.get(name) == repeated.known_values.get(name)
                            && body.nul_text_bindings.contains(name)
                                == repeated.nul_text_bindings.contains(name)
                    })
                });
                let propagate_body = known_iterations == Some(1) && !body_has_early_transfer;
                let propagate_repeated =
                    repeated_body.is_some() && (repeated_is_stable || known_iterations == Some(2));
                let summary = if propagate_repeated {
                    repeated_body.as_ref().unwrap()
                } else {
                    &body
                };
                for name in &changed {
                    flow.nul_text_bindings.remove(name);
                    flow.known_values.remove(name);
                    if propagate_body || propagate_repeated {
                        if let Some(value) = summary.known_values.get(name) {
                            flow.known_values.insert(name.clone(), value.clone());
                        }
                        if summary.nul_text_bindings.contains(name) {
                            flow.nul_text_bindings.insert(name.clone());
                        }
                    }
                }
                for name in changed {
                    if let Some(version) = summary.mutation_versions.get(&name) {
                        flow.mutation_versions
                            .entry(name)
                            .and_modify(|current| *current = (*current).max(*version))
                            .or_insert(*version);
                    }
                }
            }
            // A loop may execute zero times. Only a statically exact single
            // iteration can replace an outer binding's value facts afterward.
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
                nul_text_bindings: BTreeSet::new(),
                known_values: BTreeMap::new(),
                mutation_versions: BTreeMap::new(),
                jobs: BTreeSet::new(),
                unresolved_jobs: BTreeSet::new(),
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
            if !child.unresolved_jobs.is_empty() {
                bail!(
                    "line {}: every background job in reusable program {:?} must be awaited or cancelled before return",
                    step.line,
                    call.operation
                );
            }
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
        if let StepKind::IfCommand {
            call,
            otherwise,
            end,
            ..
        } = &step.kind
        {
            validate_command_condition(call, registry, flow)
                .with_context(|| format!("line {}", step.line))?;
            let finish = index + end;
            let has_otherwise = otherwise != end;
            let yes_end = if has_otherwise {
                index + otherwise - 1
            } else {
                finish - 1
            };
            // A command condition binds no names, so each branch starts from the
            // same entry flow and the two outcomes are merged afterwards.
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
                        call.binding.is_some()
                            || call.status_binding.is_some()
                            || call.result_binding.is_some()
                    }
                    StepKind::ParseJson { .. }
                    | StepKind::BindValue { .. }
                    | StepKind::TakeJson { .. }
                    | StepKind::TakeJsonDynamic { .. }
                    | StepKind::CaptureBytes { .. }
                    | StepKind::TempScope { .. }
                    | StepKind::TaskGroup { .. }
                    | StepKind::SpawnJob { .. }
                    | StepKind::InspectJob { .. }
                    | StepKind::AwaitJob { .. }
                    | StepKind::CancelJob { .. } => true,
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
                | StepKind::Save { path, .. }
                | StepKind::Append { path }
                | StepKind::Redirect {
                    path: Some(path), ..
                } => Some(path),
                _ => None,
            };
            if let Some(Value::Reference(name)) = path {
                if flow.types.get(name).is_some_and(|kind| {
                    *kind != crate::ast::InputType::Text && *kind != crate::ast::InputType::Never
                }) {
                    bail!("path {name:?} requires text, received {}", flow.types[name]);
                }
            }
            match &step.kind {
                StepKind::Interactive
                | StepKind::HandOver
                | StepKind::LaunchDetached { .. }
                | StepKind::OnInterrupt { .. } => {}
                StepKind::Loop { .. }
                | StepKind::EndLoop { .. }
                | StepKind::If { .. }
                | StepKind::IfCommand { .. }
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
                    flow.jobs.clear();
                    flow.unresolved_jobs.clear();
                }
                StepKind::Exit { status } => {
                    validate_exit_operand(status, flow, 0, "exit status")?;
                    // Exit cancels and reaps every owned job, so it satisfies the
                    // explicit await-or-cancel rule just as return does.
                    flow.jobs.clear();
                    flow.unresolved_jobs.clear();
                }
                StepKind::Fail { message, status } => {
                    validate_text_operand(message, flow, "fail message")?;
                    if let Some(status) = status {
                        validate_exit_operand(status, flow, 1, "fail status")?;
                    }
                    flow.jobs.clear();
                    flow.unresolved_jobs.clear();
                }
                StepKind::UseFolder { path } => {
                    check_path(path, &flow.names)?;
                }
                StepKind::UseValue { source } => {
                    validate_text_operand(source, flow, "use value")?;
                    flow.has_data = true;
                    flow.stdout_unavailable = false;
                    flow.started = true;
                }
                StepKind::BindValue {
                    source,
                    binding: name,
                } => {
                    check_value(source, &flow.names)?;
                    let kind = binding_value_type(source, &flow.types)?;
                    let known_value = resolve_known_arc(source, &flow.known_values);
                    binding(
                        &Some(name.clone()),
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    flow.bind_type(name, kind)?;
                    if let Some(value) = known_value {
                        flow.bind_known_value_arc(name, value);
                    }
                    flow.has_data = true;
                    flow.stdout_unavailable = false;
                    flow.started = true;
                }
                StepKind::TempScope { binding: name, .. } => {
                    // Runtime scopes cannot replace a binding that is already live.
                    // Keep this stricter than ordinary loop/live result rebinding.
                    binding(&Some(name.clone()), &mut flow.names, false)?;
                    flow.bind_type(name, crate::ast::InputType::Text)?;
                }
                StepKind::EndTempScope { back } => {
                    let header = index
                        .checked_sub(*back)
                        .context("invalid temporary scope end")?;
                    if let StepKind::TempScope { binding: name, .. } = &steps[header].kind {
                        flow.names.remove(name);
                        flow.types.remove(name);
                    }
                }
                StepKind::CaptureBytes { binding: name } => {
                    if !flow.has_data {
                        bail!("capture bytes requires preceding output");
                    }
                    binding(
                        &Some(name.clone()),
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    flow.bind_type(name, crate::ast::InputType::Bytes)?;
                    flow.started = true;
                }
                StepKind::UseBytes { binding: name } => {
                    match flow.types.get(name) {
                        Some(crate::ast::InputType::Bytes) => {}
                        Some(kind) => {
                            bail!("use bytes requires a byte capture, but {name:?} is {kind}")
                        }
                        None => bail!("use bytes: byte binding {name:?} is not defined"),
                    }
                    flow.has_data = true;
                    flow.stdout_unavailable = false;
                    flow.started = true;
                }
                StepKind::Read { binding: name, .. }
                | StepKind::ReadStdin { binding: name }
                | StepKind::ReadCallInput { binding: name } => {
                    if let StepKind::Read { path, .. } = &step.kind {
                        check_path(path, &flow.names)?;
                    }
                    if allow_rebinding && matches!(step.kind, StepKind::ReadStdin { .. }) {
                        bail!(
                            "managed programs use /input or listen to stdin via the inbox, not read from stdin"
                        );
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
                    // Resolve what is statically known so a selection that is
                    // already invalid can fail before earlier native effects.
                    let known_value = resolve(source, &flow.known_values)
                        .ok()
                        .map(|source| {
                            crate::structured::take(pointer, &source, *kind, step.line).cloned()
                        })
                        .transpose()?;
                    binding(
                        &Some(name.clone()),
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    flow.bind_type(name, *kind)?;
                    if let Some(value) = known_value {
                        flow.bind_known_value(name, value);
                    }
                }
                StepKind::TakeJsonDynamic {
                    pointer,
                    source,
                    kind,
                    binding: name,
                } => {
                    check_value(pointer, &flow.names)?;
                    if let Value::Reference(reference) = pointer {
                        if flow.types.get(reference).is_some_and(|kind| {
                            *kind != crate::ast::InputType::Text
                                && *kind != crate::ast::InputType::Never
                        }) {
                            bail!("dynamic JSON pointer {reference:?} requires text");
                        }
                    }
                    if let Value::String(pointer) = pointer {
                        crate::structured::validate_pointer(pointer)?;
                    }
                    check_value(source, &flow.names)?;
                    let known_value = resolve(pointer, &flow.known_values)
                        .ok()
                        .and_then(|pointer| match pointer {
                            Value::String(pointer) => Some(pointer),
                            _ => None,
                        })
                        .and_then(|pointer| {
                            resolve(source, &flow.known_values)
                                .ok()
                                .map(|source| (pointer, source))
                        })
                        .map(|(pointer, source)| {
                            crate::structured::take(&pointer, &source, *kind, step.line).cloned()
                        })
                        .transpose()?;
                    binding(
                        &Some(name.clone()),
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    flow.bind_type(name, *kind)?;
                    if let Some(value) = known_value {
                        flow.bind_known_value(name, value);
                    }
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
                    each,
                    ..
                } => {
                    if *limit == 0 {
                        bail!("task group limit must be greater than zero");
                    }
                    if tasks.is_empty() {
                        bail!("task group requires at least one task");
                    }
                    let mut task_names = BTreeSet::new();
                    if let Some(each) = each {
                        let [template] = tasks.as_slice() else {
                            bail!("parallel for each requires exactly one command");
                        };
                        if flow.names.contains(&each.binding) {
                            bail!("iteration binding {:?} is already defined", each.binding);
                        }
                        check_value(&each.values, &flow.names)?;
                        let item_type = loops::element_type(&each.values, &flow.types)?;
                        if template.binding.is_some()
                            || template.discard_stderr
                            || template.cache
                            || template.status_binding.is_some()
                            || template.result_binding.is_some()
                        {
                            bail!(
                                "the parallel for each command cannot bind output, status or result, discard stderr, or use `and cache`"
                            );
                        }
                        let manifest = registry.resolve_call(template)?;
                        if manifest.builtin.is_some() {
                            bail!("task groups cannot invoke interpreter builtins");
                        }
                        let mut types = flow.types.clone();
                        types.insert(each.binding.clone(), item_type);
                        manifest.validate_with_types(template, &types)?;
                        let mut names = flow.names.clone();
                        names.insert(each.binding.clone());
                        for value in template.parameters.values() {
                            check_value(value, &names)?;
                        }
                    }
                    for task in tasks.iter().filter(|_| each.is_none()) {
                        if task.direct
                            || task.discard_stderr
                            || task.cache
                            || task.status_binding.is_some()
                            || task.result_binding.is_some()
                        {
                            bail!(
                                "task groups require named, uncached applet calls without status or result bindings"
                            );
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
                StepKind::SpawnJob { name, stages } => {
                    if flow.loop_depth > 0 {
                        bail!("background job operations are unsupported inside loops");
                    }
                    if stages.is_empty() || stages.len() > 16 {
                        bail!("background pipeline must contain between 1 and 16 stages");
                    }
                    if !flow.jobs.insert(name.clone()) {
                        bail!("background job {name:?} is already defined in this scope");
                    }
                    flow.unresolved_jobs.insert(name.clone());
                    for (stage_index, stage) in stages.iter().enumerate() {
                        let call = &stage.call;
                        if call.discard_stderr
                            || call.cache
                            || call.binding.is_some()
                            || call.status_binding.is_some()
                            || call.result_binding.is_some()
                        {
                            bail!(
                                "line {}: background pipeline stages cannot bind output, status, or result, or use `and cache`",
                                stage.line
                            );
                        }
                        let manifest = registry.resolve_call(call).with_context(|| {
                            format!("line {}: resolving background stage", stage.line)
                        })?;
                        if manifest.builtin.is_some() {
                            bail!(
                                "line {}: interpreter builtins cannot run in a background pipeline",
                                stage.line
                            );
                        }
                        if manifest.execution_backend() != crate::contracts::AppletBackend::Command
                        {
                            bail!(
                                "line {}: background pipelines require native command applets or quoted direct executables",
                                stage.line
                            );
                        }
                        manifest
                            .validate_with_types(call, &flow.types)
                            .with_context(|| {
                                format!("line {}: background pipeline parameters", stage.line)
                            })?;
                        if stage_index > 0 && !manifest.accepts_stdin {
                            bail!(
                                "line {}: applet {:?} does not accept background pipeline input",
                                stage.line,
                                manifest.name
                            );
                        }
                        if let Some(stdio) = manifest.stdio() {
                            if stdio.stderr.is_some_and(|mode| {
                                mode != crate::contracts::StdioDisposition::Capture
                            }) || (stage_index + 1 == stages.len()
                                && stdio.stdout.is_some_and(|mode| {
                                    mode != crate::contracts::StdioDisposition::Capture
                                }))
                            {
                                bail!(
                                    "line {}: background pipelines require captured stdout and stderr",
                                    stage.line
                                );
                            }
                            if stage_index + 1 < stages.len()
                                && stdio.stdout.is_some_and(|mode| {
                                    mode != crate::contracts::StdioDisposition::Capture
                                })
                            {
                                bail!(
                                    "line {}: only the final background pipeline stage can discard or inherit stdout",
                                    stage.line
                                );
                            }
                        }
                        for value in call.parameters.values() {
                            check_value(value, &flow.names)?;
                        }
                    }
                    flow.started = true;
                }
                StepKind::InspectJob {
                    name,
                    binding: output,
                } => {
                    if flow.loop_depth > 0 {
                        bail!("background job operations are unsupported inside loops");
                    }
                    if !flow.jobs.contains(name) {
                        bail!("unknown or out-of-scope background job {name:?}");
                    }
                    binding(
                        &Some(output.clone()),
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    flow.bind_type(output, crate::ast::InputType::Json)?;
                    flow.started = true;
                }
                StepKind::AwaitJob {
                    name,
                    status_binding,
                    result_binding,
                } => {
                    if flow.loop_depth > 0 {
                        bail!("background job operations are unsupported inside loops");
                    }
                    if !flow.jobs.contains(name) {
                        bail!("unknown or out-of-scope background job {name:?}");
                    }
                    flow.unresolved_jobs.remove(name);
                    for (binding_name, kind) in [
                        (status_binding.as_ref(), crate::ast::InputType::Integer),
                        (result_binding.as_ref(), crate::ast::InputType::Json),
                    ] {
                        if let Some(binding_name) = binding_name {
                            binding(
                                &Some(binding_name.clone()),
                                &mut flow.names,
                                allow_rebinding || flow.loop_depth > 0,
                            )?;
                            flow.bind_type(binding_name, kind)?;
                        }
                    }
                    flow.has_data = true;
                    flow.stdout_unavailable = false;
                    flow.started = true;
                }
                StepKind::CancelJob { name } => {
                    if flow.loop_depth > 0 {
                        bail!("background job operations are unsupported inside loops");
                    }
                    if !flow.jobs.contains(name) {
                        bail!("unknown or out-of-scope background job {name:?}");
                    }
                    flow.unresolved_jobs.remove(name);
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
                        bail!(
                            "line {}: then cannot consume stdout explicitly discarded or inherited by the preceding applet",
                            step.line
                        );
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
                            bail!(
                                "cd changes the program directory and cannot belong to a pipe group"
                            );
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
                        if index > 0 {
                            if let StepKind::Invoke {
                                call: producer_call,
                                ..
                            } = &steps[index - 1].kind
                            {
                                let producer = registry.resolve_call(producer_call)?;
                                let issues = producer.native_pipe_compatibility_issues(manifest);
                                if !issues.is_empty() {
                                    bail!(
                                        "line {}: native pipe from {:?} to {:?} is incompatible: {}",
                                        step.line,
                                        producer.name,
                                        manifest.name,
                                        issues.join("; ")
                                    );
                                }
                            }
                        }
                        if (index == 0 && !initial_has_data)
                            || !matches!(
                                steps[index.saturating_sub(1)].kind,
                                StepKind::Read { .. }
                                    | StepKind::ReadStdin { .. }
                                    | StepKind::ReadCallInput { .. }
                                    | StepKind::UseValue { .. }
                                    | StepKind::BindValue { .. }
                                    | StepKind::CaptureBytes { .. }
                                    | StepKind::UseBytes { .. }
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
                            bail!(
                                "status capture requires a single uncached command outside a pipe group"
                            );
                        }
                        if call.status_binding == call.binding {
                            bail!("output and status require distinct binding names");
                        }
                    }
                    if let Some(name) = &call.result_binding {
                        if manifest.builtin.is_some() {
                            bail!("execution result capture requires an applet invocation");
                        }
                        if call.cache {
                            bail!(
                                "execution result capture cannot use `and cache` because cached calls do not retain pipeline stages"
                            );
                        }
                        if pipe_in_steps(steps, index + 1) {
                            bail!(
                                "execution result capture must name the terminal call in a pipe group"
                            );
                        }
                        if call.binding.as_ref() == Some(name) {
                            bail!("output and execution result require distinct binding names");
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
                        bail!(
                            "name results at the end of a pipe group; intermediate naming requires buffering"
                        );
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
                            || (manifest.output_kind().is_some() && call.result_binding.is_none()))
                    {
                        bail!(
                            "line {}: discarded or inherited stdout cannot be bound, cached, or decoded as a typed output",
                            step.line
                        );
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
                        let kind = polymorphic_output_kind(manifest, call, &flow.types)
                            .or_else(|| manifest.output_kind())
                            .unwrap_or(crate::ast::InputType::Text);
                        flow.bind_type(name, kind)?;
                    }
                    binding(
                        &call.status_binding,
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    if let Some(name) = &call.status_binding {
                        flow.bind_type(name, crate::ast::InputType::Integer)?;
                    }
                    binding(
                        &call.result_binding,
                        &mut flow.names,
                        allow_rebinding || flow.loop_depth > 0,
                    )?;
                    if let Some(name) = &call.result_binding {
                        if call.status_binding.as_ref() == Some(name) {
                            bail!("status and execution result require distinct binding names");
                        }
                        flow.bind_type(name, crate::ast::InputType::Json)?;
                    }
                    flow.has_data = manifest.builtin.is_none() && terminal_stdout_captured;
                    flow.stdout_unavailable = manifest.builtin.is_none()
                        && *connection == Connection::Then
                        && terminal_call
                        && !terminal_stdout_captured;
                    flow.started |= manifest.builtin.is_none();
                }
                StepKind::Environment {
                    set,
                    workflow_scope,
                    ..
                } => {
                    for (name, value) in set {
                        if matches!(value, Value::Reference(binding) if flow.nul_text_bindings.contains(binding))
                        {
                            bail!("environment value for {name:?} cannot contain a NUL byte");
                        }
                        validate_text_operand(
                            value,
                            flow,
                            &format!("environment value for {name:?}"),
                        )?;
                    }
                    if !*workflow_scope {
                        let invoke = match steps.get(index + 1) {
                            Some(crate::ast::Step {
                                kind: StepKind::Redirect { .. },
                                ..
                            }) => index + 2,
                            _ => index + 1,
                        };
                        let Some(crate::ast::Step {
                            kind: StepKind::Invoke { call, connection },
                            ..
                        }) = steps.get(invoke)
                        else {
                            bail!(
                                "environment must immediately precede a redirection or native command"
                            );
                        };
                        if *connection == Connection::Pipe || pipe_in_steps(steps, invoke + 1) {
                            bail!("environment applies to a single command, not a pipe stage");
                        }
                        if call.cache {
                            bail!("command environment cannot be combined with cache");
                        }
                        let manifest = registry.resolve_call(call)?;
                        if manifest.builtin.is_some() {
                            bail!("a builtin cannot take a command environment");
                        }
                        if manifest.execution_backend() != crate::contracts::AppletBackend::Command
                        {
                            bail!("command environment applies to native commands only");
                        }
                    }
                }
                StepKind::Append { path } => {
                    check_path(path, &flow.names)?;
                    if !flow.has_data {
                        bail!("append requires preceding output");
                    }
                }
                StepKind::Redirect { target, path } => {
                    if let Some(path) = path {
                        check_path(path, &flow.names)?;
                    }
                    let Some(crate::ast::Step {
                        kind: StepKind::Invoke { call, connection },
                        ..
                    }) = steps.get(index + 1)
                    else {
                        bail!("redirection must immediately precede one native command");
                    };
                    if *connection == Connection::Pipe || pipe_in_steps(steps, index + 2) {
                        bail!(
                            "redirection applies to a single command; it cannot feed or precede a pipe to stage"
                        );
                    }
                    if call.discard_stderr || call.cache {
                        bail!("redirection cannot be combined with discard stderr or cache");
                    }
                    if call.result_binding.is_some()
                        || (call.binding.is_some()
                            && *target == crate::ast::RedirectTarget::StdoutToStderr)
                    {
                        bail!(
                            "redirected stdout is pipeline data; it cannot be bound with `keep result`, or with `as` for stdout to stderr"
                        );
                    }
                    if registry.resolve_call(call)?.builtin.is_some() {
                        bail!("a builtin cannot be redirected");
                    }
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
    // The rows below only display values. A secret answer is left out, so an argument
    // that uses it is shown by its reference, never by its value.
    let known = crate::inputs::without_secret_answers(&program.inputs, known);
    let mut rows = Vec::new();
    for input in &program.inputs {
        let default = match (&input.default, crate::ast::is_sensitive(input)) {
            (Some(_), true) => "default [redacted]".to_owned(),
            (Some(value), false) => format!("default {}", describe_value(value)),
            (None, _) => "required input".into(),
        };
        rows.push(format!(
            "line {}: accept {} as {}; {default}",
            input.line, input.name, input.kind
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
                let argv = if call.marks_sensitive_output() {
                    "[withheld]".to_owned()
                } else {
                    match arguments_for_call(call, manifest, &known) {
                        Ok(args) => format!("{args:?}"),
                        Err(_) => format!(
                            "parameters {} (resolve program inputs at runtime)",
                            describe_parameters(&call.parameters)
                        ),
                    }
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
    for (index, step) in program.steps.iter().enumerate() {
        let description = match &step.kind {
            StepKind::Loop {
                kind:
                    crate::ast::LoopKind::WhileCommand {
                        call,
                        expect_success,
                    },
                ..
            } => format!(
                "while run {:?} {}; command output discarded, stdin closed; default per-loop {} entered-iteration limit",
                call.operation,
                if *expect_success { "succeeds" } else { "fails" },
                loops::MAX_LOOP_ITERATIONS
            ),
            StepKind::Loop { kind, .. } => format!(
                "loop {}; default per-loop {} entered-iteration limit",
                describe_loop_kind(kind),
                loops::MAX_LOOP_ITERATIONS
            ),
            StepKind::EndLoop { .. } => "end iteration; remove body-local names".into(),
            StepKind::If { condition, .. } => format!("if {}", describe_condition(condition)),
            StepKind::IfCommand {
                call,
                expect_success,
                ..
            } => format!(
                "if run {:?} {}; command output discarded, stdin closed, pipeline value unchanged",
                call.operation,
                if *expect_success { "succeeds" } else { "fails" }
            ),
            StepKind::Otherwise { .. } => "otherwise".into(),
            StepKind::OnInterrupt { .. } => {
                "on interrupt body: runs only when an interrupt starts a drain".into()
            }
            StepKind::EndIf => "end conditional".into(),
            StepKind::CallProgram { call, .. } => format!(
                "call program {:?}; explicit inputs {}; local scope",
                call.operation,
                if call.marks_sensitive_output() {
                    "[withheld]".to_owned()
                } else {
                    describe_parameters(&call.parameters)
                }
            ),
            StepKind::EndCall => "return program output; restore caller scope and cwd".into(),
            StepKind::BreakLoop => "break the innermost loop; restore its outer bindings".into(),
            StepKind::ContinueLoop => {
                "finish the current iteration and continue the innermost loop".into()
            }
            StepKind::Return => "return from the current reusable program".into(),
            StepKind::Exit { status } => format!(
                "exit with status {}; run pending finally blocks, cancel and reap jobs, keep output",
                describe_operand(status)
            ),
            StepKind::Fail { message, status } => format!(
                "fail with message {} and status {}; write message to stderr, run finally blocks, cancel and reap jobs",
                describe_operand(message),
                status
                    .as_ref()
                    .map_or_else(|| "1".to_owned(), describe_operand)
            ),
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
            StepKind::UseFolder { path } => {
                format!("use folder {}", describe_resolved_value(path, &known))
            }
            StepKind::UseValue { source } => {
                format!(
                    "use value {} as pipeline input; launches no process",
                    describe_value(source)
                )
            }
            StepKind::BindValue { source, binding } => {
                format!(
                    "bind value {} as {binding}; typed binding; launches no process; no program stdout",
                    describe_value(source)
                )
            }
            StepKind::CaptureBytes { binding } => {
                format!("capture pipeline bytes unchanged as {binding}; pipeline continues")
            }
            StepKind::UseBytes { binding } => {
                format!("use byte binding {binding} as pipeline input; launches no process")
            }
            StepKind::Read { path, .. } => {
                format!("read {}", describe_resolved_value(path, &known))
            }
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
            } => format!(
                "take {kind} at {pointer:?} from {} as {binding}",
                describe_value(source)
            ),
            StepKind::TakeJsonDynamic {
                pointer,
                source,
                kind,
                binding,
            } => format!(
                "take {kind} at dynamic pointer {} from {} as {binding}",
                describe_value(pointer),
                describe_value(source)
            ),
            StepKind::EncodeJson { source } => {
                format!(
                    "encode {} as compact JSON; bounded finite output",
                    describe_value(source)
                )
            }
            StepKind::TaskGroup {
                limit,
                binding,
                tasks,
                each: Some(each),
                ..
            } => format!(
                "parallel for each {} in {} (limit {limit}): {} {:?} as {binding}",
                each.binding,
                describe_value(&each.values),
                if tasks[0].direct {
                    "run executable"
                } else {
                    "run applet"
                },
                tasks[0].operation
            ),
            StepKind::TaskGroup {
                mode,
                limit,
                binding,
                tasks,
                each: None,
            } => format!(
                "{mode:?} task group (limit {limit}) with {} applets as {binding}",
                tasks.len()
            ),
            StepKind::SpawnJob { name, stages } => format!(
                "spawn named background job {name:?} with {} native pipeline stages",
                stages.len()
            ),
            StepKind::InspectJob { name, binding } => {
                format!("inspect background job {name:?} as {binding}")
            }
            StepKind::AwaitJob {
                name,
                status_binding,
                result_binding,
            } => format!(
                "await background job {name:?}{}{}",
                status_binding
                    .as_ref()
                    .map(|name| format!(" and keep status as {name}"))
                    .unwrap_or_default(),
                result_binding
                    .as_ref()
                    .map(|name| format!(" and keep result as {name}"))
                    .unwrap_or_default()
            ),
            StepKind::CancelJob { name } => format!("cancel background job {name:?}"),
            StepKind::Save { path, .. } => {
                format!(
                    "save finite output to {}",
                    describe_resolved_value(path, &known)
                )
            }
            StepKind::Interactive => "interactive (terminal handoff)".into(),
            StepKind::TempScope { kind, binding, .. } => {
                let kind = match kind {
                    crate::ast::TempKind::File => "file",
                    crate::ast::TempKind::Directory => "directory",
                };
                format!("with temporary {kind} as {binding}")
            }
            StepKind::EndTempScope { .. } => "end temporary scope".into(),
            StepKind::HandOver => "hand over to (replaces the script)".into(),
            StepKind::LaunchDetached { output } => match output {
                Some(path) => format!("launch detached (output to {})", describe_value(path)),
                None => "launch detached".into(),
            },
            StepKind::Environment {
                set,
                unset,
                workflow_scope,
            } => {
                let mut parts = Vec::new();
                if !set.is_empty() {
                    let names = set.keys().cloned().collect::<Vec<_>>().join(", ");
                    parts.push(format!("set {names}"));
                }
                if !unset.is_empty() {
                    parts.push(format!("remove {}", unset.join(", ")));
                }
                // Values are secrets for redaction, so only names are shown.
                format!(
                    "{} environment: {}; values are not shown",
                    if *workflow_scope {
                        "workflow"
                    } else {
                        "next command"
                    },
                    parts.join("; ")
                )
            }
            StepKind::Append { path } => format!(
                "append finite output to {}; the value stays the pipeline value",
                describe_path(path, &known)
            ),
            StepKind::Redirect { target, path } => {
                let destination = path.as_ref().map(|path| describe_path(path, &known));
                match (target, destination) {
                    (crate::ast::RedirectTarget::StderrFile, Some(path)) => {
                        format!("next command's stderr replaces file {path}; stdout is unchanged")
                    }
                    (crate::ast::RedirectTarget::StderrAppend, Some(path)) => {
                        format!("next command's stderr appends to file {path}; stdout is unchanged")
                    }
                    (crate::ast::RedirectTarget::StderrMerged, _) => {
                        "next command's stderr follows its stdout in the pipeline value".into()
                    }
                    _ => "next command's stdout goes to stderr; the pipeline value is empty".into(),
                }
            }
            StepKind::Invoke { call, connection } => {
                let manifest = registry.resolve_call(call)?;
                let argv = if call.marks_sensitive_output() {
                    "[withheld]".to_owned()
                } else {
                    match arguments_for_call(call, manifest, &known) {
                        Ok(argv) => format!("{argv:?}"),
                        Err(_) => format!(
                            "parameters {} (resolve named results at runtime)",
                            describe_parameters(&call.parameters)
                        ),
                    }
                };
                format!(
                    "{connection:?}: {:?} argv={argv}{}{}{}{}{}{}",
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
                        .unwrap_or_default(),
                    call.result_binding
                        .as_ref()
                        .map(|name| format!(" and keep result as {name}"))
                        .unwrap_or_default(),
                    if call.discard_stderr {
                        " [stderr discarded]"
                    } else {
                        ""
                    },
                ) + &call.retry.map_or_else(String::new, |policy| {
                    format!(
                        " and retry up to {} times{}{}",
                        policy.retries,
                        if policy.even_if_uncertain {
                            " even if uncertain"
                        } else {
                            ""
                        },
                        policy.backoff_ms.map_or_else(String::new, |ms| format!(
                            " with backoff {ms} ms{}",
                            if policy.doubling { " doubling" } else { "" }
                        )),
                    )
                }) + match crate::privilege::classify(manifest.binary_for(call)) {
                    crate::privilege::Privilege::Elevator => " [privileged: elevator]",
                    crate::privilege::Privilege::Wrapper => " [wrapper: may run another program]",
                    crate::privilege::Privilege::Standard => "",
                }
            }
        };
        let stdout = match step.kind.program_stdout() {
            Some(true)
                if (matches!(&step.kind, StepKind::Invoke { call, .. } if !call.cache)
                    || matches!(&step.kind, StepKind::AwaitJob { .. }))
                    && !crate::ast::has_stdout_consumer(&program.steps, index + 1) =>
            {
                "program stdout: stream"
            }
            Some(true) => "program stdout: only if this is the final unbound value",
            _ => "program stdout: none",
        };
        rows.push(format!("line {}: {description}; {stdout}", step.line));
    }
    Ok(rows
        .iter()
        .map(|row| crate::terminal_text::escape_terminal_field(row))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn resolve<B>(value: &Value, bindings: &BTreeMap<String, B>) -> Result<Value>
where
    B: std::borrow::Borrow<Value>,
{
    Ok(match value {
        Value::Reference(name) => bindings
            .get(name)
            .map(|value| value.borrow().clone())
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

fn resolve_known_arc(
    value: &Value,
    bindings: &BTreeMap<String, std::sync::Arc<Value>>,
) -> Option<std::sync::Arc<Value>> {
    if let Value::Reference(name) = value {
        return bindings.get(name).cloned();
    }
    resolve(value, bindings).ok().map(std::sync::Arc::new)
}
fn contains_reference(value: &Value) -> bool {
    match value {
        Value::Reference(_) => true,
        Value::List(values) => values.iter().any(contains_reference),
        Value::Record(values) => values.values().any(contains_reference),
        _ => false,
    }
}

/// UTF-8 bytes of a text value for `use value`; structured values must be encoded first.
fn text_value_bytes(
    value: &Value,
    bindings: &BTreeMap<String, Value>,
    line: usize,
) -> Result<Vec<u8>> {
    match structured_source(value, bindings)?.as_ref() {
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        _ => bail!(
            "line {line}: use value requires text; encode structured values first with `then encode ... as json`"
        ),
    }
}

/// Preserve scalar and text-list types, and retain other literals as JSON.
fn binding_value_type(
    value: &Value,
    types: &BTreeMap<String, crate::ast::InputType>,
) -> Result<crate::ast::InputType> {
    use crate::ast::InputType;
    Ok(match value {
        Value::Reference(name) => {
            let kind = *types
                .get(name)
                .with_context(|| format!("value binding {name:?} has no known type"))?;
            if kind == InputType::Bytes {
                bail!("use value cannot bind bytes; use bytes {name} instead");
            }
            kind
        }
        // NUL cannot cross the native text boundary. Preserve it as JSON so
        // a list binding containing NUL has the same loop element type as an
        // equivalent inline JSON array.
        Value::String(text) if text.contains('\0') => InputType::Json,
        Value::String(_) => InputType::Text,
        Value::Integer(_) => InputType::Integer,
        Value::Bool(_) => InputType::Boolean,
        Value::List(items) => {
            let mut text = true;
            for item in items {
                text &= binding_value_type(item, types)? == InputType::Text;
            }
            if text {
                InputType::TextList
            } else {
                InputType::Json
            }
        }
        Value::Record(fields) => {
            for value in fields.values() {
                binding_value_type(value, types)?;
            }
            InputType::Json
        }
        _ => InputType::Json,
    })
}

/// Bound values retain their type in the name; their pipeline representation is
/// UTF-8 text or compact JSON. Serialization remains subject to the output bound.
fn bound_value_bytes(value: &Value, limit: usize) -> Result<Vec<u8>> {
    if let Value::String(text) = value {
        if text.len() > limit {
            bail!("bound value exceeds the output limit of {limit} bytes");
        }
        Ok(text.as_bytes().to_vec())
    } else {
        crate::structured::encode(value, limit)
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
    Ok(cwd.join(crate::home::expand_home_path(path)?))
}

#[cfg(test)]
mod binding_type_tests {
    use super::{binding_value_type, polymorphic_output_kind};
    use crate::ast::{InputType, Value};
    use crate::manifest::Registry;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[cfg(feature = "live")]
    #[test]
    fn live_validation_rejects_a_temporary_scope_reusing_a_live_name() {
        let registry = Registry::from_sources(&[] as &[&str]).unwrap();
        let program = crate::parser::parse(
            "with temporary file as t\n  use value \"inside\"\n  then save to t\n",
        )
        .unwrap();
        let names = std::collections::BTreeSet::from(["t".to_owned()]);

        let error = super::validate_live(&program, &registry, names, true, false)
            .expect_err("temporary scopes must not replace live bindings");
        assert!(
            format!("{error:#}").contains("result \"t\" is already defined"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn nul_strings_and_lists_are_json_typed_while_plain_text_lists_stay_text() {
        let types = BTreeMap::new();
        assert_eq!(
            binding_value_type(&Value::String("nul\0text".into()), &types).unwrap(),
            InputType::Json
        );
        assert_eq!(
            binding_value_type(
                &Value::List(vec![Value::String("nul\0text".into())]),
                &types,
            )
            .unwrap(),
            InputType::Json
        );
        assert_eq!(
            binding_value_type(
                &Value::List(vec![Value::String("plain text".into())]),
                &types,
            )
            .unwrap(),
            InputType::TextList
        );
        assert_eq!(
            binding_value_type(&Value::List(Vec::new()), &types).unwrap(),
            InputType::TextList
        );
    }

    #[test]
    fn applet_list_outputs_keep_nul_items_json_typed_for_following_loops() {
        let fixture = json!({
            "name": "fixture.list",
            "binary": "fixture-list",
            "parameters": {"items": {"type": "json", "required": true}},
            "arguments": [{"json_arguments": true}],
            "contract": {
                "version": 1,
                "output": {
                    "type": "list",
                    "encoding": "json",
                    "items": {"type": "json"},
                    "same_as_parameter": "items"
                }
            }
        });
        let registry = Registry::from_sources(&[fixture.to_string()]).unwrap();
        for (values, expected) in [
            ("[\"\\u0000\"]", InputType::JsonList),
            ("[\"plain\"]", InputType::TextList),
            ("[]", InputType::JsonList),
        ] {
            let program =
                crate::parser::parse(&format!("run fixture.list with items {values} as bound\n"))
                    .unwrap();
            let crate::ast::StepKind::Invoke { call, .. } = &program.steps[0].kind else {
                panic!("expected the fixture list invocation");
            };
            let manifest = registry.resolve_call(call).unwrap();
            let item_kind = match expected {
                InputType::JsonList => InputType::Json,
                InputType::TextList => InputType::Text,
                _ => unreachable!("fixture only produces text or JSON lists"),
            };
            let mut output_types = BTreeMap::new();
            output_types.insert("bound".into(), expected);
            assert_eq!(
                polymorphic_output_kind(manifest, call, &BTreeMap::new()),
                Some(expected),
                "unexpected output type for list items {values}"
            );
            assert_eq!(
                crate::runtime::loops::element_type(
                    &Value::Reference("bound".into()),
                    &output_types,
                )
                .unwrap(),
                item_kind,
                "wrong loop element type for applet list items {values}"
            );
        }
    }
}
