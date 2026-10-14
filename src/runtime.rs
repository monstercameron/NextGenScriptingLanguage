//! Unix program executor with finite pipelines and persistent event services.
use crate::ast::{Connection, Invocation, ListenSource, Program, StepKind, Trigger, Value};
use crate::manifest::{Builtin, Manifest, Registry};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod calls;
mod checkpoint;
mod control;
pub(crate) mod live;
mod resume;
mod service;
mod streaming;
pub use checkpoint::CheckpointOptions;
pub use resume::run_checkpointed;
pub use streaming::{run_stream, StreamResult};
mod stdio;
pub(crate) use service::next_interval;
pub use service::{
    run_service, run_service_checkpointed, run_service_with_cancellation, ServiceLimits,
    ServiceResult,
};
pub use stdio::{write_cancellable, write_diagnostic, write_with_deadline};

#[derive(Clone)]
pub struct RunOptions {
    pub inputs: BTreeMap<String, Value>,
    pub working_directory: PathBuf,
    pub cache_directory: Option<PathBuf>,
    pub trace: bool,
    pub max_output_bytes: usize,
    /// Maximum lifetime of each native pipeline group or finite stdin capture, not file I/O.
    pub command_timeout: Option<Duration>,
}
#[derive(Debug)]
struct CommandTimedOut;
impl std::fmt::Display for CommandTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("command pipeline timed out; external effects may be incomplete")
    }
}
impl std::error::Error for CommandTimedOut {}
fn check_deadline(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(CommandTimedOut.into());
    }
    Ok(())
}
fn deadline_poll_timeout(deadline: Option<Instant>, maximum_ms: libc::c_int) -> libc::c_int {
    deadline.map_or(maximum_ms, |deadline| {
        let remaining_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            / 1_000_000;
        remaining_ms.min(maximum_ms as u128) as libc::c_int
    })
}
pub struct RunResult {
    pub output: Vec<u8>,
    pub cache_hits: usize,
    pub launched_processes: usize,
}

const SMALL_STDIN_PREFILL_LIMIT: usize = 64 * 1024;

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

fn configured_inputs<'a>(
    program: &Program,
    registry: &Registry,
    options: &'a RunOptions,
) -> Result<Cow<'a, RunOptions>> {
    if program.inputs.is_empty() && options.inputs.is_empty() {
        return Ok(Cow::Borrowed(options));
    }
    let mut configured = options.clone();
    configured.inputs = crate::inputs::resolve(&program.inputs, &options.inputs, false)?;
    preflight_inputs(program, registry, &configured.inputs)?;
    Ok(Cow::Owned(configured))
}

pub(crate) fn preflight_inputs(
    program: &Program,
    registry: &Registry,
    bindings: &BTreeMap<String, Value>,
) -> Result<()> {
    if program.inputs.is_empty() {
        return Ok(());
    }
    calls::preflight(program, registry, bindings)?;
    Ok(())
}

fn input_signature(program: &Program, values: &BTreeMap<String, Value>) -> Result<Option<String>> {
    if program.inputs.is_empty() {
        return Ok(None);
    }
    let signature = program
        .inputs
        .iter()
        .map(|d| (&d.name, (d.kind, values.get(&d.name))))
        .collect::<BTreeMap<_, _>>();
    let digest = Sha256::digest(serde_json::to_vec(&signature)?);
    Ok(Some(format!("inputs-v1:{digest:x}")))
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
fn pipe_at(program: &Program, index: usize) -> bool {
    matches!(
        program.steps.get(index).map(|s| &s.kind),
        Some(StepKind::Invoke {
            connection: Connection::Pipe,
            ..
        })
    )
}

/// Validate the complete script before launching any command or saving files.
pub fn validate(program: &Program, registry: &Registry) -> Result<()> {
    crate::inputs::validate(&program.inputs)?;
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
        match trigger {
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
        }
    } else {
        validate_finite(program, registry, None)
    }
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
    control::validate_shape(&program.steps)?;
    let initial_has_data = has_data;
    let mut flow = control::FlowState {
        names,
        types,
        has_data,
        started: has_data && !allow_rebinding,
    };
    validate_range(
        program,
        registry,
        &mut flow,
        0..program.steps.len(),
        initial_has_data,
        allow_rebinding,
    )?;
    if program
        .steps
        .iter()
        .any(|step| matches!(step.kind, StepKind::CallProgram { .. }))
    {
        calls::preflight(program, registry, &BTreeMap::new())?;
    }
    Ok(())
}

fn validate_range(
    program: &Program,
    registry: &Registry,
    flow: &mut control::FlowState,
    range: std::ops::Range<usize>,
    initial_has_data: bool,
    allow_rebinding: bool,
) -> Result<()> {
    let mut index = range.start;
    while index < range.end {
        let step = &program.steps[index];
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
            if *connection == Connection::Pipe || pipe_at(program, finish) {
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
                started: false,
            };
            validate_range(
                program,
                registry,
                &mut child,
                index + 1..finish - 1,
                has_data,
                false,
            )
            .with_context(|| {
                format!("program {:?}, called at line {}", call.operation, step.line)
            })?;
            binding(&call.binding, &mut flow.names, allow_rebinding)
                .with_context(|| format!("line {}", step.line))?;
            if let Some(name) = &call.binding {
                flow.types.insert(name.clone(), crate::ast::InputType::Text);
            }
            flow.has_data = child.has_data;
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
            let defines_names = program.steps[index + 1..finish - 1]
                .iter()
                .any(|step| match &step.kind {
                    StepKind::Read { binding, .. }
                    | StepKind::ReadStdin { binding }
                    | StepKind::ReadCallInput { binding } => binding.is_some(),
                    StepKind::Invoke { call, .. } => {
                        call.binding.is_some() || call.status_binding.is_some()
                    }
                    StepKind::ParseJson { .. } | StepKind::TakeJson { .. } => true,
                    StepKind::CallProgram { .. } => true,
                    _ => false,
                });
            if !defines_names {
                let before = (flow.has_data, flow.started);
                validate_range(
                    program,
                    registry,
                    flow,
                    index + 1..yes_end,
                    initial_has_data,
                    allow_rebinding,
                )?;
                let yes = (flow.has_data, flow.started);
                (flow.has_data, flow.started) = before;
                if has_otherwise {
                    validate_range(
                        program,
                        registry,
                        flow,
                        index + otherwise..finish - 1,
                        initial_has_data,
                        allow_rebinding,
                    )?;
                }
                flow.has_data &= yes.0;
                flow.started |= yes.1;
                index = finish;
                continue;
            }
            let mut yes = flow.clone();
            validate_range(
                program,
                registry,
                &mut yes,
                index + 1..yes_end,
                initial_has_data,
                allow_rebinding,
            )?;
            if has_otherwise {
                validate_range(
                    program,
                    registry,
                    flow,
                    index + otherwise..finish - 1,
                    initial_has_data,
                    allow_rebinding,
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
                StepKind::If { .. }
                | StepKind::Otherwise { .. }
                | StepKind::EndIf
                | StepKind::CallProgram { .. }
                | StepKind::EndCall => {
                    unreachable!("validated control boundaries")
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
                    binding(name, &mut flow.names, allow_rebinding)?;
                    if let Some(name) = name {
                        flow.types.insert(name.clone(), crate::ast::InputType::Text);
                    }
                    flow.has_data = true;
                    flow.started = true;
                }
                StepKind::ParseJson { binding: name } => {
                    if !flow.has_data {
                        bail!("parse json requires preceding output");
                    }
                    binding(&Some(name.clone()), &mut flow.names, allow_rebinding)?;
                    flow.types.insert(name.clone(), crate::ast::InputType::Json);
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
                    binding(&Some(name.clone()), &mut flow.names, allow_rebinding)?;
                    flow.types.insert(name.clone(), *kind);
                }
                StepKind::EncodeJson { source } => {
                    check_value(source, &flow.names)?;
                    flow.has_data = true;
                    flow.started = true;
                }
                StepKind::Invoke { call, connection } => {
                    if *connection == Connection::Then && index == 0 && !initial_has_data {
                        bail!("then requires a preceding step");
                    }
                    let manifest = registry.resolve_call(call)?;
                    manifest.validate_with_types(call, &flow.types)?;
                    for value in call.parameters.values() {
                        check_value(value, &flow.names)?;
                    }
                    if manifest.builtin == Some(Builtin::Cd) {
                        if *connection == Connection::Pipe || pipe_at(program, index + 1) {
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
                                program.steps[index.saturating_sub(1)].kind,
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
                            || pipe_at(program, index + 1)
                        {
                            bail!("status capture requires a single uncached command outside a pipe group");
                        }
                        if call.status_binding == call.binding {
                            bail!("output and status require distinct binding names");
                        }
                    }
                    if call.cache {
                        if !manifest.cacheable {
                            bail!("applet {:?} is not declared cacheable", manifest.name);
                        }
                        if *connection == Connection::Pipe || pipe_at(program, index + 1) {
                            bail!("cache requires a finite then step, outside a live pipe group");
                        }
                    }
                    if call.binding.is_some() && pipe_at(program, index + 1) {
                        bail!("name results at the end of a pipe group; intermediate naming requires buffering");
                    }
                    binding(&call.binding, &mut flow.names, allow_rebinding)?;
                    if let Some(name) = &call.binding {
                        flow.types.insert(name.clone(), crate::ast::InputType::Text);
                    }
                    binding(&call.status_binding, &mut flow.names, allow_rebinding)?;
                    if let Some(name) = &call.status_binding {
                        flow.types
                            .insert(name.clone(), crate::ast::InputType::Integer);
                    }
                    flow.has_data = manifest.builtin.is_none();
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
            StepKind::If { condition, .. } => format!(
                "if {:?} is {}{:?}",
                condition.left,
                if condition.negated { "not " } else { "" },
                condition.right
            ),
            StepKind::Otherwise { .. } => "otherwise".into(),
            StepKind::EndIf => "end conditional".into(),
            StepKind::CallProgram { call, .. } => format!(
                "call program {:?}; explicit inputs {:?}; local scope",
                call.operation, call.parameters
            ),
            StepKind::EndCall => "return program output; restore caller scope and cwd".into(),
            StepKind::ReadCallInput { .. } => {
                "read reusable program input; preserve native byte transport".into()
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
            StepKind::EncodeJson { source } => {
                format!("encode {source:?} as compact JSON; bounded finite output")
            }
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
enum GroupArguments<'a> {
    Owned(Vec<String>),
    Borrowed(Vec<Cow<'a, str>>),
}

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
fn capture(mut reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(output);
        }
        if count > limit.saturating_sub(output.len()) {
            bail!("output exceeds maximum of {limit} bytes");
        }
        output.extend_from_slice(&chunk[..count]);
    }
}
enum Input {
    Empty,
    Bytes(Vec<u8>),
    File(File),
    Stdin,
}
impl Input {
    fn materialize(
        self,
        limit: usize,
        signals: &Signals,
        timeout: Option<Duration>,
    ) -> Result<Vec<u8>> {
        match self {
            Self::Empty => Ok(Vec::new()),
            Self::Bytes(bytes) => Ok(bytes),
            Self::File(file) => capture(file, limit),
            Self::Stdin => stdio::capture_stdin(limit, signals, timeout),
        }
    }
}

/// Own all children from spawn until wait. Errors kill and reap the whole group.
struct ManagedChild {
    child: Child,
    reaped: bool,
    stderr: Option<std::process::ChildStderr>,
    diagnostic: Vec<u8>,
}
#[derive(Default)]
struct Children(Vec<ManagedChild>);
fn kill_group(child: &Child) {
    // Each applet starts its own group. A negative PID addresses descendants
    // that retain that group, without touching the runner or its terminal.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
}
impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            if !child.reaped {
                kill_group(&child.child);
            }
        }
        for child in &mut self.0 {
            if !child.reaped {
                let _ = child.child.wait();
            }
        }
    }
}

#[derive(Default)]
struct InputWriter {
    handle: Option<std::thread::JoinHandle<Result<()>>>,
    stop: Option<Arc<AtomicBool>>,
}

impl InputWriter {
    fn spawn<W>(
        &mut self,
        mut sink: W,
        data: Vec<u8>,
        offset: usize,
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) where
        W: Write + AsRawFd + Send + 'static,
    {
        debug_assert!(offset <= data.len());
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());
        self.handle = Some(std::thread::spawn(move || {
            write_input(&mut sink, &data[offset..], &cancelled, &stop, deadline)
        }));
    }

    fn join(&mut self) -> Result<()> {
        if let Some(writer) = self.handle.take() {
            writer
                .join()
                .map_err(|_| anyhow!("input writer panicked"))??;
        }
        Ok(())
    }
}

impl Drop for InputWriter {
    fn drop(&mut self) {
        // Never join during error propagation. Stop the private writer even if
        // an escaped descendant retains a reader outside our process groups.
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

fn write_input(
    sink: &mut (impl Write + AsRawFd),
    data: &[u8],
    cancelled: &AtomicBool,
    stop: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    // This is the newly created pipe's private write end, owned by this worker.
    // It must not hold the global stdout/trace lock while feeding native stdin.
    let fd = sink.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error()).context("setting applet input nonblocking");
    }
    let mut offset = 0;
    loop {
        check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
            bail!("workflow cancelled while feeding applet input");
        }
        if offset == data.len() {
            return Ok(());
        }
        match sink.write(&data[offset..]) {
            Ok(0) => bail!("writing applet input made no progress"),
            Ok(count) => offset += count,
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ready =
                    unsafe { libc::poll(&mut descriptor, 1, deadline_poll_timeout(deadline, 20)) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(error).context("waiting for applet input");
                    }
                }
            }
            Err(error) => return Err(error).context("writing applet input"),
        }
    }
}

