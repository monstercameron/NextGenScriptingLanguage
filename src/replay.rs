//! Side-effect-free replay of a run record.
//!
//! `Replayer` answers each applet group with the stdout, stderr and status that the
//! record stored for it, so no process is launched and no file is written. The
//! program's own control flow runs for real, which means the replay is only as
//! faithful as the match between the program and the record. The first group whose
//! line, applets, binaries or argv differ from the record stops the replay and is
//! reported as a divergence.

use crate::ast::Program;
use crate::manifest::Registry;
use crate::record::{StoredRecord, StoredStep};
use crate::runtime::observe::{Observer, StepRequest, StepResult};
use crate::security::{self, SecretRedactor};
use crate::terminal_text::escape_controls;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value as Json;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

/// The first point where the replayed program stopped matching the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Zero-based index of the recorded step that was expected.
    pub step: usize,
    /// Source line of the group the program tried to run.
    pub line: usize,
    pub expected: String,
    pub actual: String,
}

/// How a replay ended, apart from its program output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The program finished and every recorded step was replayed.
    Completed,
    /// The first group past `--until` was reached.
    UntilLine { line: usize, steps_replayed: usize },
    /// The program diverged from the record.
    Diverged(Divergence),
    /// The program finished with recorded steps still unused.
    UnusedRecordedSteps { next: usize, line: usize },
}

/// Replays one record. Install it with `runtime::observe::install` before a run.
pub struct Replayer {
    record: StoredRecord,
    redactor: Arc<SecretRedactor>,
    until: Option<usize>,
    state: Mutex<ReplayState>,
}

#[derive(Default)]
struct ReplayState {
    cursor: usize,
    stop: Option<Stop>,
}

impl Replayer {
    /// Open a record for replay under the given registry. Declared secrets present
    /// in the environment are registered first, so recorded redactions match.
    pub fn new(record: StoredRecord, registry: &Registry, until: Option<usize>) -> Result<Self> {
        let environment = std::env::vars_os().collect::<Vec<_>>();
        security::register_declared_environment_secrets(registry, &environment)
            .map_err(|error| anyhow!("registering declared secrets: {error}"))?;
        Ok(Self {
            record,
            redactor: security::secret_redactor(registry),
            until,
            state: Mutex::new(ReplayState::default()),
        })
    }

    /// Whether the replay has consumed every recorded step and stopped cleanly.
    pub fn stop(&self) -> Stop {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stop.clone().unwrap_or(Stop::Completed)
    }

    /// Recorded steps the replay consumed so far.
    pub fn steps_replayed(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cursor
    }

    /// Called after the run returns. A run that finished early leaves recorded
    /// steps unused, which is a divergence even though the run succeeded.
    pub fn finish_check(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stop.is_none() {
            if let Some(next) = self.record.steps.get(state.cursor) {
                state.stop = Some(Stop::UnusedRecordedSteps {
                    next: state.cursor,
                    line: next.line,
                });
            }
        }
    }

    pub fn record(&self) -> &StoredRecord {
        &self.record
    }

    fn redacted_argv(&self, request: &StepRequest) -> Vec<Vec<String>> {
        request
            .argv
            .iter()
            .map(|argv| {
                argv.iter()
                    .map(|argument| self.redactor.redact_text(argument))
                    .collect()
            })
            .collect()
    }
}

