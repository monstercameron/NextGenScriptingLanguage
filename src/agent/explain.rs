//! `execute explain FILE --json`: a per-step plan of native effects.
//!
//! The plan is derived from the parsed program and the resolved manifests. It
//! does not run anything. The walk matches every `StepKind` explicitly, with no
//! catch-all arm, so a new step kind fails to compile until it is classified.
//!
//! Three rules keep the plan fail-closed:
//!
//! - Called reusable programs are walked through their definitions, once. The
//!   parser inlines a body between `CallProgram` and `EndCall`; the inlined copy
//!   is skipped, using the call's relative `end` offset. A missing definition or
//!   a cycle is undeclared.
//! - An environment change that can alter which program runs (loader or
//!   interpreter variables, `PATH`, or any runtime-valued entry) marks the next
//!   invocation undeclared.
//! - The working directory is tracked. A write path is normalised against it,
//!   and a path that leaves the program's folder is flagged `escapes_cwd`. After
//!   a runtime folder change, relative write paths are runtime-determined.
//!
//! Effects come only from applet contracts that declare them. A row whose effects
//! cannot be stated has `effects: null` and `undeclared: true`.
//!
//! Paths, `escapes_cwd`, and folder tracking are advisory static analysis. They
//! are not containment guarantees. Enforcement is the capability policy.

use crate::ast::{
    Connection, InputDeclaration, InputSource, Invocation, LoopKind, Program, RedirectTarget,
    ReusableProgram, Step, StepKind, Value,
};
use crate::manifest::{Builtin, Registry};
use crate::privilege::is_privileged_invocation;
use anyhow::Result;
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, BTreeSet};

/// Protocol identifier of the `--json` object.
pub const PROTOCOL: &str = "execute.explain/2";
pub const LEGACY_PROTOCOL: &str = "execute.explain/1";

/// Environment names known to be inert: they change locale, time zone, or
/// terminal presentation, not what program runs or what it loads. Anything not
/// listed here, including every loader or interpreter variable, is sensitive.
/// This allowlist is fail-closed by design.
const INERT_NAMES: &[&str] = &[
    "LANG",
    "LANGUAGE",
    "TZ",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "COLUMNS",
    "LINES",
];

/// The current working directory, as the walk knows it.
#[derive(Clone, PartialEq, Eq)]
enum Cwd {
    /// Components relative to the program's starting directory.
    Known(Vec<String>),
    /// A runtime value or an absolute folder: relative paths are unknown.
    Unknown,
}

/// One planned step or invocation.
#[derive(Default)]
struct Row {
    line: usize,
    /// Stable step id (label or derived), set by the step walker.
    id: Option<String>,
    kind: &'static str,
    scope: Option<String>,
    invocation: bool,
    direct: bool,
    applet: Option<String>,
    binary: Option<String>,
    builtin: Option<bool>,
    process: Option<bool>,
    resolved_path: Option<String>,
    argv: Option<Vec<Json>>,
    legacy_argv: Option<Vec<String>>,
    parameters: Option<Json>,
    effects: Option<Vec<String>>,
    capabilities: Option<Vec<String>>,
    writes: Option<bool>,
    network: Option<bool>,
    reads: Option<bool>,
    cwd_change: Option<bool>,
    modifies_next_invocation: bool,
    environment_sensitive: bool,
    environment_tainted: bool,
    path: Option<String>,
    path_is_runtime: bool,
    escapes_cwd: bool,
    resolved: Option<bool>,
    undeclared: bool,
    /// The declared `and retry` policy, shown on the command it applies to.
    retry: Option<Json>,
    program_stdout: Option<&'static str>,
    binding: Option<String>,
}

impl Row {
    fn plain(line: usize, kind: &'static str, scope: Option<&str>) -> Self {
        Self {
            line,
            kind,
            scope: scope.map(str::to_owned),
            writes: Some(false),
            network: Some(false),
            reads: Some(false),
            cwd_change: Some(false),
            ..Self::default()
        }
    }

    fn to_json(&self, legacy: bool) -> Json {
        let mut row = json!({
            "id": self.id,
            "line": self.line,
            "kind": self.kind,
            "scope": self.scope,
            "direct": self.direct,
            "applet": self.applet,
            "binary": self.binary,
            "builtin": self.builtin,
            "process": self.process,
            "resolved_path": self.resolved_path,
            "argv": self.argv,
            "parameters": self.parameters,
            "effects": self.effects,
            "capabilities": self.capabilities,
            "writes": self.writes,
            "network": self.network,
            "reads": self.reads,
            "cwd_change": self.cwd_change,
            "modifies_next_invocation": self.modifies_next_invocation,
            "environment_sensitive": self.environment_sensitive,
            "environment_tainted": self.environment_tainted,
            "path": self.path,
            "path_is_runtime": self.path_is_runtime,
            "escapes_cwd": self.escapes_cwd,
            "resolved": self.resolved,
            "undeclared": self.undeclared,
            "privileged": row_privileged(self),
            "retry": self.retry,
            "program_stdout": self.program_stdout.unwrap_or("none"),
            "binding": self.binding,
        });
        if legacy {
            row["argv"] = json!(self.legacy_argv);
            row.as_object_mut().unwrap().remove("parameters");
        }
        row
    }
}

