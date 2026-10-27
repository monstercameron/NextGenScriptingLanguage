//! `execute repl`: enter statements a block at a time, keep bindings, and save the
//! session as a program.
//!
//! Each block is appended to the session program and validated with the same
//! static checks as a file. A block that fails validation is reported and dropped,
//! and the session continues. A valid block runs live, and the statements before it
//! are answered from the session's own record of earlier runs. So bindings carry
//! over with no process launched again, and earlier file writes and printed output
//! are not repeated. A block whose run fails is dropped with its partial effects.

use crate::ast::{Program, StepKind};
use crate::diagnostics::{analyze_source, DiagnosticSeverity};
use crate::manifest::Registry;
use crate::runtime::observe::{Observer, StepRequest, StepResult};
use crate::runtime::{self, RunOptions};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

/// Statements that open an indented body. Their block continues until a blank line,
/// since the next line may or may not be indented.
const BLOCK_OPENERS: &[&str] = &[
    "if", "for", "repeat", "while", "every", "on", "define", "try", "parallel", "whenever",
    "listen", "spawn",
];

/// One applet group the session ran, kept to answer the statements before a block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionStep {
    line: usize,
    applets: Vec<String>,
    binaries: Vec<String>,
    argv: Vec<Vec<String>>,
    result: StepResult,
}

#[derive(Default)]
struct ObserverState {
    /// Lines before this one were run by an earlier block.
    boundary: usize,
    earlier: Vec<SessionStep>,
    cursor: usize,
    fresh: Vec<SessionStep>,
}

/// Answers earlier statements from the session's record and captures the new ones.
#[derive(Default)]
struct SessionObserver {
    state: Mutex<ObserverState>,
}

impl SessionObserver {
    fn begin(&self, boundary: usize, earlier: Vec<SessionStep>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = ObserverState {
            boundary,
            earlier,
            cursor: 0,
            fresh: Vec::new(),
        };
    }

    /// The steps the block ran live, once it has finished.
    fn finish(&self) -> Vec<SessionStep> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut state.fresh)
    }
}

impl Observer for SessionObserver {
    fn replay(&self, request: &StepRequest) -> Result<Option<StepResult>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if request.line >= state.boundary {
            return Ok(None);
        }
        let index = state.cursor;
        let Some(step) = state.earlier.get(index).cloned() else {
            bail!(
                "the session has no record of the statement at line {}",
                request.line
            );
        };
        if step.line != request.line
            || step.applets != request.applets
            || step.binaries != request.binaries
            || step.argv != request.argv
        {
            bail!(
                "the session no longer matches its earlier statements at line {}",
                request.line
            );
        }
        state.cursor += 1;
        Ok(Some(step.result.clone()))
    }

    fn finished(
        &self,
        request: &StepRequest,
        _wall: std::time::Duration,
        result: &StepResult,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.fresh.push(SessionStep {
            line: request.line,
            applets: request.applets.clone(),
            binaries: request.binaries.clone(),
            argv: request.argv.clone(),
            result: result.clone(),
        });
        Ok(())
    }

    fn suppresses_writes(&self, line: usize) -> bool {
        line < self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .boundary
    }

    fn suppresses_terminal_output(&self, line: usize) -> bool {
        line < self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .boundary
    }
}

/// An interactive session over one registry and working directory.
pub struct Session<'a> {
    registry: &'a Registry,
    working_directory: std::path::PathBuf,
    source: String,
    steps: Vec<SessionStep>,
    observer: Arc<SessionObserver>,
    history: Vec<(String, Vec<SessionStep>, BTreeMap<String, &'static str>)>,
    machine_protocol: bool,
    binding_types: BTreeMap<String, &'static str>,
}

/// What one submitted block did.
#[derive(Debug, PartialEq, Eq)]
pub enum BlockOutcome {
    Ran,
    Rejected(Vec<String>),
    Failed(String),
}

impl<'a> Session<'a> {
    pub fn new(registry: &'a Registry, working_directory: std::path::PathBuf) -> Self {
        Self {
            registry,
            working_directory,
            source: String::new(),
            steps: Vec::new(),
            observer: Arc::new(SessionObserver::default()),
            history: Vec::new(),
            machine_protocol: false,
            binding_types: BTreeMap::new(),
        }
    }

    /// The accumulated program, as `:save` writes it.
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn set_machine_protocol(&mut self, enabled: bool) {
        self.machine_protocol = enabled;
    }