impl Observer for Replayer {
    fn replay(&self, request: &StepRequest) -> Result<Option<StepResult>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(limit) = self.until {
            if request.line > limit {
                state.stop = Some(Stop::UntilLine {
                    line: request.line,
                    steps_replayed: state.cursor,
                });
                bail!("replay stopped before line {}", request.line);
            }
        }
        let actual = self.redacted_argv(request);
        let index = state.cursor;
        let Some(recorded) = self.record.steps.get(index) else {
            state.stop = Some(Stop::Diverged(Divergence {
                step: index,
                line: request.line,
                expected: "no further recorded step".into(),
                actual: describe_request(request, &actual),
            }));
            bail!("replay diverged at step {index}: the record has no step for this group");
        };
        if !same_group(recorded, request, &actual) {
            let divergence = Divergence {
                step: index,
                line: request.line,
                expected: describe_recorded(recorded),
                actual: describe_request(request, &actual),
            };
            state.stop = Some(Stop::Diverged(divergence.clone()));
            bail!(
                "replay diverged at step {index}: expected {}, got {}",
                divergence.expected,
                divergence.actual
            );
        }
        let stdout = self
            .record
            .blob(&recorded.stdout)
            .map_err(|error| anyhow!("record incomplete at step {index}: {error:#}"))?;
        let stderr = self
            .record
            .blob(&recorded.stderr)
            .map_err(|error| anyhow!("record incomplete at step {index}: {error:#}"))?;
        state.cursor += 1;
        Ok(Some(StepResult {
            status: recorded.status,
            stdout,
            stderr,
            outcome: stored_outcome(recorded),
            failed: recorded.failed,
            // Replay runs no group and persists nothing, so there are no live labels.
            sensitivity: security::SensitivitySet::empty(),
            secret_environment: false,
        }))
    }

    fn finished(
        &self,
        _request: &StepRequest,
        _wall: std::time::Duration,
        _result: &StepResult,
    ) -> Result<()> {
        Ok(())
    }

    fn suppresses_writes(&self, _line: usize) -> bool {
        true
    }
}

/// The declared outcome stored with a step, rebuilt for the runtime.
fn stored_outcome(step: &StoredStep) -> Option<crate::contracts::Outcome> {
    let outcome = step.outcome.as_ref()?;
    Some(crate::contracts::Outcome {
        status: step.status.unwrap_or(0),
        name: outcome["name"].as_str()?.to_owned(),
        kind: serde_json::from_value(outcome["kind"].clone()).ok()?,
        effect_certainty: serde_json::from_value(outcome["certainty"].clone()).ok()?,
    })
}

fn same_group(recorded: &StoredStep, request: &StepRequest, actual_argv: &[Vec<String>]) -> bool {
    recorded.line == request.line
        && recorded.applets == request.applets
        && recorded.binaries == request.binaries
        && recorded.argv == actual_argv
}

fn describe_recorded(step: &StoredStep) -> String {
    describe(step.line, &step.applets, &step.argv)
}

fn describe_request(request: &StepRequest, argv: &[Vec<String>]) -> String {
    describe(request.line, &request.applets, argv)
}

fn describe(line: usize, applets: &[String], argv: &[Vec<String>]) -> String {
    let argv = argv
        .iter()
        .map(|argv| format!("{argv:?}"))
        .collect::<Vec<_>>()
        .join(" | ");
    escape_controls(&format!(
        "line {line} {} with argv {argv}",
        applets.join(" | ")
    ))
}

/// Why a program cannot be replayed faithfully, if it cannot. Triggered programs,
/// background jobs, detached launches, handoffs and cached steps either run outside
/// the observed path or touch files a replay must not write, so they are refused up front.
pub fn unsupported_reason(program: &Program) -> Option<&'static str> {
    if program.trigger.is_some() {
        return Some("triggered programs (listeners, schedules and intervals)");
    }
    // The debug form names every step and call field, so it finds nested steps
    // without a walk over every statement kind. It can over-match a string that
    // spells the same text, which only makes the check refuse more.
    let debug = format!("{:?}", program.steps);
    if debug.contains("SpawnJob") {
        return Some("background jobs (spawn)");
    }
    if debug.contains("LaunchDetached") {
        return Some("detached launches (launch detached)");
    }
    if debug.contains("HandOver") {
        return Some("handovers (hand over to)");
    }
    if debug.contains("Interactive") {
        return Some("interactive steps");
    }
    if debug.contains("cache: true") {
        return Some("cached steps (and cache)");
    }
    None
}

/// The record's program copy, for replay without an explicit `--program`.
pub fn stored_program_path(record: &Path) -> std::path::PathBuf {
    record.join(crate::record::PROGRAM_FILE)
}

/// Compare an edited program's declared name with the record's program hash.
pub fn program_matches_record(record: &StoredRecord, source: &[u8]) -> bool {
    record.run["program"]["sha256"].as_str() == Some(crate::record::sha256_hex(source).as_str())
}