/// One declared flag or option: the command-line spelling an agent must use, and the
/// input it sets. Positional inputs are not listed here.
fn flag_entry(input: &InputDeclaration) -> Option<Json> {
    let (kind, name, alias) = match &input.source {
        Some(InputSource::Flag { name, alias }) => ("flag", name, alias),
        Some(InputSource::Option { name, alias }) => ("option", name, alias),
        _ => return None,
    };
    Some(json!({
        "kind": kind,
        "name": name,
        "alias": alias,
        "input": input.name,
        "type": input.kind.to_string(),
        "default": input
            .default
            .as_ref()
            .and_then(|value| crate::structured::to_json(value).ok()),
        "description": input.description,
    }))
}

/// Build the explain object for a parsed program. `program_path` is shown as
/// given. The caller validates the program first.
pub fn explain_json(program: &Program, registry: &Registry, program_path: &str) -> Result<Json> {
    explain_json_with_inputs(program, registry, program_path, &BTreeMap::new())
}

/// Describe supplied inputs while keeping sensitive values keyed and private.
pub fn explain_json_with_inputs(
    program: &Program,
    registry: &Registry,
    program_path: &str,
    inputs: &BTreeMap<String, Value>,
) -> Result<Json> {
    explain_json_version(program, registry, program_path, inputs, false)
}

/// Original v1 shape, including its intentionally incomplete argv.
pub fn explain_json_v1(program: &Program, registry: &Registry, program_path: &str) -> Result<Json> {
    explain_json_version(program, registry, program_path, &BTreeMap::new(), true)
}

fn explain_json_version(
    program: &Program,
    registry: &Registry,
    program_path: &str,
    inputs: &BTreeMap<String, Value>,
    legacy: bool,
) -> Result<Json> {
    let mut known = BTreeMap::new();
    let mut known_literals = BTreeMap::new();
    for declaration in program.inputs.iter().filter(|_| !legacy) {
        let resolved = crate::inputs::resolve(
            std::slice::from_ref(declaration),
            &inputs
                .iter()
                .filter(|(name, _)| *name == &declaration.name)
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            false,
        )
        .ok();
        let value = resolved
            .as_ref()
            .and_then(|values| values.get(&declaration.name));
        if let Some(value) = value {
            let shown = if crate::ast::is_sensitive(declaration) {
                let key = crate::agent::what_if_key::load_or_create()?;
                let mut message = declaration.name.as_bytes().to_vec();
                message.push(0);
                message.extend(serde_json::to_vec(&crate::inputs::literal_json(value)?)?);
                let digest = crate::agent::what_if_key::hmac_sha256(&key, &message);
                json!({"sensitive": true, "hmac_sha256": digest.iter().map(|b| format!("{b:02x}")).collect::<String>()})
            } else {
                known_literals.insert(declaration.name.clone(), value.clone());
                encode_literal(crate::inputs::literal_json(value)?)
            };
            known.insert(declaration.name.clone(), shown);
        }
    }
    let mut walker = Walker {
        registry,
        known,
        known_literals,
        definitions: &program.definitions,
        rows: Vec::new(),
        stack: Vec::new(),
        cwd: Cwd::Known(Vec::new()),
        environment_tainted_next: false,
        depth: 0,
        cwd_poisoned: false,
        group_tainted: false,
        initial_working_directory: std::env::current_dir().ok(),
        program_directory: crate::program_files::program_directory(std::path::Path::new(
            program_path,
        )),
    };
    if let Some(crate::ast::Trigger::Listen {
        source: crate::ast::ListenSource::Applet(call),
        line,
        ..
    }) = &program.trigger
    {
        walker.invocation(*line, "listen_source", call, None, true);
    }
    walker.steps(&program.steps, None);
    let mut plan = summary_object(program_path, &walker.rows, legacy);
    plan["inputs"] = program.inputs.iter().map(input_summary).collect();
    plan["flags"] = program.inputs.iter().filter_map(flag_entry).collect();
    Ok(plan)
}

/// One declared program input, with its allowed values when `one of` is used.
fn input_summary(input: &crate::ast::InputDeclaration) -> Json {
    let mut spec = json!({
        "name": input.name,
        "type": input.kind.to_string(),
        "required": input.default.is_none(),
        "line": input.line,
    });
    if let Some(choices) = &input.choices {
        spec["choices"] = choices.clone().into();
    }
    // A prompt is listed with its question, so a reviewer sees what will be asked.
    if let Some(crate::ast::InputSource::Prompt { question, secret }) = &input.source {
        spec["prompt"] = json!({ "question": question, "secret": secret });
    }
    spec
}