    /// Validate a block against the accumulated program, then run it.
    pub fn submit(&mut self, block: &str) -> Result<BlockOutcome> {
        let block = block.trim_end_matches('\n');
        if block.trim().is_empty() {
            return Ok(BlockOutcome::Ran);
        }
        let boundary = self.source.lines().count() + 1;
        let candidate = format!("{}{}\n", self.source, block);
        let errors = analyze_source(&candidate, self.registry)
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
            .map(|diagnostic| format!("line {}: {}", diagnostic.span.line, diagnostic.message))
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            return Ok(BlockOutcome::Rejected(errors));
        }
        let program = crate::parser::parse(&candidate).map_err(|error| anyhow!("{error:#}"))?;
        if let Some(reason) = crate::replay::unsupported_reason(&program) {
            return Ok(BlockOutcome::Rejected(vec![format!(
                "the REPL does not run {reason}"
            )]));
        }
        let earlier = self.steps.clone();
        self.observer.begin(boundary, earlier);
        let guard = runtime::observe::install(self.observer.clone())?;
        let options = RunOptions {
            max_output_bytes: 16 * 1024 * 1024,
            ..RunOptions::new(self.working_directory.clone())
        };
        let result = if self.machine_protocol {
            runtime::run(&program, self.registry, &options)
        } else {
            runtime::with_streamed_stdout(|| runtime::run(&program, self.registry, &options))
        };
        drop(guard);
        let fresh = self.observer.finish();
        match result {
            Ok(run_result) => {
                self.history.push((self.source.clone(), self.steps.clone(), self.binding_types.clone()));
                self.binding_types = run_result.binding_types;
                self.source = candidate;
                self.steps.extend(fresh);
                Ok(BlockOutcome::Ran)
            }
            Err(error) => Ok(BlockOutcome::Failed(format!("{error:#}"))),
        }
    }

    /// Remove previously accepted blocks. The REPL replays the remaining source
    /// from its recorded results, so undo never re-executes an earlier effect.
    pub fn undo(&mut self, blocks: usize) -> Result<()> {
        if blocks == 0 || blocks > self.history.len() {
            bail!("undo count must be between 1 and {}", self.history.len());
        }
        for _ in 0..blocks {
            let (source, steps, binding_types) = self.history.pop().expect("validated history length");
            self.source = source;
            self.steps = steps;
            self.binding_types = binding_types;
        }
        Ok(())
    }

    fn validate(&self, block: &str) -> Result<(String, Vec<crate::diagnostics::Diagnostic>)> {
        let block = block.trim_end_matches('\n');
        let candidate = format!("{}{}\n", self.source, block);
        let diagnostics = analyze_source(&candidate, self.registry).into_iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error).collect();
        Ok((candidate, diagnostics))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn what_if(&mut self, block: &str) -> Result<serde_json::Value> {
        let (candidate, diagnostics) = self.validate(block)?;
        if !diagnostics.is_empty() {
            return redact_json(self.registry, serde_json::json!({"status":"rejected", "protocol":"execute.check/1", "diagnostics":diagnostics_json(self.source.len(), block, diagnostics)}));
        }
        let program = crate::parser::parse(&candidate)?;
        if let Some(reason) = crate::replay::unsupported_reason(&program) {
            return redact_json(self.registry, serde_json::json!({"status":"rejected", "protocol":"execute.check/1", "diagnostics":[{"severity":"error","code":"repl.unsupported","message":format!("the REPL does not run {reason}")}]}));
        }
        let plan_path = self.working_directory.to_string_lossy();
        let plan = redact_json(
            self.registry,
            crate::agent::explain::explain_json(&program, self.registry, &plan_path)?,
        )?;
        self.history.push((self.source.clone(), self.steps.clone(), self.binding_types.clone()));
        self.source = candidate;
        Ok(serde_json::json!({"status":"planned", "plan":plan}))
    }

    /// Bindings made at the top level of the session: the name, and the first
    /// characters of the value its statement last produced.
    pub fn bindings(&self) -> Result<Vec<(String, String)>> {
        let program = crate::parser::parse(&self.source)?;
        Ok(top_level_bindings(&program)
            .into_iter()
            .map(|(line, name, kind)| {
                let value = self.steps.iter().rev().find(|step| step.line == line)
                    .map(|step| match kind {
                        BindingPreview::Output => preview(&step.result.stdout),
                        BindingPreview::Status => step.result.status.map(|status| status.to_string()).unwrap_or_else(|| "(no status)".into()),
                        BindingPreview::Result => "(execution result)".into(),
                    })
                    .unwrap_or_else(|| "(not run)".into());
                (name, value)
            })
            .collect())
    }

    /// Types observed in the most recent runtime result, with no binding values
    /// retained by the session protocol.
    pub fn binding_types(&self) -> &BTreeMap<String, &'static str> {
        &self.binding_types
    }

    /// The explain text for the accumulated program.
    pub fn explain(&self) -> Result<String> {
        let program = crate::parser::parse(&self.source)?;
        runtime::explain_with_inputs(&program, self.registry, &BTreeMap::new())
    }
}