/// Resume a failed recorded run: replay the completed prefix from the record,
/// then run the repaired suffix natively.
///
/// The completed prefix is every recorded step before the record's first failed
/// step (or every recorded step when nothing failed, as with an interrupted
/// run). Each group is answered with the stdout, stderr and status the record
/// stored for it, so no process is launched and no file is written for those
/// steps — exactly like [`Replayer`]. Once the prefix is consumed, or when the
/// program was repaired, later groups run for real: the first group past the
/// prefix falls through to native execution.
///
/// A group inside the prefix that no longer matches the record is a refusal,
/// not a re-run: silently running it would repeat a completed effect the resume
/// promised to skip. A group whose recorded bytes are missing, withheld, or
/// truncated is refused with its step index and reason.
pub struct PrefixReplayer {
    record: StoredRecord,
    redactor: Arc<SecretRedactor>,
    /// Lines below this ran before the failure; their writes and terminal
    /// output are suppressed while they replay.
    boundary: usize,
    /// Recorded steps eligible for replay: up to the first failed step.
    prefix: usize,
    state: Mutex<PrefixState>,
}

#[derive(Default)]
struct PrefixState {
    cursor: usize,
    resumed: bool,
}

impl PrefixReplayer {
    /// Replay `record`'s completed prefix below `boundary`. Declared secrets
    /// present in the environment are registered first, so recorded redactions
    /// match.
    pub fn new(
        record: StoredRecord,
        registry: &Registry,
        boundary: usize,
    ) -> Result<Self> {
        let environment = std::env::vars_os().collect::<Vec<_>>();
        security::register_declared_environment_secrets(registry, &environment)
            .map_err(|error| anyhow!("registering declared secrets: {error}"))?;
        let prefix = record
            .steps
            .iter()
            .position(|step| crate::record::step_failed(step))
            .unwrap_or(record.steps.len());
        Ok(Self {
            record,
            redactor: security::secret_redactor(registry),
            boundary,
            prefix,
            state: Mutex::new(PrefixState::default()),
        })
    }

    /// Recorded steps replayed so far, without launching a process.
    pub fn steps_replayed(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cursor
    }

    /// How many leading recorded steps may replay before native execution.
    pub fn prefix(&self) -> usize {
        self.prefix
    }

    /// Lines below this are the completed prefix.
    pub fn boundary(&self) -> usize {
        self.boundary
    }

    /// Whether execution has fallen through to the repaired suffix.
    pub fn fell_through(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resumed
    }

    pub fn record(&self) -> &StoredRecord {
        &self.record
    }

    fn redacted_argv(&self, request: &StepRequest) -> Vec<Vec<String>> {
        request
            .argv
            .iter()
            .map(|argv| {
                argv.iter()
                    .map(|argument| self.redactor.redact_text(argument))
                    .collect()
            })
            .collect()
    }
}

impl Observer for PrefixReplayer {
    fn replay(&self, request: &StepRequest) -> Result<Option<StepResult>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.resumed || state.cursor >= self.prefix {
            state.resumed = true;
            return Ok(None);
        }
        let actual = self.redacted_argv(request);
        let index = state.cursor;
        let Some(recorded) = self.record.steps.get(index) else {
            bail!("resume reached step {index} past the end of the record; the completed prefix cannot be replayed");
        };
        if !same_group(recorded, request, &actual) {
            let divergence = Divergence {
                step: index,
                line: request.line,
                expected: describe_recorded(recorded),
                actual: describe_request(request, &actual),
            };
            bail!(
                "resume diverged at step {index}: expected {}, got {}; the completed prefix no longer matches, so it cannot be skipped",
                divergence.expected,
                divergence.actual
            );
        }
        let stdout = self
            .record
            .blob(&recorded.stdout)
            .map_err(|error| anyhow!("record incomplete at step {index}: {error:#}"))?;
        let stderr = self
            .record
            .blob(&recorded.stderr)
            .map_err(|error| anyhow!("record incomplete at step {index}: {error:#}"))?;
        state.cursor += 1;
        Ok(Some(StepResult {
            status: recorded.status,
            stdout,
            stderr,
            outcome: stored_outcome(recorded),
            failed: recorded.failed,
            // Replay runs no group and persists nothing, so there are no live labels.
            sensitivity: security::SensitivitySet::empty(),
            secret_environment: false,
        }))
    }

    fn finished(
        &self,
        _request: &StepRequest,
        _wall: std::time::Duration,
        _result: &StepResult,
    ) -> Result<()> {
        Ok(())
    }

    fn suppresses_writes(&self, line: usize) -> bool {
        line < self.boundary
    }

    fn suppresses_terminal_output(&self, line: usize) -> bool {
        line < self.boundary
    }
}