struct Walker<'a> {
    known: BTreeMap<String, Json>,
    known_literals: BTreeMap<String, Value>,
    registry: &'a Registry,
    definitions: &'a BTreeMap<String, ReusableProgram>,
    rows: Vec<Row>,
    /// Definitions currently being expanded, for cycle detection.
    stack: Vec<String>,
    cwd: Cwd,
    /// Set by an environment change that can alter the next command.
    environment_tainted_next: bool,
    /// Taint for the current pipeline group. Every stage of the group shares it.
    group_tainted: bool,
    /// Nesting depth of branch, loop, try, and called bodies.
    depth: usize,
    /// Set by a folder change nested in a body. The folder stays unknown after it.
    cwd_poisoned: bool,
    /// The process's starting directory, where runtime relative executables resolve.
    initial_working_directory: Option<std::path::PathBuf>,
    /// The calling program's directory. Called files are shown relative to it.
    program_directory: Option<std::path::PathBuf>,
}

impl Walker<'_> {
    /// Resolve the filesystem working directory when the explanation walk still
    /// has a known cwd. Runtime begins in the process's current directory;
    /// `program_directory` is for source-relative file lookup, not command cwd.
    fn known_working_directory(&self) -> Option<std::path::PathBuf> {
        let base = self.initial_working_directory.clone()?;
        match &self.cwd {
            Cwd::Known(components) => Some(
                components
                    .iter()
                    .fold(base, |path, component| path.join(component)),
            ),
            Cwd::Unknown => None,
        }
    }

    /// Walk a step list in order. A called definition's body is inlined after its
    /// `CallProgram`, using a relative `end` offset. The definition is walked once
    /// through the call, so the inlined copy is skipped. An invalid offset falls
    /// back to walking the inlined steps, so no step is skipped uncounted.
    fn steps(&mut self, steps: &[Step], scope: Option<&str>) {
        let ids = crate::ast::step_ids(steps);
        let mut index = 0;
        while index < steps.len() {
            let step = &steps[index];
            let target = match &step.kind {
                StepKind::CallProgram { end, .. } => index
                    .checked_add(*end)
                    .filter(|target| *target > index && *target <= steps.len()),
                _ => None,
            };
            let before = self.rows.len();
            let stdout = match step.kind.program_stdout() {
                Some(true)
                    if (matches!(&step.kind, StepKind::Invoke { call, .. } if !call.cache)
                        || matches!(&step.kind, StepKind::AwaitJob { .. }))
                        && !crate::ast::has_stdout_consumer(steps, index + 1) =>
                {
                    "stream"
                }
                Some(true) => "unbound_final_value",
                _ => "none",
            };
            let id = ids[index].clone();
            match (&step.kind, target) {
                (StepKind::CallProgram { call, .. }, Some(target)) => {
                    self.call_program(step.line, call, scope);
                    index = target;
                }
                _ => {
                    self.step(step.line, &step.kind, scope);
                    index += 1;
                }
            }
            // Any runtime binding may shadow an input. Stop substituting inputs
            // after such a step rather than claiming a stale value in the plan.
            let may_bind = match &step.kind {
                StepKind::BindValue { .. }
                | StepKind::CaptureBytes { .. }
                | StepKind::Read { .. }
                | StepKind::ReadStdin { .. }
                | StepKind::ReadCallInput { .. }
                | StepKind::ParseJson { .. }
                | StepKind::TakeJson { .. }
                | StepKind::TakeJsonDynamic { .. }
                | StepKind::TempScope { .. }
                | StepKind::Catch { .. }
                | StepKind::TaskGroup { .. }
                | StepKind::SpawnJob { .. }
                | StepKind::InspectJob { .. }
                | StepKind::AwaitJob { .. }
                | StepKind::Loop { .. }
                | StepKind::CallProgram { .. } => true,
                StepKind::Invoke { call, .. } | StepKind::IfCommand { call, .. } => {
                    call.binding.is_some()
                        || call.status_binding.is_some()
                        || call.result_binding.is_some()
                }
                _ => false,
            };
            if may_bind {
                self.known.clear();
                self.known_literals.clear();
            }
            // Rows from a nested body already carry their own ids; this step's rows take its id.
            for row in &mut self.rows[before..] {
                if row.program_stdout.is_none() {
                    row.program_stdout = Some(stdout);
                }
                row.id.get_or_insert_with(|| id.clone());
            }
        }
    }

    /// Classify one step. Every variant is named; there is no catch-all arm.
    fn step(&mut self, line: usize, kind: &StepKind, scope: Option<&str>) {
        // Branch, loop, and try blocks nest; their closers end the nesting.
        match kind {
            StepKind::If { .. }
            | StepKind::IfCommand { .. }
            | StepKind::Loop { .. }
            | StepKind::Try { .. } => self.depth += 1,
            StepKind::EndIf | StepKind::EndLoop { .. } | StepKind::EndTry => {
                self.depth = self.depth.saturating_sub(1);
            }
            _ => {}
        }
        match kind {
            StepKind::TempScope { kind: temp, .. } => {
                // The scope creates a path other commands write to, so it is a write.
                let kind = match temp {
                    crate::ast::TempKind::File => "temporary_file",
                    crate::ast::TempKind::Directory => "temporary_directory",
                };
                self.file_write(line, kind, None, scope);
            }
            StepKind::EndTempScope { .. } => self.push_plain(line, "end_temporary", scope),
            StepKind::TaskGroup { tasks, .. } => {
                for call in tasks {
                    self.invocation(line, "task_group", call, scope, true);
                }
            }
            StepKind::SpawnJob { stages, .. } => {
                for stage in stages {
                    self.invocation(line, "spawn_job", &stage.call, scope, true);
                }
            }
            StepKind::InspectJob { .. } => self.push_plain(line, "inspect_job", scope),
            StepKind::AwaitJob { .. } => self.push_plain(line, "await_job", scope),
            StepKind::CancelJob { .. } => self.push_plain(line, "cancel_job", scope),
            StepKind::Loop { kind, .. } => match kind {
                LoopKind::WhileCommand { call, .. } => {
                    self.invocation(line, "while_command", call, scope, true);
                }
                LoopKind::Repeat { .. } => self.push_plain(line, "loop_repeat", scope),
                LoopKind::ForEach { .. } => self.push_plain(line, "loop_for_each", scope),
                LoopKind::While { .. } => self.push_plain(line, "loop_while", scope),
            },
            StepKind::EndLoop { .. } => self.push_plain(line, "end_loop", scope),
            StepKind::If { .. } => self.push_plain(line, "if", scope),
            StepKind::IfCommand { call, .. } => {
                self.invocation(line, "if_command", call, scope, true);
            }
            StepKind::Otherwise { .. } => self.push_plain(line, "otherwise", scope),
            StepKind::EndIf => self.push_plain(line, "end_if", scope),
            // Reached only when the offset is invalid. The inlined body is then
            // walked as ordinary steps, so this marker must not walk it again.
            StepKind::CallProgram { .. } => self.push_plain(line, "call_program", scope),
            StepKind::EndCall => self.push_plain(line, "end_call", scope),
            StepKind::BreakLoop => self.push_plain(line, "break_loop", scope),
            StepKind::ContinueLoop => self.push_plain(line, "continue_loop", scope),
            StepKind::Return => self.push_plain(line, "return", scope),
            StepKind::Exit { .. } => self.push_plain(line, "exit", scope),
            StepKind::Fail { .. } => self.push_plain(line, "fail", scope),
            StepKind::Try { .. } => self.push_plain(line, "try", scope),
            StepKind::Catch { .. } => self.push_plain(line, "catch", scope),
            StepKind::Finally => self.push_plain(line, "finally", scope),
            StepKind::EndTry => self.push_plain(line, "end_try", scope),
            StepKind::ReadCallInput { .. } => self.push_plain(line, "call_input", scope),
            StepKind::UseFolder { path } => {
                let mut row = Row::plain(line, "use_folder", scope);
                row.cwd_change = Some(true);
                self.change_folder(&mut row, path);
                self.rows.push(row);
            }
            StepKind::UseValue { .. } => self.push_plain(line, "use_value", scope),
            StepKind::BindValue { binding, .. } => {
                let mut row = Row::plain(line, "bind_value", scope);
                row.binding = Some(binding.clone());
                row.process = Some(false);
                self.rows.push(row);
            }
            StepKind::CaptureBytes { .. } => self.push_plain(line, "capture_bytes", scope),
            StepKind::UseBytes { .. } => self.push_plain(line, "use_bytes", scope),
            StepKind::OnInterrupt { .. } => self.push_plain(line, "on_interrupt", scope),
            StepKind::Read { path, .. } => {
                let mut row = Row::plain(line, "read", scope);
                row.reads = Some(true);
                set_path(&mut row, path);
                self.rows.push(row);
            }
            StepKind::ReadStdin { .. } => {
                // Standard input may be a file, so treat it as a read.
                let mut row = Row::plain(line, "read_stdin", scope);
                row.reads = Some(true);
                self.rows.push(row);
            }
            StepKind::ParseJson { .. } => self.push_plain(line, "parse_json", scope),
            StepKind::TakeJson { .. } => self.push_plain(line, "take_json", scope),
            StepKind::TakeJsonDynamic { .. } => self.push_plain(line, "take_json", scope),
            StepKind::EncodeJson { .. } => self.push_plain(line, "encode_json", scope),
            StepKind::Invoke { call, connection } => {
                // A `pipe to` stage continues the current pipeline group, so it
                // inherits that group's taint. Other connections start a group.
                let starts_group = *connection != Connection::Pipe;
                self.invocation(line, "invoke", call, scope, starts_group);
            }
            StepKind::Save { path, .. } => self.file_write(line, "save", Some(path), scope),
            StepKind::Append { path } => self.file_write(line, "append", Some(path), scope),
            StepKind::Environment { set, unset } => {
                // A change to a loader or interpreter variable, or any runtime
                // value, can change which program runs. Taint the next command.
                let sensitive = set
                    .iter()
                    .any(|(name, value)| is_sensitive_name(name) || has_reference(value))
                    || unset.iter().any(|name| is_sensitive_name(name));
                let mut row = Row::plain(line, "environment", scope);
                row.modifies_next_invocation = true;
                row.environment_sensitive = sensitive;
                self.environment_tainted_next |= sensitive;
                self.rows.push(row);
            }
            StepKind::Interactive => {
                // Hands the terminal to the next native command.
                let mut row = Row::plain(line, "interactive", scope);
                row.modifies_next_invocation = true;
                self.rows.push(row);
            }
            StepKind::HandOver => {
                // Replaces the script with the next native command.
                let mut row = Row::plain(line, "hand over to", scope);
                row.modifies_next_invocation = true;
                self.rows.push(row);
            }
            StepKind::LaunchDetached { output } => {
                // Starts the next native command unowned; the script does not wait for it.
                // Its output file is a write, reported like a redirect target.
                if let Some(path) = output {
                    self.file_write(line, "launch detached output", Some(path), scope);
                    if let Some(row) = self.rows.last_mut() {
                        row.modifies_next_invocation = true;
                    }
                }
                let mut row = Row::plain(line, "launch detached", scope);
                row.modifies_next_invocation = true;
                self.rows.push(row);
            }
            StepKind::Redirect { target, path } => match target {
                RedirectTarget::StderrFile | RedirectTarget::StderrAppend => {
                    self.file_write(line, "redirect", path.as_ref(), scope);
                    if let Some(row) = self.rows.last_mut() {
                        row.modifies_next_invocation = true;
                    }
                }
                RedirectTarget::StderrMerged | RedirectTarget::StdoutToStderr => {
                    let mut row = Row::plain(line, "redirect", scope);
                    row.modifies_next_invocation = true;
                    self.rows.push(row);
                }
            },
        }
    }

    fn push_plain(&mut self, line: usize, kind: &'static str, scope: Option<&str>) {
        self.rows.push(Row::plain(line, kind, scope));
    }

    /// A step that writes a file. The path is normalised against the working
    /// directory. A runtime destination is recorded as runtime, and never guessed.
    fn file_write(
        &mut self,
        line: usize,
        kind: &'static str,
        path: Option<&Value>,
        scope: Option<&str>,
    ) {
        let mut row = Row::plain(line, kind, scope);
        row.writes = Some(true);
        if let Some(path) = path {
            self.resolve_write_path(&mut row, path);
        }
        self.rows.push(row);
    }

    fn resolve_write_path(&self, row: &mut Row, path: &Value) {
        let Value::String(text) = path else {
            row.path_is_runtime = true;
            return;
        };
        // Lexical checks are advisory. A `..` component or an absolute path is
        // always flagged, whether or not it would normalise inside the folder.
        if is_absolute(text) || has_parent_component(text) {
            row.path = Some(text.clone());
            row.escapes_cwd = true;
            return;
        }
        // A nested folder change stays in effect even after a called body restores
        // the caller's folder, so the destination is runtime from here on.
        if self.cwd_poisoned {
            row.path_is_runtime = true;
            return;
        }
        match &self.cwd {
            Cwd::Unknown => row.path_is_runtime = true,
            Cwd::Known(base) => {
                let (normalised, _) = join_relative(base, text);
                row.path = Some(normalised);
            }
        }
    }

    /// Apply a folder change: a literal relative folder moves the known position,
    /// while an absolute or runtime folder makes relative paths unknown.
    fn change_folder(&mut self, row: &mut Row, path: &Value) {
        // A folder change inside a branch, loop, try block, or called body may not
        // happen on every path. The folder is unknown from here to the end of the
        // walk, and this does not recover.
        if self.depth > 0 {
            self.cwd_poisoned = true;
        }
        let Value::String(text) = path else {
            row.path_is_runtime = true;
            self.cwd = Cwd::Unknown;
            return;
        };
        row.path = Some(text.clone());
        let lexical_escape = is_absolute(text) || has_parent_component(text);
        row.escapes_cwd = lexical_escape;
        if self.cwd_poisoned || lexical_escape {
            self.cwd = Cwd::Unknown;
            return;
        }
        let normalised = match &self.cwd {
            Cwd::Known(base) => join_relative(base, text).0,
            Cwd::Unknown => return,
        };
        self.cwd = Cwd::Known(
            normalised
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        );
    }

    fn invocation(
        &mut self,
        line: usize,
        kind: &'static str,
        call: &Invocation,
        scope: Option<&str>,
        starts_group: bool,
    ) {
        // An environment change before a group's first command taints the whole
        // group. A change before a `pipe to` stage taints the group from there on.
        // Every stage in a tainted group is undeclared, not only the first.
        let pending = std::mem::take(&mut self.environment_tainted_next);
        if starts_group {
            self.group_tainted = pending;
        } else {
            self.group_tainted |= pending;
        }
        let tainted = self.group_tainted;
        let mut row = Row {
            line,
            kind,
            scope: scope.map(str::to_owned),
            invocation: true,
            direct: call.direct,
            environment_tainted: tainted,
            ..Row::default()
        };
        // A declared retry is shown whether or not the command resolves. `idempotent`
        // reflects the manifest-level retry-safety rule used for replay.
        row.retry = call.retry.map(|policy| {
            let idempotent = !call.direct
                && self
                    .registry
                    .resolve_call(call)
                    .is_ok_and(crate::runtime::resume::applet_retry_safe);
            json!({
                "attempts": u64::from(policy.retries) + 1,
                "backoff_ms": policy.backoff_ms,
                "doubling": policy.doubling,
                "even_if_uncertain": policy.even_if_uncertain,
                "idempotent": idempotent,
            })
        });
        let empty = BTreeMap::new();
        let empty_literals = BTreeMap::new();
        let known = if scope.is_none() { &self.known } else { &empty };
        let parameters: BTreeMap<String, Json> = call
            .parameters
            .iter()
            .map(|(name, value)| (name.clone(), display_value(value, known)))
            .collect();
        row.argv = Some(Vec::new());
        if !call.direct {
            row.parameters = Some(json!(parameters));
        }
        if call.direct {
            row.argv = Some(match parameters.get("arguments") {
                Some(Json::Array(items)) => items
                    .iter()
                    .map(|item| match item {
                        Json::Bool(_) | Json::Number(_) => Json::String(item.to_string()),
                        _ => item.clone(),
                    })
                    .collect(),
                Some(value) => vec![value.clone()],
                None => vec![],
            });
            // A quoted executable bypasses the catalog, so its effects are unknown.
            row.binary = Some(call.operation.clone());
            row.process = Some(true);
            if let Some(cwd) = self.known_working_directory() {
                let resolved = crate::runtime::binary_path(&call.operation, &cwd);
                row.resolved = Some(resolved.is_ok());
                row.resolved_path = resolved
                    .ok()
                    .and_then(|path| std::fs::canonicalize(path).ok())
                    .map(|path| path.to_string_lossy().into_owned());
            }
            row.undeclared = true;
            self.rows.push(row);
            return;
        }
        let Ok(manifest) = self.registry.resolve_call(call) else {
            row.argv = Some(vec![
                json!({"unresolved_applet": call.operation, "parameters": parameters}),
            ]);
            row.applet = Some(call.operation.clone());
            row.resolved = Some(false);
            row.undeclared = true;
            self.rows.push(row);
            return;
        };
        row.legacy_argv = if call.parameters.values().any(has_reference) {
            None
        } else {
            manifest.build_arguments(&call.parameters).ok()
        };
        row.resolved = Some(true);
        row.applet = Some(manifest.name_for(call).to_owned());
        row.binary = Some(manifest.binary_for(call).to_owned());
        row.builtin = Some(manifest.builtin.is_some());
        row.process = Some(
            manifest.builtin.is_none()
                && !matches!(
                    manifest.execution_backend(),
                    crate::contracts::AppletBackend::InProcess
                        | crate::contracts::AppletBackend::ProgramModule
                        | crate::contracts::AppletBackend::Remote
                ),
        );
        // When every parameter is known and public, use the same manifest
        // renderer as execution (including path expansion and JSON arguments).
        let literals = if scope.is_none() {
            &self.known_literals
        } else {
            &empty_literals
        };
        let resolved: Option<BTreeMap<String, Value>> = call
            .parameters
            .iter()
            .map(|(name, value)| resolve_static(value, literals).map(|value| (name.clone(), value)))
            .collect();
        let rendered = resolved.and_then(|values| manifest.build_arguments(&values).ok());
        row.argv = Some(
            rendered
                .map(|args| args.into_iter().map(Json::String).collect())
                .unwrap_or_else(|| manifest.explain_arguments(&parameters)),
        );
        // The builtin `cd` changes the working directory for later steps.
        // Detect the builtin the way the runtime does: by its manifest field, not
        // by name. A destination that is not a literal leaves the folder unknown.
        if manifest.builtin == Some(Builtin::Cd) {
            row.cwd_change = Some(true);
            if let Some(path) = call.parameters.get("path") {
                let path = path.clone();
                self.change_folder(&mut row, &path);
            } else {
                self.cwd = Cwd::Unknown;
            }
        }
        match manifest.execution_contract() {
            Some(_) if tainted => {
                // A tainted environment can change what this program does, so its
                // declared effects no longer describe this run.
                row.undeclared = true;
            }
            Some(contract) => {
                row.effects = Some(contract.effects.clone());
                row.capabilities = Some(contract.capabilities.clone());
                row.writes = Some(
                    contract
                        .effects
                        .iter()
                        .any(|effect| is_write_effect(effect)),
                );
                row.network = Some(
                    contract
                        .effects
                        .iter()
                        .any(|effect| effect.starts_with("network.")),
                );
            }
            None => row.undeclared = true,
        }
        self.rows.push(row);
        if manifest.execution_backend() == crate::manifest::AppletBackend::ProgramModule {
            // A module whose body calls itself, directly or through another module, is
            // shown once and marked unresolved, so the gate refuses it by default.
            let key = format!("module:{}", call.operation);
            let module = (!self.stack.contains(&key))
                .then_some(manifest)
                .and_then(|manifest| {
                    manifest
                        .execution_contract()
                        .and_then(|contract| contract.program.as_ref())
                        .zip(manifest.origin())
                        .and_then(|(reference, origin)| {
                            let root = origin.parent()?;
                            crate::modules::load_program_module(reference, origin, root).ok()
                        })
                });
            let Some(module) = module else {
                if let Some(row) = self.rows.last_mut() {
                    row.resolved = Some(false);
                    row.undeclared = true;
                    row.effects = None;
                    row.capabilities = None;
                }
                return;
            };
            self.stack.push(key);
            self.depth += 1;
            if let Some(definition) = module.finite_definition() {
                self.steps(&definition.steps, Some(&call.operation));
            } else {
                if let Some(crate::ast::Trigger::Listen {
                    source: crate::ast::ListenSource::Applet(source),
                    line,
                    ..
                }) = &module.program.trigger
                {
                    self.invocation(*line, "listen_source", source, Some(&call.operation), true);
                }
                self.steps(&module.program.steps, Some(&call.operation));
            }
            self.depth = self.depth.saturating_sub(1);
            self.stack.pop();
        }
    }

    /// A reusable program call. Its steps are walked in place, so their effects
    /// count toward this plan. A cycle or missing definition is undeclared. The
    /// working directory is restored afterwards, as the call's `EndCall` does.
    fn call_program(&mut self, line: usize, call: &Invocation, scope: Option<&str>) {
        let name = call.operation.clone();
        let mut row = Row::plain(line, "call_program", scope);
        // A file call is shown relative to the file that makes it: the innermost called
        // file on the stack, or the top-level program when the call is in that program.
        let caller = self
            .stack
            .iter()
            .rev()
            .find_map(|open| {
                open.strip_prefix(crate::parser::PROGRAM_FILE_PREFIX)
                    .and_then(|path| std::path::Path::new(path).parent())
                    .map(std::path::Path::to_path_buf)
            })
            .or_else(|| self.program_directory.clone());
        row.applet = Some(crate::program_files::display_key(&name, caller.as_deref()));
        if self.stack.contains(&name) {
            row.resolved = Some(false);
            row.undeclared = true;
            self.rows.push(row);
            return;
        }
        // Copy the shared reference so the definition does not borrow `self`.
        let definitions = self.definitions;
        let Some(definition) = definitions.get(&name) else {
            row.resolved = Some(false);
            row.undeclared = true;
            self.rows.push(row);
            return;
        };
        row.resolved = Some(true);
        self.rows.push(row);
        let saved_cwd = self.cwd.clone();
        self.stack.push(name.clone());
        // A called body may run on some paths only, so its folder changes are nested.
        self.depth += 1;
        self.steps(&definition.steps, Some(&name));
        self.depth = self.depth.saturating_sub(1);
        self.stack.pop();
        self.cwd = saved_cwd;
    }
}