#[derive(Clone, Copy)]
enum BindingPreview { Output, Status, Result }

fn top_level_bindings(program: &Program) -> Vec<(usize, String, BindingPreview)> {
    program
        .steps
        .iter()
        .flat_map(|step| match &step.kind {
            StepKind::Invoke { call, .. } => {
                let mut found = Vec::new();
                if let Some(name) = &call.binding { found.push((step.line, name.clone(), BindingPreview::Output)); }
                if let Some(name) = &call.status_binding { found.push((step.line, name.clone(), BindingPreview::Status)); }
                if let Some(name) = &call.result_binding { found.push((step.line, name.clone(), BindingPreview::Result)); }
                found
            }
            _ => Vec::new(),
        })
        .collect()
}

fn diagnostics_json(
    block_start: usize,
    block: &str,
    diagnostics: Vec<crate::diagnostics::Diagnostic>,
) -> Vec<serde_json::Value> {
    let block = block.trim_end_matches('\n');
    diagnostics.into_iter().map(|diagnostic| {
        let start = diagnostic.span.start_byte.saturating_sub(block_start).min(block.len());
        let end = diagnostic.span.end_byte.saturating_sub(block_start).min(block.len());
        // Analyzer byte offsets are UTF-8 boundaries. Clamp defensively before
        // slicing in case a diagnostic points into the prior session portion.
        let prefix = &block[..start];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        serde_json::json!({
            "severity": diagnostic.severity,
            "code": diagnostic.code,
            "message": diagnostic.message,
            "span": {
                "session": diagnostic.span,
                "block": {"start_byte":start,"end_byte":end,"line":line,"column":column}
            }
        })
    }).collect()
}

fn redact_json(registry: &Registry, mut value: serde_json::Value) -> Result<serde_json::Value> {
    fn visit(value: &mut serde_json::Value, redactor: &crate::security::SecretRedactor) {
        match value {
            serde_json::Value::String(text) => *text = redactor.redact_text(text),
            serde_json::Value::Array(items) => {
                for item in items { visit(item, redactor); }
            }
            serde_json::Value::Object(fields) => {
                for item in fields.values_mut() { visit(item, redactor); }
            }
            _ => {}
        }
    }
    let redactor = crate::security::secret_redactor(registry);
    visit(&mut value, &redactor);
    Ok(value)
}

fn preview(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_end_matches('\n');
    let mut shown: String = trimmed.chars().take(60).collect();
    if trimmed.chars().count() > 60 {
        shown.push_str("...");
    }
    format!("{shown:?}")
}

/// Whether a line opens an indented block that continues until a blank line.
pub fn opens_block(line: &str) -> bool {
    line.split_whitespace()
        .next()
        .is_some_and(|word| BLOCK_OPENERS.contains(&word))
}

/// Read statements from `input` and run them until `:quit` or end of input. Prompts
/// are written only when `interactive` is set. Returns the number of blocks run.
pub fn run(
    session: &mut Session<'_>,
    input: impl BufRead,
    mut output: impl Write,
    interactive: bool,
) -> Result<usize> {
    let mut block: Option<Vec<String>> = None;
    let mut ran = 0;
    let mut lines = input.lines();
    loop {
        if interactive {
            write!(
                output,
                "{}",
                if block.is_some() {
                    "...   "
                } else {
                    "execute> "
                }
            )?;
            output.flush()?;
        }
        let Some(line) = lines.next() else {
            if let Some(pending) = block.take() {
                ran += submit_and_report(session, &pending, &mut output)?;
            }
            break;
        };
        let line = line.context("reading the session input")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(pending) = block.take() {
                ran += submit_and_report(session, &pending, &mut output)?;
            }
            continue;
        }
        if trimmed.starts_with(':') && block.is_none() {
            if command(session, trimmed, &mut output)? {
                break;
            }
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        match block.as_mut() {
            Some(pending) if indented => pending.push(line),
            Some(_) => {
                let pending = block.take().expect("checked above");
                ran += submit_and_report(session, &pending, &mut output)?;
                if opens_block(&line) {
                    block = Some(vec![line]);
                } else {
                    ran += submit_and_report(session, &[line], &mut output)?;
                }
            }
            None => {
                if opens_block(&line) {
                    block = Some(vec![line]);
                } else {
                    ran += submit_and_report(session, &[line], &mut output)?;
                }
            }
        }
    }
    Ok(ran)
}