type StdinRemainder = (std::io::PipeWriter, Vec<u8>, usize);

fn prefill_stdin_pipe(data: Vec<u8>) -> Result<(std::io::PipeReader, Option<StdinRemainder>)> {
    let (reader, mut pipe_writer) = std::io::pipe().context("creating applet stdin pipe")?;
    let fd = pipe_writer.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error()).context("getting applet stdin pipe flags");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error())
            .context("setting applet stdin pipe nonblocking");
    }

    // Exactly one nonblocking write is attempted. EINTR and WouldBlock leave
    // the unaccepted suffix for the post-spawn writer thread.
    let write_result = pipe_writer.write(&data);
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } == -1 {
        return Err(std::io::Error::last_os_error()).context("restoring applet stdin pipe flags");
    }
    let written = match write_result {
        Ok(written) => written,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) =>
        {
            0
        }
        Err(error) => return Err(error).context("prefilling applet stdin pipe"),
    };

    if written == data.len() {
        drop(pipe_writer);
        Ok((reader, None))
    } else {
        Ok((reader, Some((pipe_writer, data, written))))
    }
}

struct ChildObserver {
    #[cfg(target_os = "macos")]
    queue: OwnedFd,
}
impl ChildObserver {
    #[cfg(target_os = "macos")]
    fn new() -> Option<Self> {
        let fd = unsafe { libc::kqueue() };
        if fd == -1 {
            return None;
        }
        // Darwin installs close-on-exec and close-on-fork before publishing
        // kqueue descriptors (XNU kqueue creation); no fcntl race or extra
        // descriptor configuration is needed here.
        let queue = unsafe { OwnedFd::from_raw_fd(fd) };
        Some(Self { queue })
    }

    #[cfg(not(target_os = "macos"))]
    fn new() -> Option<Self> {
        None
    }

    #[cfg(target_os = "macos")]
    fn raw_fd(&self) -> libc::c_int {
        self.queue.as_raw_fd()
    }

    #[cfg(not(target_os = "macos"))]
    fn raw_fd(&self) -> libc::c_int {
        -1
    }

    fn observe(&self, pid: u32) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let change = libc::kevent {
                ident: pid as _,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let no_wait = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let result = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    &no_wait,
                )
            };
            if result == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = pid;
        Ok(())
    }

    fn drain(&self) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
            let no_wait = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            loop {
                let ready = unsafe {
                    libc::kevent(
                        self.queue.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        events.len() as _,
                        &no_wait,
                    )
                };
                if ready == -1 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        return Ok(());
                    }
                    return Err(error);
                }
                if ready == 0 {
                    return Ok(());
                }
                // Only direct-child NOTE_EXIT registrations can enter this
                // queue, so draining until empty is bounded by this group.
            }
        }
        #[cfg(not(target_os = "macos"))]
        Ok(())
    }
}

struct Signals {
    cancelled: Arc<AtomicBool>,
    registrations: Vec<signal_hook::SigId>,
    capture_diagnostics: bool,
    deadline: Option<Instant>,
}
impl Signals {
    fn register() -> Result<Self> {
        let mut guard = Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            registrations: Vec::with_capacity(3),
            capture_diagnostics: false,
            deadline: None,
        };
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT] {
            guard.registrations.push(signal_hook::flag::register(
                signal,
                guard.cancelled.clone(),
            )?);
        }
        Ok(guard)
    }

    fn with_cancellation(cancelled: Arc<AtomicBool>, capture_diagnostics: bool) -> Result<Self> {
        Ok(Self {
            cancelled,
            registrations: Vec::new(),
            capture_diagnostics,
            deadline: None,
        })
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for registration in &self.registrations {
            signal_hook::low_level::unregister(*registration);
        }
    }
}
fn binary_path(binary: &str, cwd: &Path) -> Result<PathBuf> {
    // Keep the selected path spelling: multicall tools and symlink aliases
    // dispatch on argv[0], so canonicalizing to the target changes behavior.
    if binary.contains('/') {
        let candidate = cwd.join(binary);
        let metadata = fs::metadata(&candidate)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            bail!("{} is not an executable file", candidate.display());
        }
        Ok(candidate)
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        binary_path_from_path(binary, cwd, &path)
    }
}

fn binary_path_from_path(binary: &str, cwd: &Path, path: &std::ffi::OsStr) -> Result<PathBuf> {
    let mut candidate = PathBuf::new();
    for entry in std::env::split_paths(path) {
        candidate.clear();
        if entry.is_absolute() {
            // Path::join replaces its left-hand side for an absolute entry.
            // Pushing it directly avoids copying cwd and keeps the exact spelling.
            candidate.push(entry);
        } else {
            candidate.push(cwd);
            candidate.push(entry);
        }
        candidate.push(binary);

        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return Ok(candidate);
        }
    }
    Err(anyhow!("executable {binary:?} not found in PATH"))
}
fn event(enabled: bool, value: serde_json::Value, signals: &Signals) -> Result<()> {
    if enabled {
        stdio::write_with_deadline(
            libc::STDERR_FILENO,
            format!("{value}\n").as_bytes(),
            &signals.cancelled,
            signals.deadline,
        )?;
    }
    Ok(())
}
fn lazy_event(
    enabled: bool,
    signals: &Signals,
    value: impl FnOnce() -> serde_json::Value,
) -> Result<()> {
    if enabled {
        event(true, value(), signals)
    } else {
        Ok(())
    }
}

