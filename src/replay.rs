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
use crate::runtime::observe::{InputKeyPolicy, Observer, StepRequest, StepResult};
use crate::runtime::ProgramExit;
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

/// The explicit exit a replay reports, mirroring normal execution. Only a
/// completed replay — every recorded step replayed, no divergence — reports
/// the replayed program's `exit with status` / `fail with message` outcome.
/// An early stop or a divergence keeps its own outcome instead.
pub fn completed_exit(stop: &Stop, exit: Option<ProgramExit>) -> Option<ProgramExit> {
    match stop {
        Stop::Completed => exit,
        Stop::UntilLine { .. } | Stop::Diverged(_) | Stop::UnusedRecordedSteps { .. } => None,
    }
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
    warned_unverified_input: bool,
}

impl Replayer {
    /// Open a record for replay under the given registry. Declared secrets present
    /// in the environment are registered first, so recorded redactions match.
    pub fn new(record: StoredRecord, registry: &Registry, until: Option<usize>) -> Result<Self> {
        refuse_task_groups(&record)?;
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
    fn input_key_policy(&self) -> InputKeyPolicy {
        InputKeyPolicy::Existing
    }

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
        let input = input_check(recorded, request);
        if !same_group(recorded, request, &actual) || input == InputCheck::Changed {
            let divergence = Divergence {
                step: index,
                line: request.line,
                expected: describe_recorded_input(recorded, input),
                actual: describe_request_input(request, &actual, input),
            };
            state.stop = Some(Stop::Diverged(divergence.clone()));
            bail!(
                "replay diverged at step {index}: expected {}, got {}",
                divergence.expected,
                divergence.actual
            );
        }
        if input == InputCheck::Unverified && !state.warned_unverified_input {
            state.warned_unverified_input = true;
            eprintln!(
                "warning: replay cannot check the input of the group at line {}; the record holds no comparable input digest for it, so changed input is not detected",
                request.line
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
        Ok(Some(recorded_step_result(recorded, stdout, stderr)))
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

/// The runtime result for a recorded step. Records written before the signal and
/// stage metadata have none of it, which reads as not recorded.
fn recorded_step_result(recorded: &StoredStep, stdout: Vec<u8>, stderr: Vec<u8>) -> StepResult {
    StepResult {
        status: recorded.status,
        signal: recorded.signal,
        stages: recorded.stages.clone(),
        failed_stage: recorded.failed_stage,
        stdout,
        stderr,
        outcome: stored_outcome(recorded),
        failed: recorded.failed,
        // Replay runs no group and persists nothing, so there are no live labels.
        sensitivity: security::SensitivitySet::empty(),
        secret_environment: false,
    }
}

fn same_group(recorded: &StoredStep, request: &StepRequest, actual_argv: &[Vec<String>]) -> bool {
    recorded.line == request.line
        && recorded.applets == request.applets
        && recorded.binaries == request.binaries
        && recorded.argv == actual_argv
}

/// How the replayed group's input compares with the recorded input digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputCheck {
    /// Both digests were made under the same key and are equal.
    Matches,
    /// Both digests were made under the same key and differ: the input changed.
    Changed,
    /// The input cannot be compared: an older record without a digest, a stdin or
    /// file input, or a digest made under a different key. Replay still
    /// substitutes the recorded output, and warns once.
    Unverified,
}

fn input_check(recorded: &StoredStep, request: &StepRequest) -> InputCheck {
    let (Some(recorded), Some(actual)) = (&recorded.input_digest, &request.input_digest) else {
        return InputCheck::Unverified;
    };
    match (recorded.split_once(':'), actual.split_once(':')) {
        (Some((key, digest)), Some((actual_key, actual_digest))) if key == actual_key => {
            if digest == actual_digest {
                InputCheck::Matches
            } else {
                InputCheck::Changed
            }
        }
        _ => InputCheck::Unverified,
    }
}

fn describe_recorded(step: &StoredStep) -> String {
    describe(step.line, &step.applets, &step.argv)
}

/// The recorded description, naming the input when the input changed. Digests and
/// input contents never appear in it.
fn describe_recorded_input(step: &StoredStep, input: InputCheck) -> String {
    let text = describe_recorded(step);
    if input == InputCheck::Changed {
        format!("{text} with the recorded input")
    } else {
        text
    }
}

fn describe_request_input(
    request: &StepRequest,
    argv: &[Vec<String>],
    input: InputCheck,
) -> String {
    let text = describe_request(request, argv);
    if input == InputCheck::Changed {
        format!("{text} with changed input")
    } else {
        text
    }
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

/// The line of the first statement that holds a parallel or race task group, in
/// the top-level steps or in a reusable program's steps.
pub fn task_group_line(program: &Program) -> Option<usize> {
    let holds = |steps: &[crate::ast::Step]| {
        steps
            .iter()
            .find(|step| format!("{:?}", step.kind).contains("TaskGroup"))
            .map(|step| step.line)
    };
    holds(&program.steps).or_else(|| {
        program
            .definitions
            .values()
            .find_map(|definition| holds(&definition.steps))
    })
}

/// Refuse to replay a recorded program with a parallel or race task group. Their
/// results arrive in completion order, which the sequential replay cursor cannot
/// follow (issue #690). Recording such a program is still allowed.
fn refuse_task_groups(record: &StoredRecord) -> Result<()> {
    let source = record.program_source()?;
    let text = std::str::from_utf8(&source).context("the recorded program is not UTF-8")?;
    let program = crate::program_files::parse_source_resolved(&record.program_path(), text, None)
        .context("parsing the recorded program for replay")?;
    match task_group_line(&program) {
        Some(line) => bail!(
            "replay does not support parallel or race task groups (line {line}): their completion order is not deterministic"
        ),
        None => Ok(()),
    }
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
    warned_unverified_input: bool,
}

impl PrefixReplayer {
    /// Replay `record`'s completed prefix below `boundary`. Declared secrets
    /// present in the environment are registered first, so recorded redactions
    /// match.
    pub fn new(record: StoredRecord, registry: &Registry, boundary: usize) -> Result<Self> {
        refuse_task_groups(&record)?;
        let environment = std::env::vars_os().collect::<Vec<_>>();
        security::register_declared_environment_secrets(registry, &environment)
            .map_err(|error| anyhow!("registering declared secrets: {error}"))?;
        let prefix = record
            .steps
            .iter()
            .position(crate::record::step_failed)
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
    fn input_key_policy(&self) -> InputKeyPolicy {
        InputKeyPolicy::Existing
    }

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
        let input = input_check(recorded, request);
        if !same_group(recorded, request, &actual) || input == InputCheck::Changed {
            let divergence = Divergence {
                step: index,
                line: request.line,
                expected: describe_recorded_input(recorded, input),
                actual: describe_request_input(request, &actual, input),
            };
            bail!(
                "resume diverged at step {index}: expected {}, got {}; the completed prefix no longer matches, so it cannot be skipped",
                divergence.expected,
                divergence.actual
            );
        }
        if input == InputCheck::Unverified && !state.warned_unverified_input {
            state.warned_unverified_input = true;
            eprintln!(
                "warning: resume cannot check the input of the group at line {}; the record holds no comparable input digest for it, so changed input is not detected",
                request.line
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
        Ok(Some(recorded_step_result(recorded, stdout, stderr)))
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
        // Lines below the recorded boundary are the completed prefix. Lines
        // at or past it are suppressed while the run is still inside the
        // prefix: every step executed before the fall-through already ran in
        // the recorded run, including file writes in a failed block or a
        // loop body whose lines sit at or past the boundary. Once the
        // repaired suffix runs natively, only the completed prefix stays
        // suppressed.
        line < self.boundary || !self.fell_through()
    }

    fn suppresses_terminal_output(&self, line: usize) -> bool {
        line < self.boundary || !self.fell_through()
    }
}

/// Recorded inputs for a resume, refusing a secret answer the record withheld.
/// A withheld answer cannot be reproduced, so resuming with a marker in its
/// place would run a different program than the recorded one.
pub fn recorded_inputs(
    record: &StoredRecord,
) -> Result<std::collections::BTreeMap<String, crate::ast::Value>> {
    crate::record::check_replayable_after_redaction(record)?;
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
        None | Some(Json::Null) if recorded_limit.is_none_or(serde_json::Value::is_null) => {
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
    fn completed_replay_reports_the_explicit_exit_status_and_message() {
        for (status, message) in [
            (7, Some("bad input".to_owned())),
            (7, None),
            (0, Some("done".to_owned())),
            (0, None),
        ] {
            let exit = ProgramExit {
                status,
                message: message.clone(),
            };
            assert_eq!(
                completed_exit(&Stop::Completed, Some(exit)),
                Some(ProgramExit { status, message })
            );
        }
        // A normal completion has no explicit exit to report.
        assert_eq!(completed_exit(&Stop::Completed, None), None);
    }

    #[test]
    fn early_stop_and_divergence_do_not_report_the_explicit_exit() {
        let exit = || {
            Some(ProgramExit {
                status: 7,
                message: Some("bad input".into()),
            })
        };
        for stop in [
            Stop::UntilLine {
                line: 2,
                steps_replayed: 1,
            },
            Stop::Diverged(Divergence {
                step: 0,
                line: 1,
                expected: "recorded".into(),
                actual: "replayed".into(),
            }),
            Stop::UnusedRecordedSteps { next: 1, line: 2 },
        ] {
            assert_eq!(completed_exit(&stop, exit()), None, "{stop:?}");
        }
    }

    #[test]
    fn replay_refuses_detached_launches_and_handovers() {
        for source in ["launch detached \"true\"\n", "hand over to \"true\"\n"] {
            let program = crate::parser::parse(source).unwrap();
            assert!(unsupported_reason(&program).is_some(), "{source}");
        }
    }

    fn stored_step(input_digest: Option<&str>) -> StoredStep {
        StoredStep {
            index: 0,
            line: 1,
            applets: vec!["cat".into()],
            binaries: vec!["cat".into()],
            argv: vec![vec!["cat".into()]],
            input_digest: input_digest.map(str::to_owned),
            status: Some(0),
            signal: None,
            stages: None,
            failed_stage: None,
            outcome: None,
            failed: false,
            wall_us: 0,
            stdout: Json::Null,
            stderr: Json::Null,
        }
    }

    #[test]
    fn a_recorded_old_failure_is_reported_as_unrecorded_not_replayed() {
        // A record from before signal and stage metadata: a failed step with no exit
        // code cannot say what killed it, so replay refuses it with the reason.
        let old = StoredStep {
            status: None,
            failed: true,
            ..stored_step(None)
        };
        let result = recorded_step_result(&old, Vec::new(), Vec::new());
        assert_eq!(result.stages, None);
        let reason = crate::runtime::observe::unreplayable_reason(&result)
            .expect("an unrecorded signal failure must be refused");
        assert!(reason.contains("predates"), "{reason}");
    }

    #[test]
    fn a_recorded_signal_and_stages_reach_the_runtime_result() {
        let stages = vec![crate::runtime::observe::RecordedStage {
            line: 1,
            applet: "sh".into(),
            state: crate::runtime::execution_result::StageState::Signaled,
            status: None,
            signal: Some(15),
            broken_pipe: false,
        }];
        let recorded = StoredStep {
            status: None,
            signal: Some(15),
            stages: Some(stages.clone()),
            failed_stage: Some(0),
            failed: true,
            ..stored_step(None)
        };
        let result = recorded_step_result(&recorded, Vec::new(), Vec::new());
        assert_eq!(result.signal, Some(15));
        assert_eq!(result.failed_stage, Some(0));
        assert_eq!(result.stages, Some(stages));
        assert_eq!(
            crate::runtime::observe::unreplayable_reason(&result),
            None,
            "a recorded signal is replayable"
        );
    }

    fn request_with_digest(input_digest: Option<&str>) -> StepRequest {
        StepRequest {
            line: 1,
            applets: vec!["cat".into()],
            binaries: vec!["cat".into()],
            argv: vec![vec!["cat".into()]],
            input_digest: input_digest.map(str::to_owned),
        }
    }

    #[test]
    fn changed_upstream_input_is_detected_and_unchanged_input_is_not() {
        let recorded = stored_step(Some("0123456789abcdef:aaaa"));
        assert_eq!(
            input_check(
                &recorded,
                &request_with_digest(Some("0123456789abcdef:bbbb"))
            ),
            InputCheck::Changed
        );
        assert_eq!(
            input_check(
                &recorded,
                &request_with_digest(Some("0123456789abcdef:aaaa"))
            ),
            InputCheck::Matches
        );
    }

    #[test]
    fn records_without_comparable_input_are_unverified_not_divergent() {
        let legacy = stored_step(None);
        assert_eq!(
            input_check(&legacy, &request_with_digest(Some("0123456789abcdef:aaaa"))),
            InputCheck::Unverified
        );
        let recorded = stored_step(Some("0123456789abcdef:aaaa"));
        // A different key, or input that is not in memory, cannot be compared.
        assert_eq!(
            input_check(
                &recorded,
                &request_with_digest(Some("fedcba9876543210:aaaa"))
            ),
            InputCheck::Unverified
        );
        assert_eq!(
            input_check(&recorded, &request_with_digest(None)),
            InputCheck::Unverified
        );
    }

    #[test]
    fn task_groups_record_but_replay_refuses_them_with_their_line() {
        let group = [
            "parallel at most 2 as results\n  run number.add with left 20 and right 22 as sum\n",
            "race at most 2 as winner\n  run number.add with left 20 and right 22 as sum\n",
        ];
        for source in group {
            let program = crate::parser::parse(source).unwrap();
            // Record side: a task-group program is still accepted for recording.
            assert_eq!(unsupported_reason(&program), None, "{source}");
            // Replay side: the group is found on its statement line and refused.
            assert_eq!(task_group_line(&program), Some(1), "{source}");
        }
        let nested = crate::parser::parse(
            "define a program named worker\n  parallel at most 2 as results\n    run number.add with left 20 and right 22 as sum\nrun program worker as result\nuse value result\n",
        )
        .unwrap();
        assert_eq!(unsupported_reason(&nested), None);
        assert_eq!(task_group_line(&nested), Some(2));
        let sequential = crate::parser::parse("run \"true\"\n").unwrap();
        assert_eq!(task_group_line(&sequential), None);
    }
}