// Keep literal records structurally distinct from references and secret markers.
fn encode_literal(value: Json) -> Json {
    match value {
        Json::Array(items) => Json::Array(items.into_iter().map(encode_literal).collect()),
        Json::Object(fields) => {
            json!({"record": fields.into_iter().map(|(key, value)| (key, encode_literal(value))).collect::<serde_json::Map<String, Json>>() })
        }
        scalar => scalar,
    }
}

fn resolve_static(value: &Value, known: &BTreeMap<String, Value>) -> Option<Value> {
    match value {
        Value::Reference(name) => known.get(name).cloned(),
        Value::List(items) => items
            .iter()
            .map(|v| resolve_static(v, known))
            .collect::<Option<Vec<_>>>()
            .map(Value::List),
        Value::Record(fields) => fields
            .iter()
            .map(|(k, v)| resolve_static(v, known).map(|v| (k.clone(), v)))
            .collect::<Option<BTreeMap<_, _>>>()
            .map(Value::Record),
        literal => Some(literal.clone()),
    }
}

fn display_value(value: &Value, known: &BTreeMap<String, Json>) -> Json {
    match value {
        Value::Reference(name) => known
            .get(name)
            .cloned()
            .unwrap_or_else(|| json!({"ref": name})),
        Value::List(items) => Json::Array(
            items
                .iter()
                .map(|item| display_value(item, known))
                .collect(),
        ),
        Value::Record(fields) => {
            json!({"record": fields.iter().map(|(name, value)| (name.clone(), display_value(value, known))).collect::<serde_json::Map<String, Json>>() })
        }
        _ => crate::inputs::literal_json(value).unwrap_or(Json::Null),
    }
}

