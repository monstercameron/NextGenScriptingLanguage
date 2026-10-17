//! `execute explain FILE --json`: a per-step plan of native effects.
//!
//! The plan is derived from the parsed program and the resolved manifests. It
//! does not run anything. The walk matches every `StepKind` explicitly, with no
//! catch-all arm, so a new step kind fails to compile until it is classified
//! here. Called reusable programs are walked recursively, and a cycle or a
//! missing definition is reported as undeclared.
//!
//! Effects come only from applet contracts that declare them. A step whose
//! effects are unknown reports `effects: null` and counts as undeclared. The
//! summary's `effects_declared` is true only when no row is undeclared.

use crate::ast::{
    Invocation, LoopKind, Program, RedirectTarget, ReusableProgram, Step, StepKind, Value,
};
use crate::manifest::Registry;
use anyhow::Result;
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, BTreeSet};

/// Protocol identifier of the `--json` object.
pub const PROTOCOL: &str = "execute.explain/1";

/// One planned step or invocation. `undeclared` marks any row whose effects
/// cannot be stated, so a safety decision must not treat it as harmless.
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
    path: Option<String>,
    path_is_runtime: bool,
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
            "path": self.path,
            "path_is_runtime": self.path_is_runtime,
            "resolved": self.resolved,
            "undeclared": self.undeclared,
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
}

impl Walker<'_> {
    /// Walk a step list. The parser inlines a called definition's body between
    /// `CallProgram` and its `EndCall`, and the definition is walked once through
    /// the call. Skipping the inlined copy avoids double counting. If `end` is not
    /// a valid forward index, the inlined steps are walked instead, so no step is
    /// skipped without being counted.
    fn steps(&mut self, steps: &[Step], scope: Option<&str>) {
        let mut index = 0;
        while index < steps.len() {
            let step = &steps[index];
            match &step.kind {
                StepKind::CallProgram { call, end, .. } if *end > index && *end <= steps.len() => {
                    self.call_program(step.line, call, scope);
                    index = *end;
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
        match kind {
            StepKind::TaskGroup { tasks, .. } => {
                for call in tasks {
                    self.invocation(line, "task_group", call, scope);
                }
            }
            StepKind::SpawnJob { stages, .. } => {
                for stage in stages {
                    self.invocation(line, "spawn_job", &stage.call, scope);
                }
            }
            StepKind::InspectJob { .. } => self.push_plain(line, "inspect_job", scope),
            StepKind::AwaitJob { .. } => self.push_plain(line, "await_job", scope),
            StepKind::CancelJob { .. } => self.push_plain(line, "cancel_job", scope),
            StepKind::Loop { kind, .. } => match kind {
                LoopKind::WhileCommand { call, .. } => {
                    self.invocation(line, "while_command", call, scope);
                }
                LoopKind::Repeat { .. } => self.push_plain(line, "loop_repeat", scope),
                LoopKind::ForEach { .. } => self.push_plain(line, "loop_for_each", scope),
                LoopKind::While { .. } => self.push_plain(line, "loop_while", scope),
            },
            StepKind::EndLoop { .. } => self.push_plain(line, "end_loop", scope),
            StepKind::If { .. } => self.push_plain(line, "if", scope),
            StepKind::IfCommand { call, .. } => {
                self.invocation(line, "if_command", call, scope);
            }
            StepKind::Otherwise { .. } => self.push_plain(line, "otherwise", scope),
            StepKind::EndIf => self.push_plain(line, "end_if", scope),
            StepKind::CallProgram { call, .. } => self.call_program(line, call, scope),
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
                set_path(&mut row, path);
                self.rows.push(row);
            }
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
            StepKind::Invoke { call, .. } => self.invocation(line, "invoke", call, scope),
            StepKind::Save { path, .. } => self.file_write(line, "save", Some(path), scope),
            StepKind::Append { path } => self.file_write(line, "append", Some(path), scope),
            StepKind::Environment { .. } => {
                // Changes the environment of the next native command.
                let mut row = Row::plain(line, "environment", scope);
                row.modifies_next_invocation = true;
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

    /// A step that writes a file. A path that is a runtime value is recorded as
    /// such, so the plan never guesses the destination.
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
            set_path(&mut row, path);
        }
        self.rows.push(row);
    }

    fn invocation(
        &mut self,
        line: usize,
        kind: &'static str,
        call: &Invocation,
        scope: Option<&str>,
    ) {
        let mut row = Row {
            line,
            kind,
            scope: scope.map(str::to_owned),
            invocation: true,
            direct: call.direct,
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
        if let Some(contract) = manifest.execution_contract() {
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
        } else {
            row.undeclared = true;
        }
        self.rows.push(row);
    }

    /// A reusable program call. Its steps are walked in place, so their effects
    /// count toward this plan. A cycle or missing definition is undeclared.
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
        self.stack.push(name.clone());
        self.steps(&definition.steps, Some(&name));
        self.stack.pop();
    }
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

fn is_write_effect(effect: &str) -> bool {
    effect.starts_with("fs.write") || effect.starts_with("file.write") || effect.contains(".write")
}

fn summary_object(program_path: &str, rows: &[Row]) -> Json {
    let mut executables = BTreeSet::new();
    let mut write_effects = BTreeSet::new();
    let mut write_paths = Vec::new();
    let mut runtime_write_paths = 0usize;
    let mut invocations = 0usize;
    let mut undeclared = 0usize;
    let mut writes = false;
    let mut network = false;
    let mut reads = false;
    let mut cwd_changes = false;
    let mut modifies_next = false;
    for row in rows {
        if row.invocation {
            invocations += 1;
        }
        if let Some(binary) = &row.binary {
            if row.invocation {
                executables.insert(binary.clone());
            }
        }
        if row.undeclared {
            undeclared += 1;
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
            match &row.path {
                Some(path) => write_paths.push(path.clone()),
                None => runtime_write_paths += 1,
            }
        }
    }
    let steps: Vec<Json> = rows.iter().map(Row::to_json).collect();
    json!({
        "protocol": PROTOCOL,
        "program": program_path,
        "steps": steps,
        "summary": {
            "invocations": invocations,
            "executables": executables.into_iter().collect::<Vec<_>>(),
            "writes": writes,
            "write_effects": write_effects.into_iter().collect::<Vec<_>>(),
            "write_paths": write_paths,
            "runtime_write_paths": runtime_write_paths,
            "network": network,
            "reads": reads,
            "cwd_changes": cwd_changes,
            "modifies_next_invocation": modifies_next,
            "undeclared_invocations": undeclared,
            "effects_declared": undeclared == 0,
            "privileged": "not_classified_see_policy_marking"
        }
    })
}