fn scan_child_statuses(
    children: &mut Children,
    calls: &[(&Invocation, &Manifest, usize)],
    trace: bool,
    signals: &Signals,
    observer: &mut Option<ChildObserver>,
    observer_ready: &mut bool,
    started: Instant,
) -> Result<()> {
    let drain_failed = *observer_ready
        && observer
            .as_ref()
            .is_some_and(|current| current.drain().is_err());
    *observer_ready = false;
    if drain_failed {
        // The readiness source is optional. On failure, use the periodic poll
        // fallback and preserve try_wait as the only child-status authority.
        *observer = None;
    }
    for (index, managed) in children.0.iter_mut().enumerate() {
        drain_diagnostic(managed)?;
        if !managed.reaped {
            if let Some(status) = managed.child.try_wait()? {
                // An applet must stay alive until its work is complete.
                // Reclaim descendants that outlive their owning applet.
                kill_group(&managed.child);
                managed.reaped = true;
                lazy_event(
                    trace,
                    signals,
                    || serde_json::json!({"event":"exit","line":calls[index].2,"applet":calls[index].1.name_for(calls[index].0),"status":status.code(),"group_elapsed_us":started.elapsed().as_micros()}),
                )?;
                if !status.success()
                    && !(calls[index].0.status_binding.is_some() && status.code().is_some())
                {
                    drain_diagnostic(managed)?;
                    return Err(CommandFailed {
                        line: calls[index].2,
                        operation: calls[index].1.name_for(calls[index].0).to_owned(),
                        status,
                        diagnostic: String::from_utf8_lossy(&managed.diagnostic)
                            .trim()
                            .to_owned(),
                    }
                    .into());
                }
            }
        }
    }
    if children.0.iter().all(|child| child.reaped) {
        observer.take();
    }
    Ok(())
}

/// A native command exited unsuccessfully. Timeouts, spawn/I/O errors and
/// cancellation use separate errors and are never converted into exit codes.
#[derive(Debug)]
pub struct CommandFailed {
    pub line: usize,
    pub operation: String,
    pub status: std::process::ExitStatus,
    pub diagnostic: String,
}
impl std::fmt::Display for CommandFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "line {}: applet {:?} failed ({}): {}",
            self.line, self.operation, self.status, self.diagnostic
        )
    }
}
impl std::error::Error for CommandFailed {}

struct GroupOutput {
    bytes: Vec<u8>,
    status: Option<i32>,
}

#[allow(clippy::too_many_arguments)]
fn execute_group(
    calls: &[(&Invocation, &Manifest, usize)],
    input: Input,
    prepared_first_call: Option<(PathBuf, Vec<String>)>,
    bindings: &BTreeMap<String, Value>,
    cwd: &Path,
    options: &RunOptions,
    launched: &mut usize,
    signals: &Signals,
) -> Result<GroupOutput> {
    execute_group_with_sink(
        calls,
        input,
        prepared_first_call,
        bindings,
        cwd,
        options,
        launched,
        signals,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_group_with_sink(
    calls: &[(&Invocation, &Manifest, usize)],
    input: Input,
    prepared_first_call: Option<(PathBuf, Vec<String>)>,
    bindings: &BTreeMap<String, Value>,
    cwd: &Path,
    options: &RunOptions,
    launched: &mut usize,
    signals: &Signals,
    mut sink: Option<&mut streaming::LineSink<'_>>,
) -> Result<GroupOutput> {
    let mut writer = InputWriter::default();
    let mut children = Children(Vec::with_capacity(calls.len()));
    let mut input = Some(input);
    let mut prepared_first_call = prepared_first_call;
    let mut previous = None;
    let started = Instant::now();
    let mut child_observer = ChildObserver::new();
    let mut observer_ready = false;
    let bounded_signals = Signals {
        cancelled: signals.cancelled.clone(),
        registrations: Vec::new(),
        capture_diagnostics: signals.capture_diagnostics,
        deadline: options
            .command_timeout
            .map(|duration| {
                started
                    .checked_add(duration)
                    .context("command timeout is too large")
            })
            .transpose()?,
    };
    let signals = &bounded_signals;
    for (index, (call, manifest, line)) in calls.iter().enumerate() {
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        check_deadline(signals.deadline)?;
        let (binary, argv) = match prepared_first_call.take() {
            Some((binary, argv)) => (binary, GroupArguments::Owned(argv)),
            None => {
                let argv = arguments_for_group_call(call, manifest, bindings)?;
                (binary_path(manifest.binary_for(call), cwd)?, argv)
            }
        };
        let mut command = Command::new(&binary);
        match &argv {
            GroupArguments::Owned(argv) => {
                command.args(argv);
            }
            GroupArguments::Borrowed(argv) => {
                command.args(argv.iter().map(|argument| argument.as_ref()));
            }
        }
        command
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(if signals.capture_diagnostics {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .process_group(0);
        let mut bytes = None;
        let mut prefilled_input = None;
        let stdin = if index != 0 {
            Stdio::from(previous.take().expect("prior child stdout"))
        } else {
            match input.take().expect("first input") {
                Input::File(file) => Stdio::from(file),
                Input::Stdin => Stdio::inherit(),
                Input::Bytes(data) if !data.is_empty() && data.len() <= libc::PIPE_BUF => {
                    let (reader, mut pipe_writer) =
                        std::io::pipe().context("creating applet stdin pipe")?;
                    pipe_writer
                        .write_all(&data)
                        .context("prefilling applet stdin pipe")?;
                    // Close before spawn so the child observes EOF after the
                    // buffered input without a background writer thread.
                    drop(pipe_writer);
                    Stdio::from(reader)
                }
                Input::Bytes(data)
                    if !data.is_empty() && data.len() <= SMALL_STDIN_PREFILL_LIMIT =>
                {
                    let (reader, remainder) = prefill_stdin_pipe(data)?;
                    prefilled_input = remainder;
                    Stdio::from(reader)
                }
                Input::Bytes(data) if !data.is_empty() => {
                    bytes = Some(data);
                    Stdio::piped()
                }
                _ => Stdio::null(),
            }
        };
        command.stdin(stdin);
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        check_deadline(signals.deadline)?;
        let mut child = command
            .spawn()
            .with_context(|| format!("line {line}: spawning {}", binary.display()))?;
        let observer_failed = child_observer
            .as_ref()
            .and_then(|observer| observer.observe(child.id()).err())
            .is_some_and(|error| error.raw_os_error() != Some(libc::ESRCH));
        if observer_failed {
            child_observer = None;
        }
        *launched += 1;
        let child_stdin = bytes
            .as_ref()
            .map(|_| child.stdin.take().expect("piped child stdin"));
        previous = child.stdout.take();
        let stderr = child.stderr.take();
        children.0.push(ManagedChild {
            child,
            reaped: false,
            stderr,
            diagnostic: Vec::new(),
        });
        if let Some((pipe_writer, data, offset)) = prefilled_input {
            writer.spawn(
                pipe_writer,
                data,
                offset,
                signals.cancelled.clone(),
                signals.deadline,
            );
        } else if let Some(data) = bytes {
            writer.spawn(
                child_stdin.expect("piped child stdin"),
                data,
                0,
                signals.cancelled.clone(),
                signals.deadline,
            );
        }
        if let Some(stderr) = &children.0.last().unwrap().stderr {
            let fd = stderr.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(std::io::Error::last_os_error())
                    .context("setting diagnostics nonblocking");
            }
        }
        // Parameter values are deliberately omitted from traces (may be secrets).
        lazy_event(
            options.trace,
            signals,
            || serde_json::json!({"event":"spawn", "line":line,"applet":manifest.name_for(call),"binary":binary,"pid":children.0.last().unwrap().child.id()}),
        )?;
    }
    let mut stdout = previous.expect("nonempty group");
    let fd = stdout.as_raw_fd();
    // Only the terminal pipe is monitored. Intermediate bytes travel directly
    // between child descriptors; the interpreter never copies them.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error()).context("setting terminal pipe nonblocking");
    }
    let mut diagnostic_poll_descriptors = Vec::with_capacity(if signals.capture_diagnostics {
        children.0.len() + 2
    } else {
        0
    });
    if signals.capture_diagnostics {
        for _ in 0..children.0.len() + 2 {
            diagnostic_poll_descriptors.push(libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            });
        }
    }
    let mut output = Vec::new();
    let mut eof = false;
    let mut chunk = [0u8; 32768];
    loop {
        check_deadline(signals.deadline)?;
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        scan_child_statuses(
            &mut children,
            calls,
            options.trace,
            signals,
            &mut child_observer,
            &mut observer_ready,
            started,
        )?;
        let mut reached_eof = false;
        if !eof {
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => {
                        eof = true;
                        reached_eof = true;
                        if let Some(sink) = sink.as_deref_mut() {
                            sink.finish(signals)?;
                        }
                        break;
                    }
                    Ok(count) => {
                        if let Some(sink) = sink.as_deref_mut() {
                            if sink.push(&chunk[..count], signals)? {
                                // Account for failures that became observable while
                                // reading or emitting the final requested record.
                                scan_child_statuses(
                                    &mut children,
                                    calls,
                                    options.trace,
                                    signals,
                                    &mut child_observer,
                                    &mut observer_ready,
                                    started,
                                )?;
                                // The script's output limit is deliberate successful
                                // termination. Children owns kill/reap on this return;
                                // killed upstream stages do not become native failures.
                                return Ok(GroupOutput {
                                    bytes: output,
                                    status: None,
                                });
                            }
                        } else {
                            if count > options.max_output_bytes.saturating_sub(output.len()) {
                                bail!(
                                    "output exceeds maximum of {} bytes",
                                    options.max_output_bytes
                                );
                            }
                            output.extend_from_slice(&chunk[..count]);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error).context("reading terminal pipe"),
                }
                if signals.cancelled.load(Ordering::Relaxed) {
                    bail!("workflow cancelled by signal");
                }
                check_deadline(signals.deadline)?;
            }
        }
        if reached_eof && children.0.iter().any(|child| !child.reaped) {
            // stdout EOF can precede child exit. Recheck once before falling
            // back to the short no-descriptor poll for cancellation/deadline.
            check_deadline(signals.deadline)?;
            if signals.cancelled.load(Ordering::Relaxed) {
                bail!("workflow cancelled by signal");
            }
            scan_child_statuses(
                &mut children,
                calls,
                options.trace,
                signals,
                &mut child_observer,
                &mut observer_ready,
                started,
            )?;
        }
        if eof && children.0.iter().all(|child| child.reaped) {
            break;
        }
        // Keep a short timeout for external cancellation and observer fallback.
        // Active kqueue observation makes a 20 ms EOF fallback sufficient;
        // unavailable observers retain the previous 1 ms EOF rescan.
        let maximum_wait_ms = if eof && child_observer.is_none() {
            1
        } else {
            20
        };
        let timeout_ms = deadline_poll_timeout(signals.deadline, maximum_wait_ms);
        let observer_notified;
        let ready = if signals.capture_diagnostics {
            let descriptors = &mut diagnostic_poll_descriptors;
            descriptors[0].fd = if eof { -1 } else { fd };
            descriptors[0].events = libc::POLLIN;
            descriptors[0].revents = 0;
            descriptors[1].fd = child_observer.as_ref().map_or(-1, ChildObserver::raw_fd);
            descriptors[1].events = libc::POLLIN;
            descriptors[1].revents = 0;
            for (descriptor, child) in descriptors[2..].iter_mut().zip(&children.0) {
                descriptor.fd = child
                    .stderr
                    .as_ref()
                    .map_or(-1, |stderr| stderr.as_raw_fd());
                descriptor.events = libc::POLLIN;
                descriptor.revents = 0;
            }
            let ready =
                unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, timeout_ms) };
            observer_notified = ready > 0 && descriptors[1].revents != 0;
            ready
        } else {
            let mut descriptors = [
                libc::pollfd {
                    fd: if eof { -1 } else { fd },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: child_observer.as_ref().map_or(-1, ChildObserver::raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout_ms) };
            observer_notified = ready > 0 && descriptors[1].revents != 0;
            ready
        };
        observer_ready |= observer_notified;
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("polling terminal pipe");
            }
        }
    }

    writer
        .join()
        .with_context(|| format!("line {}: feeding applet stdin", calls[0].2))?;
    let status = if calls.last().unwrap().0.status_binding.is_some() {
        children
            .0
            .last_mut()
            .unwrap()
            .child
            .try_wait()?
            .and_then(|status| status.code())
    } else {
        None
    };
    Ok(GroupOutput {
        bytes: output,
        status,
    })
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
fn cache_key(
    manifest: &Manifest,
    argv: &[String],
    executable: &Path,
    cwd: &Path,
    input: &[u8],
) -> Result<String> {
    cache_key_with_environment(manifest, argv, executable, cwd, input, std::env::vars_os)
}