/// True for an environment name that can change which program runs or loads.
fn is_sensitive_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let inert = upper.starts_with("LC_") || INERT_NAMES.iter().any(|known| upper == *known);
    !inert
}

/// True when a value depends on the program's runtime state.
fn has_reference(value: &Value) -> bool {
    match value {
        Value::Reference(_) => true,
        Value::List(items) => items.iter().any(has_reference),
        Value::Record(fields) => fields.values().any(has_reference),
        Value::String(_) | Value::Integer(_) | Value::Bool(_) | Value::Null | Value::Number(_) => {
            false
        }
    }
}

/// Record a literal path, or mark the destination as runtime-determined.
fn set_path(row: &mut Row, path: &Value) {
    match path {
        Value::String(text) => row.path = Some(text.clone()),
        _ => row.path_is_runtime = true,
    }
}

/// True for a path that does not resolve under the program's folder: a POSIX or
/// Windows-rooted path, a drive path, or a home path (`~`, `~/...`, `~\...`). A
/// home path is never joined to the tracked folder as a relative path.
fn is_absolute(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.starts_with('/')
        || text.starts_with('\\')
        || is_home_path(text)
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// True for `~`, `~/...`, and `~\...`. The path is reported literally, because
/// the expanded location depends on the environment at run time.
fn is_home_path(text: &str) -> bool {
    text == "~" || text.starts_with("~/") || text.starts_with("~\\")
}

/// True when any component of a path is `..`, whichever separator is used.
fn has_parent_component(text: &str) -> bool {
    text.split(['/', '\\']).any(|segment| segment == "..")
}

/// Join a relative path to a base, normalising `.` and `..`. The boolean is true
/// when `..` climbs above the base, so the path leaves the program's folder.
fn join_relative(base: &[String], text: &str) -> (String, bool) {
    let mut parts: Vec<String> = base.to_vec();
    let mut escapes = false;
    for segment in text.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    escapes = true;
                }
            }
            other => parts.push(other.to_owned()),
        }
    }
    (parts.join("/"), escapes)
}