/// Process the machine protocol: exactly one JSON request and one JSON response
/// per input line. Malformed requests produce an error response and do not end the
/// session.
#[cfg(not(target_arch = "wasm32"))]
pub fn run_json(
    session: &mut Session<'_>,
    mut input: impl BufRead,
    mut output: impl Write,
    what_if: bool,
) -> Result<()> {
    const MAX_REQUEST_BYTES: usize = 1024 * 1024;
    session.set_machine_protocol(true);
    loop {
        let Some((line, oversized)) = read_json_line(&mut input, MAX_REQUEST_BYTES)? else { break; };
        let response = if oversized {
            serde_json::json!({"status":"error","error":"request line exceeds the 1048576-byte limit"})
        } else { (|| -> Result<serde_json::Value> {
            let request: serde_json::Value = serde_json::from_slice(&line).context("request must be JSON")?;
            if !request.is_object() { bail!("request must be a JSON object"); }
            if let Some(block) = request.get("submit").and_then(serde_json::Value::as_str) {
                if what_if { return session.what_if(block); }
                let (_, checked) = session.validate(block)?;
                if !checked.is_empty() {
                    let diagnostics = diagnostics_json(session.source.len(), block, checked);
                    return redact_json(session.registry, serde_json::json!({"status":"rejected","protocol":"execute.check/1","diagnostics":diagnostics}));
                }
                let before = session.steps.len();
                let outcome = session.submit(block)?;
                match outcome {
                    BlockOutcome::Rejected(messages) => {
                        let redactor = crate::security::secret_redactor(session.registry);
                        let diagnostics = messages.into_iter().map(|message| serde_json::json!({"severity":"error","code":"check.rejected","message":redactor.redact_text(&message)})).collect::<Vec<_>>();
                        Ok(serde_json::json!({"status":"rejected","protocol":"execute.check/1","diagnostics":diagnostics}))
                    }
                    BlockOutcome::Failed(error) => {
                        let redactor = crate::security::secret_redactor(session.registry);
                        Ok(serde_json::json!({"status":"failed","error":redactor.redact_text(&error)}))
                    }
                    BlockOutcome::Ran => {
                        let fresh = &session.steps[before..];
                        let mut stdout = Vec::new(); let mut stderr = Vec::new(); let mut exit = 0;
                        for step in fresh { stdout.extend_from_slice(&step.result.stdout); stderr.extend_from_slice(&step.result.stderr); if let Some(code) = step.result.status { exit = code; } }
                        let redactor = crate::security::secret_redactor(session.registry);
                        let stdout = String::from_utf8_lossy(&redactor.redact_bytes(&stdout)).into_owned();
                        let stderr = String::from_utf8_lossy(&redactor.redact_bytes(&stderr)).into_owned();
                        let types = session.binding_types().clone();
                        let bindings = session.bindings()?.into_iter().map(|(name, preview)| { let kind = types.get(&name).copied().unwrap_or("unknown"); (name, serde_json::json!({"type":kind,"preview":redactor.redact_text(&preview)})) }).collect::<serde_json::Map<_,_>>();
                        let stderr_tail = stderr.chars().rev().take(4096).collect::<String>().chars().rev().collect::<String>();
                        Ok(serde_json::json!({"status":"ran","stdout":stdout,"stderr_tail":stderr_tail,"exit":exit,"bindings":bindings}))
                    }
                }
            } else if request.get("explain").and_then(serde_json::Value::as_bool)==Some(true) {
                let explain = crate::security::secret_redactor(session.registry)
                    .redact_text(&session.explain()?);
                Ok(serde_json::json!({"status":"ok","explain":explain}))
            } else if request.get("bindings").and_then(serde_json::Value::as_bool)==Some(true) {
                let redactor = crate::security::secret_redactor(session.registry);
                let types = session.binding_types().clone();
                let bindings = session.bindings()?.into_iter().map(|(name, preview)| { let kind = types.get(&name).copied().unwrap_or("unknown"); (name,serde_json::json!({"type":kind,"preview":redactor.redact_text(&preview)})) }).collect::<serde_json::Map<_,_>>();
                Ok(serde_json::json!({"status":"ok","bindings":bindings}))
            } else if request.get("source").and_then(serde_json::Value::as_bool)==Some(true) {
                Ok(serde_json::json!({"status":"ok","source":session.source()}))
            } else if let Some(path) = request.get("save").and_then(serde_json::Value::as_str) {
                save_source(Path::new(path), session.source())?;
                Ok(serde_json::json!({"status":"saved","path":path}))
            } else if let Some(count) = request.get("undo").and_then(serde_json::Value::as_u64) {
                session.undo(usize::try_from(count).context("undo count is too large")?)?;
                Ok(serde_json::json!({"status":"undone","source":session.source()}))
            } else { bail!("request must contain submit, explain, bindings, source, save, or undo"); }
        })().unwrap_or_else(|error| serde_json::json!({"status":"error","error":format!("{error:#}")})) };
        serde_json::to_writer(&mut output, &response)?;
        writeln!(output)?;
        output.flush()?;
    }
    Ok(())
}

