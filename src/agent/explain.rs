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
    Connection, Invocation, LoopKind, Program, RedirectTarget, ReusableProgram, Step, StepKind,
    Value,
};
use crate::manifest::{Builtin, Registry};
use crate::privilege::is_privileged_invocation;
use anyhow::Result;
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, BTreeSet};

/// Protocol identifier of the `--json` object.
pub const PROTOCOL: &str = "execute.explain/1";

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
    kind: &'static str,
    scope: Option<String>,
    invocation: bool,
    direct: bool,
    applet: Option<String>,
    binary: Option<String>,
    argv: Option<Vec<String>>,
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

    fn to_json(&self) -> Json {
        json!({
            "line": self.line,
            "kind": self.kind,
            "scope": self.scope,
            "direct": self.direct,
            "applet": self.applet,
            "binary": self.binary,
            "argv": self.argv,
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
        })
    }
}

/// Build the explain object for a parsed program. `program_path` is shown as
/// given. The caller validates the program first.
pub fn explain_json(program: &Program, registry: &Registry, program_path: &str) -> Result<Json> {
    let mut walker = Walker {
        registry,
        definitions: &program.definitions,
        rows: Vec::new(),
        stack: Vec::new(),
        cwd: Cwd::Known(Vec::new()),
        environment_tainted_next: false,
        depth: 0,
        cwd_poisoned: false,
        group_tainted: false,
    };
    walker.steps(&program.steps, None);
    Ok(summary_object(program_path, &walker.rows))
}

struct Walker<'a> {
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
}

impl Walker<'_> {
    /// Walk a step list in order. A called definition's body is inlined after its
    /// `CallProgram`, using a relative `end` offset. The definition is walked once
    /// through the call, so the inlined copy is skipped. An invalid offset falls
    /// back to walking the inlined steps, so no step is skipped uncounted.
    fn steps(&mut self, steps: &[Step], scope: Option<&str>) {
        let mut index = 0;
        while index < steps.len() {
            let step = &steps[index];
            let target = match &step.kind {
                StepKind::CallProgram { end, .. } => index
                    .checked_add(*end)
                    .filter(|target| *target > index && *target <= steps.len()),
                _ => None,
            };
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
        if call.direct {
            // A quoted executable bypasses the catalog, so its effects are unknown.
            row.binary = Some(call.operation.clone());
            row.undeclared = true;
            self.rows.push(row);
            return;
        }
        let Ok(manifest) = self.registry.resolve_call(call) else {
            row.applet = Some(call.operation.clone());
            row.resolved = Some(false);
            row.undeclared = true;
            self.rows.push(row);
            return;
        };
        row.resolved = Some(true);
        row.applet = Some(manifest.name_for(call).to_owned());
        row.binary = Some(manifest.binary_for(call).to_owned());
        row.argv = if call.parameters.values().any(has_reference) {
            None
        } else {
            manifest.build_arguments(&call.parameters).ok()
        };
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
    }

    /// A reusable program call. Its steps are walked in place, so their effects
    /// count toward this plan. A cycle or missing definition is undeclared. The
    /// working directory is restored afterwards, as the call's `EndCall` does.
    fn call_program(&mut self, line: usize, call: &Invocation, scope: Option<&str>) {
        let name = call.operation.clone();
        let mut row = Row::plain(line, "call_program", scope);
        row.applet = Some(name.clone());
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

fn summary_object(program_path: &str, rows: &[Row]) -> Json {
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
    let steps: Vec<Json> = rows.iter().map(Row::to_json).collect();
    json!({
        "protocol": PROTOCOL,
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