fn is_write_effect(effect: &str) -> bool {
    effect.starts_with("fs.write") || effect.starts_with("file.write") || effect.contains(".write")
}

/// Whether a row is a privileged step: an elevator, or a wrapper that can run
/// another program. An invocation whose binary is unknown is privileged, so an
/// unclassified step cannot pass as harmless.
fn row_privileged(row: &Row) -> bool {
    row.invocation && row.binary.as_deref().is_none_or(is_privileged_invocation)
}

fn summary_object(program_path: &str, rows: &[Row], legacy: bool) -> Json {
    let mut executables = BTreeSet::new();
    let mut write_effects = BTreeSet::new();
    let mut write_paths = Vec::new();
    let mut runtime_write_paths = 0usize;
    let mut escaping_write_paths = 0usize;
    let mut invocations = 0usize;
    let mut undeclared = 0usize;
    let mut privileged_invocations = 0usize;
    let mut tainted = 0usize;
    let mut writes = false;
    let mut network = false;
    let mut reads = false;
    let mut cwd_changes = false;
    let mut modifies_next = false;
    for row in rows {
        if row.invocation {
            invocations += 1;
        }
        if let (true, Some(binary)) = (row.invocation, &row.binary) {
            executables.insert(binary.clone());
        }
        if row_privileged(row) {
            privileged_invocations += 1;
        }
        if row.undeclared {
            undeclared += 1;
        }
        if row.environment_tainted {
            tainted += 1;
        }
        if row.writes == Some(true) {
            writes = true;
        }
        if row.network == Some(true) {
            network = true;
        }
        if row.reads == Some(true) {
            reads = true;
        }
        if row.cwd_change == Some(true) {
            cwd_changes = true;
        }
        if row.modifies_next_invocation {
            modifies_next = true;
        }
        if let Some(effects) = &row.effects {
            for effect in effects.iter().filter(|effect| is_write_effect(effect)) {
                write_effects.insert(effect.clone());
            }
        }
        if row.writes == Some(true) && matches!(row.kind, "save" | "append" | "redirect") {
            if row.path_is_runtime {
                runtime_write_paths += 1;
            } else if let Some(path) = &row.path {
                write_paths.push(path.clone());
            }
            if row.escapes_cwd {
                escaping_write_paths += 1;
            }
        }
    }
    let steps: Vec<Json> = rows.iter().map(|row| row.to_json(legacy)).collect();
    json!({
        "protocol": if legacy { LEGACY_PROTOCOL } else { PROTOCOL },
        "language_version": crate::parser::LANGUAGE_VERSION,
        "program": program_path,
        "steps": steps,
        "summary": {
            "invocations": invocations,
            "executables": executables.into_iter().collect::<Vec<_>>(),
            "writes": writes,
            "write_effects": write_effects.into_iter().collect::<Vec<_>>(),
            "write_paths": write_paths,
            "runtime_write_paths": runtime_write_paths,
            "escaping_write_paths": escaping_write_paths,
            "network": network,
            "reads": reads,
            "cwd_changes": cwd_changes,
            "modifies_next_invocation": modifies_next,
            "environment_tainted_invocations": tainted,
            "undeclared_invocations": undeclared,
            "effects_declared": undeclared == 0,
            "privileged": privileged_invocations > 0,
            "privileged_invocations": privileged_invocations
        }
    })
}