fn cache_key_with_environment<F, I>(
    manifest: &Manifest,
    argv: &[String],
    executable: &Path,
    cwd: &Path,
    input: &[u8],
    environment_source: F,
) -> Result<String>
where
    F: FnOnce() -> I,
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"flow-cache-v1");
    let mut serialized = Vec::with_capacity(128);
    serde_json::to_writer(&mut serialized, manifest)?;
    hash_field(&mut hash, &serialized);
    serialized.clear();
    serde_json::to_writer(&mut serialized, argv)?;
    hash_field(&mut hash, &serialized);
    hash_field(&mut hash, cwd.as_os_str().as_bytes());
    hash_field(&mut hash, executable.as_os_str().as_bytes());
    let mut binary_hash = Sha256::new();
    let mut file = File::open(executable)?;
    let mut chunk = [0u8; 32768];
    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        binary_hash.update(&chunk[..count]);
    }
    hash_field(&mut hash, &binary_hash.finalize());
    hash_environment(&mut hash, environment_source());
    hash_field(&mut hash, input);
    Ok(format!("{:x}", hash.finalize()))
}

fn hash_environment<I>(hash: &mut Sha256, environment: I)
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    // Stable sorting preserves original order among duplicate names. Skipping
    // all but the last therefore matches BTreeMap collection's last-value rule.
    let mut environment: Vec<_> = environment.into_iter().collect();
    environment.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (index, (name, value)) in environment.iter().enumerate() {
        if environment
            .get(index + 1)
            .is_some_and(|(next_name, _)| next_name == name)
        {
            continue;
        }
        hash_field(hash, name.as_os_str().as_bytes());
        hash_field(hash, value.as_os_str().as_bytes());
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(
        ".flow-{}-{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("creating temporary output for {}", path.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

pub fn run(program: &Program, registry: &Registry, options: &RunOptions) -> Result<RunResult> {
    validate(program, registry)?;
    let options = configured_inputs(program, registry, options)?;
    if program.trigger.is_some() {
        bail!("triggered programs require run_service");
    }
    let signals = Signals::register()?;
    run_finite(program, registry, &options, &signals, Input::Empty, None)
}

fn run_finite(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    initial_input: Input,
    initial_binding: Option<(String, Value)>,
) -> Result<RunResult> {
    run_finite_inner(
        program,
        registry,
        options,
        signals,
        initial_input,
        initial_binding,
        None,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FiniteStateSource {
    Fresh,
    Restore,
}

#[allow(clippy::too_many_arguments)]
fn run_finite_inner(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    initial_input: Input,
    initial_binding: Option<(String, Value)>,
    journal: Option<&mut checkpoint::Journal>,
) -> Result<RunResult> {
    let state_source = if journal.is_some() {
        FiniteStateSource::Restore
    } else {
        FiniteStateSource::Fresh
    };
    run_finite_inner_with_state_source(
        program,
        registry,
        options,
        signals,
        initial_input,
        initial_binding,
        journal,
        state_source,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_finite_inner_with_state_source(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    initial_input: Input,
    initial_binding: Option<(String, Value)>,
    journal: Option<&mut checkpoint::Journal>,
    state_source: FiniteStateSource,
) -> Result<RunResult> {
    run_finite_state(
        program,
        registry,
        options,
        signals,
        initial_input,
        initial_binding.into_iter().collect(),
        journal,
        state_source,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_finite_state(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    initial_input: Input,
    initial_bindings: BTreeMap<String, Value>,
    mut journal: Option<&mut checkpoint::Journal>,
    state_source: FiniteStateSource,
    state_out: Option<&mut live::NodeState>,
) -> Result<RunResult> {
    let mut cwd = fs::canonicalize(&options.working_directory)?;
    let cache_override = options.cache_directory.as_ref().map(|path| cwd.join(path));
    let mut bindings = initial_bindings;
    let mut data = initial_input;
    let mut result = RunResult {
        output: Vec::new(),
        cache_hits: 0,
        launched_processes: 0,
    };
    let mut index = 0;
    let mut call_frames: Vec<calls::Frame<'_>> = Vec::new();
    if let Some(journal) = journal.as_deref_mut() {
        if state_source == FiniteStateSource::Restore {
            let state = journal.take_restored_state()?;
            index = journal.completed_nodes().last().map_or(0, |node| node.end);
            cwd = state.cwd;
            bindings = state.bindings;
            data = Input::Bytes(state.output);
            if index == program.steps.len() {
                journal.discard_removed_tail()?;
            }
        } else {
            // The service just committed this exact path with the event snapshot.
            cwd = options.working_directory.clone();
        }
        if !cwd.is_dir() {
            bail!("checkpoint working directory no longer exists");
        }
        lazy_event(
            options.trace,
            signals,
            || serde_json::json!({"event":"checkpoint_resume","restored_nodes":journal.completed_nodes().len(),"next_step":index}),
        )?;
    }
    if !program.inputs.is_empty() {
        for (name, value) in &options.inputs {
            bindings
                .entry(name.clone())
                .or_insert_with(|| value.clone());
        }
        bindings.extend(crate::inputs::resolve(&program.inputs, &bindings, true)?);
    }
    let mut journal_end = None;
    let mut journal_conditional = false;
    while index < program.steps.len() {
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        let step = &program.steps[index];
        if let Some(journal) = journal.as_deref_mut().filter(|_| journal_end.is_none()) {
            let end = resume::node_end(program, index);
            journal_end = Some(end);
            journal_conditional = matches!(
                step.kind,
                StepKind::If { .. } | StepKind::CallProgram { .. }
            );
            let fingerprint = resume::node_fingerprint(program, index, end, registry, &cwd)?;
            journal.begin_node(
                &fingerprint,
                index,
                end,
                step.line,
                &cwd,
                resume::retry_safe(program, index, end, registry)?,
            )?;
        }
        let mut jump_to = None;
        let execution = (|| -> Result<()> {
            match &step.kind {
                StepKind::If {
                    condition,
                    otherwise,
                    ..
                } => {
                    let matched = control::evaluate(condition, &bindings)
                        .with_context(|| format!("line {}", step.line))?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"condition","line":step.line,"matched":matched}),
                    )?;
                    if !matched {
                        jump_to = Some(index + otherwise);
                    }
                }
                StepKind::Otherwise { end } => jump_to = Some(index + end),
                StepKind::EndIf => {}
                StepKind::CallProgram {
                    call,
                    inputs,
                    connection,
                    ..
                } => {
                    let child = calls::bind(call, inputs, &bindings).with_context(|| {
                        format!("program {:?}, called at line {}", call.operation, step.line)
                    })?;
                    call_frames.push(calls::Frame {
                        name: &call.operation,
                        line: step.line,
                        binding: call.binding.as_deref(),
                        bindings: std::mem::replace(&mut bindings, child),
                        cwd: cwd.clone(),
                    });
                    if *connection == Connection::Start {
                        data = Input::Empty;
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"program_enter", "program":call.operation, "line":step.line}),
                    )?;
                }
                StepKind::EndCall => {
                    let frame = call_frames.pop().context("program return without call")?;
                    bindings = frame.bindings;
                    cwd = frame.cwd;
                    if let Some(name) = frame.binding {
                        let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                            options.max_output_bytes,
                            signals,
                            options.command_timeout,
                        )?;
                        bindings.insert(
                            name.to_owned(),
                            Value::String(
                                String::from_utf8(bytes.clone())
                                    .context("named program results must be UTF-8 text")?,
                            ),
                        );
                        data = Input::Bytes(bytes);
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"program_return", "program":frame.name, "line":frame.line}),
                    )?;
                }
                StepKind::UseFolder { path } => {
                    cwd = fs::canonicalize(path_value(path, &bindings, &cwd)?)?;
                    if !cwd.is_dir() {
                        bail!("line {}: use folder requires a directory", step.line);
                    }
                }
                StepKind::Read { binding, .. }
                | StepKind::ReadStdin { binding }
                | StepKind::ReadCallInput { binding } => {
                    data = match &step.kind {
                        StepKind::Read { path, .. } => {
                            let path = path_value(path, &bindings, &cwd)?;
                            Input::File(File::open(&path).with_context(|| {
                                format!("line {}: reading {}", step.line, path.display())
                            })?)
                        }
                        StepKind::ReadCallInput { .. } => {
                            std::mem::replace(&mut data, Input::Empty)
                        }
                        _ => Input::Stdin,
                    };
                    if let Some(name) = binding {
                        let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                            options.max_output_bytes,
                            signals,
                            options.command_timeout,
                        )?;
                        bindings.insert(
                            name.clone(),
                            Value::String(
                                String::from_utf8(bytes.clone())
                                    .context("named results must be UTF-8 text")?,
                            ),
                        );
                        data = Input::Bytes(bytes);
                    }
                }
                StepKind::ParseJson { binding } => {
                    let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                        options.max_output_bytes,
                        signals,
                        options.command_timeout,
                    )?;
                    let value = crate::structured::parse(&bytes)
                        .with_context(|| format!("line {}", step.line))?;
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"parse_json", "line":step.line, "input_bytes":bytes.len()}),
                    )?;
                    data = Input::Bytes(bytes);
                }
                StepKind::TakeJson {
                    pointer,
                    source,
                    kind,
                    binding,
                } => {
                    let source = structured_source(source, &bindings)?;
                    let value = crate::structured::take(pointer, &source, *kind, step.line)
                        .with_context(|| format!("line {}", step.line))?;
                    let value = value.clone();
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"take_json", "line":step.line, "type":kind.to_string()}),
                    )?;
                }
                StepKind::EncodeJson { source } => {
                    let source = structured_source(source, &bindings)?;
                    let bytes = crate::structured::encode(&source, options.max_output_bytes)
                        .with_context(|| format!("line {}", step.line))?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"encode_json", "line":step.line, "output_bytes":bytes.len()}),
                    )?;
                    data = Input::Bytes(bytes);
                }
                StepKind::Save { path, .. } => {
                    let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                        options.max_output_bytes,
                        signals,
                        options.command_timeout,
                    )?;
                    let destination = path_value(path, &bindings, &cwd)?;
                    atomic_write(&destination, &bytes)?;
                    if journal.is_some() {
                        File::open(destination.parent().unwrap_or(&cwd))?.sync_all()?;
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"save","line":step.line,"bytes":bytes.len()}),
                    )?;
                    // Preserve the pipeline value for later steps and final stdout.
                    data = Input::Bytes(bytes);
                }
                StepKind::Invoke { call, connection } => {
                    let manifest = registry.resolve_call(call)?;
                    if manifest.builtin == Some(Builtin::Cd) {
                        let parameters = call
                            .parameters
                            .iter()
                            .map(|(name, value)| Ok((name.clone(), resolve(value, &bindings)?)))
                            .collect::<Result<BTreeMap<_, _>>>()?;
                        let args = manifest.build_arguments(&parameters)?;
                        cwd = fs::canonicalize(cwd.join(&args[0]))
                            .with_context(|| format!("line {}: cd to {:?}", step.line, args[0]))?;
                        if !cwd.is_dir() {
                            bail!("line {}: cd requires a directory", step.line);
                        }
                        data = Input::Empty;
                    } else {
                        let input = if *connection != Connection::Start && manifest.accepts_stdin {
                            std::mem::replace(&mut data, Input::Empty)
                        } else {
                            Input::Empty
                        };
                        let mut calls = vec![(call, manifest, step.line)];
                        while pipe_at(program, index + 1) {
                            index += 1;
                            let next = &program.steps[index];
                            if let StepKind::Invoke { call, .. } = &next.kind {
                                calls.push((call, registry.resolve_call(call)?, next.line));
                            }
                        }
                        let bytes = if call.cache {
                            let input = input.materialize(
                                options.max_output_bytes,
                                signals,
                                options.command_timeout,
                            )?;
                            let argv = arguments_for_call(call, manifest, &bindings)?;
                            let executable = binary_path(manifest.binary_for(call), &cwd)?;
                            let key = cache_key(manifest, &argv, &executable, &cwd, &input)?;
                            let directory = cache_override
                                .clone()
                                .unwrap_or_else(|| cwd.join(".flow-cache"));
                            let path = directory.join(format!("{key}.bin"));
                            match File::open(&path) {
                                Ok(file) => {
                                    let bytes = capture(file, options.max_output_bytes)?;
                                    result.cache_hits += 1;
                                    lazy_event(
                                        options.trace,
                                        signals,
                                        || serde_json::json!({"event":"cache_hit","line":step.line,"applet":manifest.name_for(call),"key":key,"bytes":bytes.len()}),
                                    )?;
                                    bytes
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    let bytes = execute_group(
                                        &calls,
                                        Input::Bytes(input),
                                        Some((executable, argv)),
                                        &bindings,
                                        &cwd,
                                        options,
                                        &mut result.launched_processes,
                                        signals,
                                    )?
                                    .bytes;
                                    fs::create_dir_all(&directory)?;
                                    atomic_write(&path, &bytes)?;
                                    lazy_event(
                                        options.trace,
                                        signals,
                                        || serde_json::json!({"event":"cache_store","line":step.line,"applet":manifest.name_for(call),"key":key,"bytes":bytes.len()}),
                                    )?;
                                    bytes
                                }
                                Err(error) => return Err(error).context("reading cached result"),
                            }
                        } else {
                            let completed = execute_group(
                                &calls,
                                input,
                                None,
                                &bindings,
                                &cwd,
                                options,
                                &mut result.launched_processes,
                                signals,
                            )?;
                            if let Some(name) = &call.status_binding {
                                let code = completed
                                    .status
                                    .context("native command finished without an exit code")?;
                                bindings.insert(name.clone(), Value::Integer(i64::from(code)));
                            }
                            completed.bytes
                        };
                        if let Some(name) = &calls.last().unwrap().0.binding {
                            bindings.insert(
                                name.clone(),
                                Value::String(
                                    String::from_utf8(bytes.clone())
                                        .context("named results must be UTF-8 text")?,
                                ),
                            );
                        }
                        data = Input::Bytes(bytes);
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = execution {
            let error = call_frames.iter().rev().fold(error, |error, frame| {
                error.context(format!(
                    "program {:?}, called at line {}",
                    frame.name, frame.line
                ))
            });
            if let Some(journal) = journal.as_deref_mut() {
                if !journal.persistence_failed() {
                    journal.fail(
                        &format!("{error:#}"),
                        signals.cancelled.load(Ordering::Relaxed)
                            || error.is::<CommandTimedOut>()
                            || journal_conditional,
                    )?;
                }
            }
            return Err(error);
        }
        index = jump_to.unwrap_or(index + 1);
        if let Some(journal) = journal
            .as_deref_mut()
            .filter(|_| journal_end == Some(index))
        {
            journal_end = None;
            journal_conditional = false;
            let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                options.max_output_bytes,
                signals,
                options.command_timeout,
            )?;
            journal.complete_node_parts(
                &cwd,
                &bytes,
                bindings.iter().map(|(name, value)| (name.as_str(), value)),
            )?;
            data = Input::Bytes(bytes);
            lazy_event(
                options.trace,
                signals,
                || serde_json::json!({"event":"checkpoint_commit","completed_nodes":journal.completed_nodes().len(),"next_step":index}),
            )?;
        }
    }
    result.output = std::mem::replace(&mut data, Input::Empty).materialize(
        options.max_output_bytes,
        signals,
        options.command_timeout,
    )?;
    if let Some(state) = state_out {
        *state = live::NodeState {
            cwd,
            bindings,
            data: result.output.clone(),
        };
    }
    Ok(result)
}

