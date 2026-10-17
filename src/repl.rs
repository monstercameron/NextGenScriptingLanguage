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
        }
    }

    /// The accumulated program, as `:save` writes it.
    pub fn source(&self) -> &str {
        &self.source
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
            inputs: BTreeMap::new(),
            working_directory: self.working_directory.clone(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 16 * 1024 * 1024,
            command_timeout: None,
            total_timeout: None,
        };
        let result =
            runtime::with_streamed_stdout(|| runtime::run(&program, self.registry, &options));
        drop(guard);
        let fresh = self.observer.finish();
        match result {
            Ok(_) => {
                self.source = candidate;
                self.steps.extend(fresh);
                Ok(BlockOutcome::Ran)
            }
            Err(error) => Ok(BlockOutcome::Failed(format!("{error:#}"))),
        }
    }

    /// Bindings made at the top level of the session: the name, and the first
    /// characters of the value its statement last produced.
    pub fn bindings(&self) -> Result<Vec<(String, String)>> {
        let program = crate::parser::parse(&self.source)?;
        Ok(top_level_bindings(&program)
            .into_iter()
            .map(|(line, name)| {
                let value = self
                    .steps
                    .iter()
                    .rev()
                    .find(|step| step.line == line)
                    .map(|step| preview(&step.result.stdout))
                    .unwrap_or_else(|| "(not run)".into());
                (name, value)
            })
            .collect())
    }

    /// The explain text for the accumulated program.
    pub fn explain(&self) -> Result<String> {
        let program = crate::parser::parse(&self.source)?;
        runtime::explain_with_inputs(&program, self.registry, &BTreeMap::new())
    }
}

fn top_level_bindings(program: &Program) -> Vec<(usize, String)> {
    program
        .steps
        .iter()
        .filter_map(|step| match &step.kind {
            StepKind::Invoke { call, .. } => call.binding.clone().map(|name| (step.line, name)),
            _ => None,
        })
        .collect()
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