/// Read one request without allowing an unterminated line to grow without bound.
/// Once over limit, discard through the newline and return one error response.
fn read_json_line(input: &mut impl BufRead, limit: usize) -> Result<Option<(Vec<u8>, bool)>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    let mut read_any = false;
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return Ok(read_any.then_some((bytes, oversized)));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        read_any = true;
        if !oversized && bytes.len().saturating_add(count) <= limit {
            bytes.extend_from_slice(&available[..count]);
        } else {
            oversized = true;
        }
        input.consume(count);
        if newline.is_some() {
            return Ok(Some((bytes, oversized)));
        }
    }
}

fn submit_and_report(
    session: &mut Session<'_>,
    lines: &[String],
    output: &mut impl Write,
) -> Result<usize> {
    let block = lines.join("\n");
    let outcome = session.submit(&block)?;
    // Streamed output is written to the terminal directly, so flush what this
    // session has already written before the next block can print.
    output.flush()?;
    match outcome {
        BlockOutcome::Ran => Ok(1),
        BlockOutcome::Rejected(errors) => {
            for error in errors {
                writeln!(output, "rejected: {error}")?;
            }
            output.flush()?;
            Ok(0)
        }
        BlockOutcome::Failed(error) => {
            writeln!(output, "error: {error}")?;
            output.flush()?;
            Ok(0)
        }
    }
}

/// Handle one `:command`. Returns true when the session should end.
fn command(session: &mut Session<'_>, line: &str, output: &mut impl Write) -> Result<bool> {
    let mut parts = line.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let argument = parts.next().map(str::trim).unwrap_or_default();
    match name {
        ":quit" | ":q" => return Ok(true),
        ":help" => writeln!(
            output,
            ":explain  show the plan of the session program\n:bindings show bindings and their last values\n:save FILE write the session program to a new file\n:quit     end the session"
        )?,
        ":explain" => match session.explain() {
            Ok(text) => writeln!(output, "{text}")?,
            Err(error) => writeln!(output, "error: {error:#}")?,
        },
        ":bindings" => {
            let bindings = session.bindings()?;
            if bindings.is_empty() {
                writeln!(output, "no bindings")?;
            }
            for (name, value) in bindings {
                writeln!(output, "{name} = {value}")?;
            }
        }
        ":save" => {
            if argument.is_empty() {
                writeln!(output, "usage: :save FILE")?;
            } else {
                match save_source(Path::new(argument), session.source()) {
                    Ok(()) => writeln!(output, "saved {argument}")?,
                    Err(error) => writeln!(output, "error: {error:#}")?,
                }
            }
        }
        other => writeln!(output, "unknown command {other:?}; try :help")?,
    }
    Ok(false)
}

/// Write the session program to a new file. An existing file is never replaced.
fn save_source(path: &Path, source: &str) -> Result<()> {
    if source.is_empty() {
        bail!("the session has no statements to save");
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {} (it must not exist)", path.display()))?;
    file.write_all(source.as_bytes())?;
    Ok(())
}