/// Execute a component worker using an explicit cancellation token and port values.
pub fn run_with_context(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    bindings: BTreeMap<String, Value>,
    input: Vec<u8>,
    cancelled: Arc<AtomicBool>,
) -> Result<RunResult> {
    if program.trigger.is_some() {
        bail!("triggered programs require run_service");
    }
    if !options.inputs.is_empty() {
        crate::inputs::known_values(&program.inputs, &options.inputs)?;
    }
    validate_with_inputs(
        program,
        registry,
        bindings.keys().cloned().collect(),
        !bindings.is_empty() || !input.is_empty(),
    )?;
    if !program.inputs.is_empty() {
        let mut supplied = options.inputs.clone();
        supplied.extend(bindings.clone());
        let values = crate::inputs::resolve(&program.inputs, &supplied, true)?;
        preflight_inputs(program, registry, &values)?;
    }
    let signals = Signals::with_cancellation(cancelled, true)?;
    run_finite_state(
        program,
        registry,
        options,
        &signals,
        Input::Bytes(input),
        bindings,
        None,
        FiniteStateSource::Fresh,
        None,
    )
}
fn drain_diagnostic(child: &mut ManagedChild) -> Result<()> {
    let mut reached_eof = false;
    let Some(stderr) = child.stderr.as_mut() else {
        return Ok(());
    };
    let mut buffer = [0u8; 4096];
    for _ in 0..8 {
        match stderr.read(&mut buffer) {
            Ok(0) => {
                reached_eof = true;
                break;
            }
            Ok(count) => {
                let keep = count.min(8192usize.saturating_sub(child.diagnostic.len()));
                child.diagnostic.extend_from_slice(&buffer[..keep]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("reading applet diagnostics"),
        }
    }
    if reached_eof {
        // Removing the closed descriptor prevents poll from spinning on HUP.
        child.stderr.take();
    }
    Ok(())
}

#[cfg(test)]
mod child_observer_tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn kernel_creates_kqueue_close_on_exec() {
        let observer = ChildObserver::new().expect("creating test kqueue");
        let flags = unsafe { libc::fcntl(observer.raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    struct RestoreSigchld(libc::sigaction);
    impl Drop for RestoreSigchld {
        fn drop(&mut self) {
            unsafe {
                libc::sigaction(libc::SIGCHLD, &self.0, std::ptr::null_mut());
            }
        }
    }

    #[test]
    #[ignore = "isolated process-wide SIGCHLD disposition check"]
    fn runtime_observer_preserves_host_sigchld_ignore() {
        let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
        let mut ignored = unsafe { std::mem::zeroed::<libc::sigaction>() };
        unsafe {
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut previous),
                0
            );
            ignored.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut ignored.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &ignored, std::ptr::null_mut()),
                0
            );
        }
        let _restore = RestoreSigchld(previous);

        {
            let _signals =
                Signals::with_cancellation(Arc::new(AtomicBool::new(false)), true).unwrap();
            let _observer = ChildObserver::new();
        }

        let mut observed = unsafe { std::mem::zeroed::<libc::sigaction>() };
        unsafe {
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut observed),
                0
            );
        }
        assert_eq!(observed.sa_sigaction, libc::SIG_IGN);
    }
}