/// Recorded inputs for a resume, refusing a secret answer the record withheld.
/// A withheld answer cannot be reproduced, so resuming with a marker in its
/// place would run a different program than the recorded one.
pub fn recorded_inputs(
    record: &StoredRecord,
) -> Result<std::collections::BTreeMap<String, crate::ast::Value>> {
    if let Some(name) = record.run["inputs"]
        .as_object()
        .and_then(|fields| {
            fields
                .iter()
                .find(|(_, value)| value.get("withheld").is_some())
        })
        .map(|(name, _)| name.clone())
    {
        bail!(
            "resume cannot reproduce the secret answer for input {name:?}; the record withholds it"
        );
    }
    serde_json::from_value(record.run["inputs"].clone())
        .context("the record's inputs cannot be read")
}

/// The loop budget a resume replays under, matching the recorded run. Records
/// written before per-loop limits used 100,000 as their default.
pub fn recorded_loop_budget(
    record: &StoredRecord,
) -> Result<(usize, crate::runtime::LoopBudgetScope)> {
    let recorded_limit = record.run["limits"].get("loop_iterations_max");
    let max_loop_iterations = match recorded_limit {
        None | Some(Json::Null) => 100_000,
        Some(value) => usize::try_from(value.as_u64().context("invalid recorded loop limit")?)
            .context("recorded loop limit exceeds this platform")?,
    };
    let loop_budget_scope = match record.run["limits"].get("loop_iterations_scope") {
        None | Some(Json::Null)
            if recorded_limit.is_none_or(serde_json::Value::is_null) =>
        {
            crate::runtime::LoopBudgetScope::LegacySharedRun
        }
        None | Some(Json::Null) => crate::runtime::LoopBudgetScope::PerLoop,
        Some(Json::String(scope)) if scope == "per_loop" => {
            crate::runtime::LoopBudgetScope::PerLoop
        }
        Some(Json::String(scope)) if scope == "legacy_shared_run" => {
            crate::runtime::LoopBudgetScope::LegacySharedRun
        }
        _ => bail!("invalid recorded loop budget scope"),
    };
    Ok((max_loop_iterations, loop_budget_scope))
}

/// Recorded inputs as strings, for `--as-test` fixtures. Non-string values are
/// rendered as JSON text.
pub fn inputs_as_strings(record: &StoredRecord) -> Json {
    let mut fields = serde_json::Map::new();
    if let Some(inputs) = record.run["inputs"].as_object() {
        for (name, value) in inputs {
            let text = match value {
                Json::String(text) => text.clone(),
                other => other.to_string(),
            };
            fields.insert(name.clone(), Json::String(text));
        }
    }
    Json::Object(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_name_the_line_applets_and_argv() {
        let text = describe(7, &["printf".into()], &[vec!["printf".into(), "a".into()]]);
        assert_eq!(text, "line 7 printf with argv [\"printf\", \"a\"]");
    }

    #[test]
    fn divergence_descriptions_escape_c1_and_bidi_text() {
        let description = describe(
            4,
            &["tool\u{009b}\u{202e}".to_owned()],
            &[vec!["tool\u{009b}".to_owned()]],
        );
        assert!(!description.contains('\u{009b}'));
        assert!(!description.contains('\u{202e}'));
        assert!(description.contains(r"\u{9b}"));
        assert!(description.contains(r"\u{202e}"));
    }

    #[test]
    fn triggered_programs_are_refused() {
        let program = crate::parser::parse("every 1 hour\n  run \"true\"\n").unwrap();
        assert!(unsupported_reason(&program).is_some());
        let plain = crate::parser::parse("run \"true\"\n").unwrap();
        assert!(unsupported_reason(&plain).is_none());
    }

    #[test]
    fn replay_refuses_detached_launches_and_handovers() {
        for source in ["launch detached \"true\"\n", "hand over to \"true\"\n"] {
            let program = crate::parser::parse(source).unwrap();
            assert!(unsupported_reason(&program).is_some(), "{source}");
        }
    }
}
