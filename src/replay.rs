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
use anyhow::{anyhow, bail, Result};
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