#[cfg(test)]
mod argv_construction_tests {
    use super::*;
    use tempfile::tempdir;

    fn opt_runtime_registry(json: &str) -> Registry {
        let dir = tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        fs::write(&path, json).unwrap();
        Registry::load(&path).unwrap()
    }

    #[test]
    fn opt_runtime_borrows_only_uncached_literal_arguments() {
        let registry = opt_runtime_registry(
            r#"{"name":"check","binary":"tool","parameters":{"text":{"type":"string","required":true}},"arguments":[{"literal":"prefix"},{"parameter":"text"}]}"#,
        );
        let manifest = registry.resolve("check").unwrap();
        let bindings = BTreeMap::from([("result".into(), Value::String("resolved".into()))]);
        let literal = Invocation {
            operation: "check".into(),
            direct: false,
            parameters: BTreeMap::from([("text".into(), Value::String("literal".into()))]),
            binding: None,
            cache: false,
            status_binding: None,
        };
        match arguments_for_group_call(&literal, manifest, &bindings).unwrap() {
            GroupArguments::Borrowed(arguments) => {
                let rendered: Vec<&str> =
                    arguments.iter().map(|argument| argument.as_ref()).collect();
                assert_eq!(rendered, ["prefix", "literal"]);
            }
            GroupArguments::Owned(_) => panic!("uncached literal arguments should borrow"),
        }

        let mut referenced = literal.clone();
        referenced
            .parameters
            .insert("text".into(), Value::Reference("result".into()));
        match arguments_for_group_call(&referenced, manifest, &bindings).unwrap() {
            GroupArguments::Owned(arguments) => assert_eq!(arguments, ["prefix", "resolved"]),
            GroupArguments::Borrowed(_) => panic!("reference results must use owned arguments"),
        }

        let mut cached = literal;
        cached.cache = true;
        match arguments_for_group_call(&cached, manifest, &bindings).unwrap() {
            GroupArguments::Owned(arguments) => assert_eq!(arguments, ["prefix", "literal"]),
            GroupArguments::Borrowed(_) => panic!("cached calls must use owned arguments"),
        }
    }

    fn sample_command_construction(
        manifest: &Manifest,
        empty: &BTreeMap<String, Value>,
        borrowed: bool,
        iterations: usize,
    ) -> u128 {
        let started = Instant::now();
        for _ in 0..iterations {
            let mut command = Command::new("unused-binary");
            if borrowed {
                let arguments = manifest.build_arguments_borrowed(empty).unwrap();
                command.args(arguments.iter().map(|argument| argument.as_ref()));
            } else {
                let arguments = manifest.build_arguments(empty).unwrap();
                command.args(&arguments);
            }
            std::hint::black_box(command);
        }
        started.elapsed().as_nanos()
    }

    #[test]
    #[ignore = "opt-in in-process argv construction timing; this never spawns a process"]
    fn opt_runtime_command_construction_benchmark_raw_samples() {
        let dir = tempdir().unwrap();
        for argument_count in [2usize, 256, 4096] {
            let arguments = (0..argument_count)
                .map(|index| serde_json::json!({"literal": format!("arg-{index}")}))
                .collect::<Vec<_>>();
            let manifest_json = serde_json::json!({
                "name": "benchmark",
                "binary": "unused-binary",
                "arguments": arguments,
            });
            let path = dir.path().join(format!("{argument_count}.json"));
            fs::write(&path, serde_json::to_vec(&manifest_json).unwrap()).unwrap();
            let registry = Registry::load(&path).unwrap();
            let manifest = registry.resolve("benchmark").unwrap();
            let empty = BTreeMap::new();
            let iterations = match argument_count {
                2 => 10_000,
                256 => 500,
                _ => 50,
            };
            for batch in 0..5 {
                let modes = if batch % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                };
                for borrowed in modes {
                    let elapsed =
                        sample_command_construction(manifest, &empty, borrowed, iterations);
                    println!(
                        "opt_argv_sample mode={} argc={argument_count} batch={batch} iterations={iterations} total_ns={elapsed} ns_per_op={:.1}",
                        if borrowed { "borrowed" } else { "owned" },
                        elapsed as f64 / iterations as f64,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod opt_manifest_path_cache_tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;
    use tempfile::tempdir;

    fn opt_manifest_registry(dir: &Path) -> Registry {
        let manifest_path = dir.join("unicode-manifest.json");
        fs::write(
            &manifest_path,
            r#"{"name":"λ","binary":"tool","arguments":[{"literal":"雪"}]}"#,
        )
        .unwrap();
        Registry::load(&manifest_path).unwrap()
    }

    #[test]
    fn opt_manifest_path_value_borrows_literal_and_one_level_string_reference() {
        let cwd = Path::new("/workspace/π");
        let bindings = BTreeMap::from([
            ("text".to_owned(), Value::String("nested/雪".to_owned())),
            ("number".to_owned(), Value::Integer(7)),
            ("reference".to_owned(), Value::Reference("text".to_owned())),
        ]);
        assert_eq!(
            path_value(&Value::String("literal/é".to_owned()), &bindings, cwd).unwrap(),
            cwd.join("literal/é")
        );
        assert_eq!(
            path_value(&Value::Reference("text".to_owned()), &bindings, cwd).unwrap(),
            cwd.join("nested/雪")
        );
        assert_eq!(
            path_value(&Value::Reference("reference".to_owned()), &bindings, cwd)
                .unwrap_err()
                .to_string(),
            "path result must contain text"
        );
        assert_eq!(
            path_value(&Value::Reference("number".to_owned()), &bindings, cwd)
                .unwrap_err()
                .to_string(),
            "path result must contain text"
        );
        assert_eq!(
            path_value(&Value::Reference("missing".to_owned()), &bindings, cwd)
                .unwrap_err()
                .to_string(),
            "undefined result \"missing\""
        );
    }

    fn opt_manifest_legacy_cache_key_with_environment<F, I>(
        manifest: &Manifest,
        argv: &[String],
        executable: &Path,
        cwd: &Path,
        input: &[u8],
        environment_source: F,
    ) -> Result<String>
    where
        F: FnOnce() -> I,
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        let mut hash = Sha256::new();
        hash_field(&mut hash, b"flow-cache-v1");
        hash_field(&mut hash, &serde_json::to_vec(manifest)?);
        hash_field(&mut hash, &serde_json::to_vec(argv)?);
        hash_field(&mut hash, cwd.as_os_str().as_bytes());
        hash_field(&mut hash, executable.as_os_str().as_bytes());
        let mut binary_hash = Sha256::new();
        let mut file = File::open(executable)?;
        let mut chunk = [0u8; 32768];
        loop {
            let count = file.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            binary_hash.update(&chunk[..count]);
        }
        hash_field(&mut hash, &binary_hash.finalize());
        let environment: BTreeMap<_, _> = environment_source().into_iter().collect();
        for (name, value) in environment {
            hash_field(&mut hash, name.as_os_str().as_bytes());
            hash_field(&mut hash, value.as_os_str().as_bytes());
        }
        hash_field(&mut hash, input);
        Ok(format!("{:x}", hash.finalize()))
    }

    fn opt_manifest_env_snapshot() -> Vec<(OsString, OsString)> {
        vec![
            (OsString::from("Z_LAST"), OsString::from("tail")),
            (OsString::from("DUP_KEY"), OsString::from("first value")),
            (
                OsString::from_vec(vec![0x80, b'K']),
                OsString::from_vec(vec![0xff, b'V']),
            ),
            (OsString::from("A_FIRST"), OsString::from("雪")),
            (OsString::from("DUP_KEY"), OsString::from("last value")),
        ]
    }

    fn opt_manifest_legacy_hash_environment<I>(hash: &mut Sha256, environment: I)
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        let environment: BTreeMap<_, _> = environment.into_iter().collect();
        for (name, value) in environment {
            hash_field(hash, name.as_os_str().as_bytes());
            hash_field(hash, value.as_os_str().as_bytes());
        }
    }

    #[test]
    fn opt_manifest_cache_key_matches_legacy_bytes_for_unicode_and_os_environment() {
        let dir = tempdir().unwrap();
        let registry = opt_manifest_registry(dir.path());
        let manifest = registry.resolve("λ").unwrap();
        let executable = dir.path().join("dummy-binary");
        fs::write(&executable, b"owned synthetic executable bytes").unwrap();
        let cwd = dir.path().join("工作目录");
        let argv = vec![String::new(), "東京".to_owned(), "café".to_owned()];
        let input = b"cache input";
        let environment = opt_manifest_env_snapshot();

        let optimized =
            cache_key_with_environment(manifest, &argv, &executable, &cwd, input, || {
                environment.clone()
            })
            .unwrap();
        let legacy = opt_manifest_legacy_cache_key_with_environment(
            manifest,
            &argv,
            &executable,
            &cwd,
            input,
            || environment,
        )
        .unwrap();
        assert_eq!(optimized, legacy);
    }

    #[test]
    #[ignore = "opt-in in-process cache-key timing; uses a temporary file and never spawns"]
    fn opt_manifest_cache_key_benchmark_raw_samples() -> Result<()> {
        let dir = tempdir()?;
        let registry = opt_manifest_registry(dir.path());
        let manifest = registry.resolve("λ").unwrap();
        let executable = dir.path().join("dummy-binary");
        fs::write(&executable, vec![b'x'; 1024])?;
        let cwd = dir.path().join("工作目录");
        let argv = vec![String::new(), "東京".to_owned(), "café".to_owned()];
        let input = b"cache input";
        let snapshot: Vec<_> = std::env::vars_os().collect();
        assert_eq!(
            cache_key_with_environment(manifest, &argv, &executable, &cwd, input, || snapshot
                .clone(),)?,
            opt_manifest_legacy_cache_key_with_environment(
                manifest,
                &argv,
                &executable,
                &cwd,
                input,
                || snapshot.clone(),
            )?
        );

        let iterations = 200;
        for batch in 0..5 {
            let modes = if batch % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for optimized in modes {
                let started = Instant::now();
                for _ in 0..iterations {
                    let key = if optimized {
                        cache_key(manifest, &argv, &executable, &cwd, input)?
                    } else {
                        opt_manifest_legacy_cache_key_with_environment(
                            manifest,
                            &argv,
                            &executable,
                            &cwd,
                            input,
                            std::env::vars_os,
                        )?
                    };
                    std::hint::black_box(key);
                }
                let elapsed = started.elapsed().as_nanos();
                println!(
                    "opt_cache_key_sample mode={} batch={batch} iterations={iterations} total_ns={elapsed} ns_per_op={:.1}",
                    if optimized { "reused_vec_sorted_env" } else { "to_vec_btreemap" },
                    elapsed as f64 / iterations as f64,
                );
            }
        }

        // Isolate the environment container/sort cost at ordinary and unusually
        // large sizes without changing the process-wide environment.
        for environment_size in [16usize, 512] {
            let environment: Vec<_> = (0..environment_size)
                .rev()
                .map(|index| {
                    (
                        OsString::from(format!("OPT_BENCH_{:04}", index % 128)),
                        OsString::from(format!("value-{index}")),
                    )
                })
                .collect();
            let iterations = if environment_size == 16 { 5_000 } else { 100 };
            for batch in 0..5 {
                let modes = if batch % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                };
                for optimized in modes {
                    let started = Instant::now();
                    for _ in 0..iterations {
                        let mut hash = Sha256::new();
                        if optimized {
                            hash_environment(&mut hash, environment.iter().cloned());
                        } else {
                            opt_manifest_legacy_hash_environment(
                                &mut hash,
                                environment.iter().cloned(),
                            );
                        }
                        std::hint::black_box(hash.finalize());
                    }
                    let elapsed = started.elapsed().as_nanos();
                    println!(
                        "opt_env_sample mode={} vars={environment_size} batch={batch} iterations={iterations} total_ns={elapsed} ns_per_op={:.1}",
                        if optimized { "sorted_vec" } else { "btreemap" },
                        elapsed as f64 / iterations as f64,
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod opt_manifest_binary_path_tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn opt_manifest_legacy_binary_path(binary: &str, cwd: &Path, path: &OsStr) -> Result<PathBuf> {
        // This preserves the original candidate construction as a parity oracle.
        if binary.contains('/') {
            let candidate = cwd.join(binary);
            let metadata = fs::metadata(&candidate)?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                bail!("{} is not an executable file", candidate.display());
            }
            Ok(candidate)
        } else {
            std::env::split_paths(path)
                .find_map(|entry| {
                    let candidate = cwd.join(entry).join(binary);
                    let metadata = fs::metadata(&candidate).ok()?;
                    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                        .then_some(candidate)
                })
                .ok_or_else(|| anyhow!("executable {binary:?} not found in PATH"))
        }
    }

    fn opt_manifest_join_path(entries: &[PathBuf]) -> OsString {
        std::env::join_paths(entries.iter().map(|entry| entry.as_os_str()))
            .expect("test PATH components are representable")
    }

    fn opt_manifest_write_file(path: &Path, mode: u32) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, b"synthetic executable metadata fixture")?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    fn opt_manifest_assert_path_parity(binary: &str, cwd: &Path, path: &OsStr) {
        let legacy = opt_manifest_legacy_binary_path(binary, cwd, path);
        let optimized = binary_path_from_path(binary, cwd, path);
        match (legacy, optimized) {
            (Ok(legacy), Ok(optimized)) => assert_eq!(
                legacy.as_os_str().as_bytes(),
                optimized.as_os_str().as_bytes(),
                "selected path bytes differ"
            ),
            (Err(legacy), Err(optimized)) => {
                assert_eq!(legacy.to_string(), optimized.to_string())
            }
            (legacy, optimized) => {
                panic!("lookup outcomes differ: legacy={legacy:?}, optimized={optimized:?}")
            }
        }
    }

    #[test]
    fn opt_manifest_binary_path_preserves_path_entry_spelling_and_os_bytes() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let binary = "probe";

        let mut double_slash_bytes = cwd.as_os_str().as_bytes().to_vec();
        double_slash_bytes.extend_from_slice(b"/double//child");
        let double_slash = PathBuf::from(OsString::from_vec(double_slash_bytes));
        let non_utf8 = PathBuf::from(OsString::from_vec(vec![b'n', 0x80, b'd']));
        let absolute = root.path().join("absolute");
        let entries = vec![
            PathBuf::from("relative"),
            PathBuf::from("."),
            PathBuf::new(),
            double_slash.clone(),
            non_utf8.clone(),
            absolute.clone(),
        ];

        for entry in &entries {
            let candidate = cwd.join(entry).join(binary);
            let path = opt_manifest_join_path(std::slice::from_ref(entry));
            // APFS rejects invalid UTF-8 filenames; that entry still exercises
            // byte-preserving PATH splitting and identical failed lookup.
            if entry == &non_utf8 && cfg!(target_os = "macos") {
                opt_manifest_assert_path_parity(binary, &cwd, &path);
                assert!(binary_path_from_path(binary, &cwd, &path).is_err());
                continue;
            }
            opt_manifest_write_file(&candidate, 0o755).unwrap();
            opt_manifest_assert_path_parity(binary, &cwd, &path);
            let selected = binary_path_from_path(binary, &cwd, &path).unwrap();
            assert_eq!(
                selected.as_os_str().as_bytes(),
                candidate.as_os_str().as_bytes()
            );
        }

        // A joined PATH retains non-UTF-8 bytes; splitting it must do so too.
        let path = opt_manifest_join_path(std::slice::from_ref(&non_utf8));
        let split = std::env::split_paths(&path).next().unwrap();
        assert_eq!(
            split.as_os_str().as_bytes(),
            non_utf8.as_os_str().as_bytes()
        );
        assert!(double_slash
            .as_os_str()
            .as_bytes()
            .windows(2)
            .any(|pair| pair == b"//"));
    }

    #[test]
    fn opt_manifest_binary_path_preserves_misses_permissions_and_late_hit() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let binary = "probe";

        let missing_path = opt_manifest_join_path(&[PathBuf::from("absent")]);
        opt_manifest_assert_path_parity(binary, &cwd, &missing_path);
        assert_eq!(
            binary_path_from_path(binary, &cwd, &missing_path)
                .unwrap_err()
                .to_string(),
            "executable \"probe\" not found in PATH"
        );

        let non_executable = PathBuf::from("no-execute");
        opt_manifest_write_file(&cwd.join(&non_executable).join(binary), 0o644).unwrap();
        let non_executable_path = opt_manifest_join_path(std::slice::from_ref(&non_executable));
        opt_manifest_assert_path_parity(binary, &cwd, &non_executable_path);
        assert_eq!(
            binary_path_from_path(binary, &cwd, &non_executable_path)
                .unwrap_err()
                .to_string(),
            "executable \"probe\" not found in PATH"
        );

        let mut entries = Vec::with_capacity(65);
        for index in 0..64 {
            let entry = PathBuf::from(format!("miss-{index:02}"));
            fs::create_dir_all(cwd.join(&entry)).unwrap();
            entries.push(entry);
        }
        let late_hit = PathBuf::from("late-hit");
        let expected = cwd.join(&late_hit).join(binary);
        opt_manifest_write_file(&expected, 0o755).unwrap();
        entries.push(late_hit);
        let late_path = opt_manifest_join_path(&entries);
        opt_manifest_assert_path_parity(binary, &cwd, &late_path);
        let selected = binary_path_from_path(binary, &cwd, &late_path).unwrap();
        assert_eq!(
            selected.as_os_str().as_bytes(),
            expected.as_os_str().as_bytes()
        );
    }

    #[test]
    fn opt_manifest_slash_binary_keeps_direct_lookup_and_metadata_error() {
        let root = tempdir().unwrap();
        let cwd = root.path().join("work");
        let binary = "nested/probe";
        let expected = cwd.join(binary);
        opt_manifest_write_file(&expected, 0o755).unwrap();
        let irrelevant_path = OsStr::new("unused");
        let legacy = opt_manifest_legacy_binary_path(binary, &cwd, irrelevant_path).unwrap();
        let current = binary_path(binary, &cwd).unwrap();
        assert_eq!(
            legacy.as_os_str().as_bytes(),
            expected.as_os_str().as_bytes()
        );
        assert_eq!(
            current.as_os_str().as_bytes(),
            expected.as_os_str().as_bytes()
        );

        let missing = "missing/probe";
        let legacy_error = opt_manifest_legacy_binary_path(missing, &cwd, irrelevant_path)
            .unwrap_err()
            .to_string();
        let current_error = binary_path(missing, &cwd).unwrap_err().to_string();
        assert_eq!(current_error, legacy_error);
        assert_ne!(
            current_error,
            "executable \"missing/probe\" not found in PATH"
        );
    }

    fn opt_manifest_lookup_sample(
        lookup: fn(&str, &Path, &OsStr) -> Result<PathBuf>,
        binary: &str,
        cwd: &Path,
        path: &OsStr,
        iterations: usize,
    ) -> u128 {
        let started = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(lookup(binary, cwd, path).expect("warm executable fixture"));
        }
        started.elapsed().as_nanos()
    }

    #[test]
    #[ignore = "opt-in PATH lookup timing; metadata is warmed and no process is launched"]
    fn opt_manifest_binary_path_benchmark_raw_samples() -> Result<()> {
        let root = tempdir()?;
        let cwd = root.path().join("work");
        fs::create_dir_all(&cwd)?;

        let single_entry = PathBuf::from("single");
        let single_candidate = cwd.join(&single_entry).join("probe");
        opt_manifest_write_file(&single_candidate, 0o755)?;
        let single_path = opt_manifest_join_path(std::slice::from_ref(&single_entry));

        let mut late_entries = Vec::with_capacity(65);
        for index in 0..64 {
            let entry = PathBuf::from(format!("miss-{index:02}"));
            fs::create_dir_all(cwd.join(&entry))?;
            late_entries.push(entry);
        }
        let late_entry = PathBuf::from("late-hit");
        opt_manifest_write_file(&cwd.join(&late_entry).join("probe"), 0o755)?;
        late_entries.push(late_entry);
        let late_path = opt_manifest_join_path(&late_entries);

        // Prime both code paths so subsequent batches include warmed metadata.
        for path in [&single_path, &late_path] {
            opt_manifest_legacy_binary_path("probe", &cwd, path)?;
            binary_path_from_path("probe", &cwd, path)?;
        }
        for (scenario, path, iterations) in [
            ("path1", &single_path, 2_000usize),
            ("path64_miss_prefix_late_hit", &late_path, 150usize),
        ] {
            for batch in 0..5 {
                let modes = if batch % 2 == 0 { [0, 1] } else { [1, 0] };
                for mode in modes {
                    let (name, lookup) = if mode == 0 {
                        (
                            "legacy",
                            opt_manifest_legacy_binary_path
                                as fn(&str, &Path, &OsStr) -> Result<PathBuf>,
                        )
                    } else {
                        (
                            "reused_buffer",
                            binary_path_from_path as fn(&str, &Path, &OsStr) -> Result<PathBuf>,
                        )
                    };
                    let elapsed =
                        opt_manifest_lookup_sample(lookup, "probe", &cwd, path, iterations);
                    println!(
                        "opt_manifest_binary_path_sample mode={name} scenario={scenario} batch={batch} iterations={iterations} total_ns={elapsed} ns_per_op={:.1}",
                        elapsed as f64 / iterations as f64,
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod input_writer_drop_tests {
    use super::InputWriter;
    use std::io::{PipeWriter, Write};
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    struct NotifyingPipeWriter {
        writer: PipeWriter,
        started: Option<mpsc::Sender<()>>,
        finished: Arc<AtomicBool>,
    }

    impl Write for NotifyingPipeWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            self.writer.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.writer.flush()
        }
    }

    impl AsRawFd for NotifyingPipeWriter {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            self.writer.as_raw_fd()
        }
    }

    impl Drop for NotifyingPipeWriter {
        fn drop(&mut self) {
            self.finished.store(true, Ordering::Release);
        }
    }

    fn fill_pipe(writer: &mut PipeWriter) {
        let fd = writer.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );

        let chunk = [0u8; 4096];
        loop {
            match writer.write(&chunk) {
                Ok(0) => panic!("pipe accepted a zero-length write"),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("filling stdin pipe: {error}"),
            }
        }

        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) }, 0);
    }

    #[test]
    fn dropping_input_writer_does_not_join_while_pipe_reader_is_retained() {
        let (reader, mut pipe_writer) = std::io::pipe().unwrap();
        fill_pipe(&mut pipe_writer);

        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let (started_tx, started_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            let mut input_writer = InputWriter::default();
            input_writer.spawn(
                NotifyingPipeWriter {
                    writer: pipe_writer,
                    started: Some(started_tx),
                    finished: worker_finished,
                },
                vec![1],
                0,
                Arc::new(AtomicBool::new(false)),
                None,
            );
            let write_started = started_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            thread::sleep(Duration::from_millis(20));
            drop(input_writer);
            dropped_tx.send(write_started).unwrap();
        });

        let returned_while_reader_open = dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or(false);
        let finish_deadline = Instant::now() + Duration::from_secs(1);
        while !finished.load(Ordering::Acquire) && Instant::now() < finish_deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let finished_with_reader_open = finished.load(Ordering::Acquire);
        // Always release the owned reader before assertions so regressions cannot
        // strand the test's writer. The fixed worker stops before this release.
        drop(reader);
        dropper.join().unwrap();
        assert!(
            returned_while_reader_open,
            "writer Drop blocked on the full pipe"
        );
        assert!(
            finished_with_reader_open,
            "dropping the owner failed to stop its writer while a reader remained open"
        );
    }

    #[test]
    fn joined_input_writer_observes_deadline_and_cancellation_with_a_retained_reader() {
        for cancel in [false, true] {
            let (reader, mut pipe_writer) = std::io::pipe().unwrap();
            fill_pipe(&mut pipe_writer);
            let cancelled = Arc::new(AtomicBool::new(false));
            let (release, retained) = mpsc::channel();
            let guard = thread::spawn(move || {
                let _ = retained.recv_timeout(Duration::from_secs(1));
                drop(reader);
            });
            let mut writer = InputWriter::default();
            writer.spawn(
                pipe_writer,
                vec![1],
                0,
                cancelled.clone(),
                (!cancel).then(|| Instant::now() + Duration::from_millis(30)),
            );
            if cancel {
                cancelled.store(true, Ordering::Relaxed);
            }
            let result = writer.join();
            let _ = release.send(());
            guard.join().unwrap();
            let error =
                result.expect_err("a retained reader must not hide cancellation or deadline");
            if cancel {
                assert!(error.to_string().contains("cancelled"));
            } else {
                assert!(error.is::<super::CommandTimedOut>());
            }
        }
    }
}
