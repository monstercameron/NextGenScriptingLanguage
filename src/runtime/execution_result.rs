//! Structured results for applet execution and explicit recovery.
//!
//! Ordinary applet outcomes keep their declared manifest classification.
//! Runtime failures use separate kinds and never invent an exit status.

use crate::ast::Value;
pub use crate::contracts::EffectCertainty;
use crate::contracts::{Outcome, OutcomeKind};
use std::collections::BTreeMap;
use std::fmt;

/// Stable classification of an applet execution result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionKind {
    Success,
    Absence,
    Failure,
    Retryable,
    Timeout,
    Cancelled,
    Signaled,
    SpawnError,
    TransportError,
    ContractDecodeError,
    IoError,
    OutputLimit,
    CapacityError,
    UnknownError,
}

impl ExecutionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Absence => "absence",
            Self::Failure => "failure",
            Self::Retryable => "retryable",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Signaled => "signaled",
            Self::SpawnError => "spawn_error",
            Self::TransportError => "transport_error",
            Self::ContractDecodeError => "contract_decode_error",
            Self::IoError => "io_error",
            Self::OutputLimit => "output_limit",
            Self::CapacityError => "capacity_error",
            Self::UnknownError => "unknown_error",
        }
    }

    pub const fn from_outcome(kind: OutcomeKind) -> Self {
        match kind {
            OutcomeKind::Success => Self::Success,
            OutcomeKind::Absence => Self::Absence,
            OutcomeKind::Failure => Self::Failure,
            OutcomeKind::Retryable => Self::Retryable,
        }
    }
}

/// The observed state of one command in a native pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    Exited,
    Signaled,
    SpawnFailed,
    NotStarted,
    Unknown,
}

impl StageState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Signaled => "signaled",
            Self::SpawnFailed => "spawn_failed",
            Self::NotStarted => "not_started",
            Self::Unknown => "unknown",
        }
    }
}

/// One pipeline stage, including the status and captured diagnostic bytes
/// known when the execution result was assembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageResult {
    pub line: usize,
    pub applet: String,
    pub state: StageState,
    pub status: Option<i32>,
    pub signal: Option<i32>,
    pub outcome: Option<String>,
    pub kind: Option<ExecutionKind>,
    pub effect_certainty: EffectCertainty,
    /// `None` means stderr was inherited or discarded. `Some(Vec::new())`
    /// means it was captured and empty.
    pub stderr: Option<Vec<u8>>,
}

impl StageResult {
    pub fn not_started(line: usize, applet: impl Into<String>) -> Self {
        Self {
            line,
            applet: applet.into(),
            state: StageState::NotStarted,
            status: None,
            signal: None,
            outcome: None,
            kind: None,
            effect_certainty: EffectCertainty::NotStarted,
            stderr: None,
        }
    }

    pub fn spawn_failed(line: usize, applet: impl Into<String>) -> Self {
        Self {
            line,
            applet: applet.into(),
            state: StageState::SpawnFailed,
            status: None,
            signal: None,
            outcome: None,
            kind: Some(ExecutionKind::SpawnError),
            effect_certainty: EffectCertainty::NotStarted,
            stderr: None,
        }
    }

    pub fn unknown(line: usize, applet: impl Into<String>) -> Self {
        Self {
            line,
            applet: applet.into(),
            state: StageState::Unknown,
            status: None,
            signal: None,
            outcome: None,
            kind: None,
            effect_certainty: EffectCertainty::Uncertain,
            stderr: None,
        }
    }

    pub fn exited(
        line: usize,
        applet: impl Into<String>,
        status: i32,
        outcome: Outcome,
        stderr: Option<Vec<u8>>,
    ) -> Self {
        let effect_certainty = outcome.effect_certainty;
        Self {
            line,
            applet: applet.into(),
            state: StageState::Exited,
            status: Some(status),
            signal: None,
            outcome: Some(outcome.name),
            kind: Some(ExecutionKind::from_outcome(outcome.kind)),
            effect_certainty,
            stderr,
        }
    }

    pub fn signaled(
        line: usize,
        applet: impl Into<String>,
        signal: Option<i32>,
        stderr: Option<Vec<u8>>,
    ) -> Self {
        Self {
            line,
            applet: applet.into(),
            state: StageState::Signaled,
            status: None,
            signal,
            outcome: None,
            kind: Some(ExecutionKind::Signaled),
            effect_certainty: EffectCertainty::Uncertain,
            stderr,
        }
    }

    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::new();
        fields.insert("line".into(), Value::Integer(self.line as i64));
        fields.insert("applet".into(), Value::String(self.applet.clone()));
        fields.insert("state".into(), Value::String(self.state.as_str().into()));
        fields.insert("status".into(), optional_integer(self.status));
        fields.insert("signal".into(), optional_integer(self.signal));
        fields.insert("outcome".into(), optional_text(self.outcome.as_deref()));
        fields.insert(
            "kind".into(),
            self.kind
                .map(|kind| Value::String(kind.as_str().into()))
                .unwrap_or(Value::Null),
        );
        fields.insert(
            "effect_certainty".into(),
            Value::String(self.effect_certainty.as_str().into()),
        );
        fields.insert(
            "stderr".into(),
            self.stderr
                .as_deref()
                .map(bytes_value)
                .unwrap_or_else(|| Value::List(Vec::new())),
        );
        fields.insert("stderr_captured".into(), Value::Bool(self.stderr.is_some()));
        Value::Record(fields)
    }
}

/// Bounded successful-invocation metadata retained by `RunResult`. Raw output
/// and stderr stay on the existing output/catch paths; these byte counts and
/// capture flags preserve what the runtime actually observed without
/// duplicating potentially large buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageMetadata {
    pub index: usize,
    pub line: usize,
    pub applet: String,
    pub state: StageState,
    pub status: Option<i32>,
    pub signal: Option<i32>,
    pub outcome: Option<String>,
    pub kind: Option<ExecutionKind>,
    pub effect_certainty: EffectCertainty,
    pub stderr_captured: bool,
    pub stderr_bytes: usize,
}

impl StageMetadata {
    fn from_stage(index: usize, stage: &StageResult) -> Self {
        Self {
            index,
            line: stage.line,
            applet: stage.applet.clone(),
            state: stage.state,
            status: stage.status,
            signal: stage.signal,
            outcome: stage.outcome.clone(),
            kind: stage.kind,
            effect_certainty: stage.effect_certainty,
            stderr_captured: stage.stderr.is_some(),
            stderr_bytes: stage.stderr.as_ref().map_or(0, Vec::len),
        }
    }

    fn to_value(&self) -> Value {
        let mut fields = BTreeMap::new();
        fields.insert("index".into(), Value::Integer(self.index as i64));
        fields.insert("line".into(), Value::Integer(self.line as i64));
        fields.insert("applet".into(), Value::String(self.applet.clone()));
        fields.insert("state".into(), Value::String(self.state.as_str().into()));
        fields.insert("status".into(), optional_integer(self.status));
        fields.insert("signal".into(), optional_integer(self.signal));
        fields.insert("outcome".into(), optional_text(self.outcome.as_deref()));
        fields.insert(
            "kind".into(),
            self.kind
                .map(|kind| Value::String(kind.as_str().into()))
                .unwrap_or(Value::Null),
        );
        fields.insert(
            "effect_certainty".into(),
            Value::String(self.effect_certainty.as_str().into()),
        );
        fields.insert("stderr_captured".into(), Value::Bool(self.stderr_captured));
        fields.insert(
            "stderr_bytes".into(),
            Value::Integer(self.stderr_bytes as i64),
        );
        Value::Record(fields)
    }
}

/// One completed logical invocation, which may contain several native
/// pipeline stages. Top-level status/outcome refer to the terminal backend
/// result; each stage keeps its own observed status and signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionRecord {
    pub line: usize,
    /// Stable id of the step that ran the invocation (its label or derived id).
    pub step_id: Option<String>,
    pub applet: String,
    pub kind: ExecutionKind,
    pub outcome: Option<String>,
    pub status: Option<i32>,
    pub signal: Option<i32>,
    pub effect_certainty: EffectCertainty,
    pub stdout_captured: bool,
    pub stdout_bytes: usize,
    pub stderr_captured: bool,
    pub stderr_bytes: usize,
    pub stages: Vec<StageMetadata>,
    /// Number of pipeline stages omitted by the per-run metadata bound.
    pub stages_dropped: usize,
}

impl ExecutionRecord {
    pub fn from_result(line: usize, applet: impl Into<String>, result: &ExecutionResult) -> Self {
        Self::from_result_with_stage_limit(line, applet, result, MAX_EXECUTION_STAGES)
    }

    fn from_result_with_stage_limit(
        line: usize,
        applet: impl Into<String>,
        result: &ExecutionResult,
        stage_limit: usize,
    ) -> Self {
        Self::from_fields(
            line,
            applet,
            result.kind,
            result.outcome.as_deref(),
            result.status,
            result.signal,
            result.effect_certainty,
            result.stdout.as_deref(),
            result.stderr.as_deref(),
            &result.stages,
            stage_limit,
        )
    }

    /// Project the metadata available at a completed invocation boundary
    /// without first owning copies of its captured output buffers.
    pub fn from_outcome(
        line: usize,
        applet: impl Into<String>,
        outcome: &Outcome,
        stdout: Option<&[u8]>,
        stderr: Option<&[u8]>,
        stages: &[StageResult],
    ) -> Self {
        Self::from_outcome_with_stage_limit(
            line,
            applet,
            outcome,
            stdout,
            stderr,
            stages,
            MAX_EXECUTION_STAGES,
        )
    }

    fn from_outcome_with_stage_limit(
        line: usize,
        applet: impl Into<String>,
        outcome: &Outcome,
        stdout: Option<&[u8]>,
        stderr: Option<&[u8]>,
        stages: &[StageResult],
        stage_limit: usize,
    ) -> Self {
        let effect_certainty = combine_effect_certainty(
            std::iter::once(outcome.effect_certainty)
                .chain(stages.iter().map(|stage| stage.effect_certainty)),
        );
        Self::from_fields(
            line,
            applet,
            ExecutionKind::from_outcome(outcome.kind),
            Some(&outcome.name),
            Some(outcome.status),
            None,
            effect_certainty,
            stdout,
            stderr,
            stages,
            stage_limit,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_fields(
        line: usize,
        applet: impl Into<String>,
        kind: ExecutionKind,
        outcome: Option<&str>,
        status: Option<i32>,
        signal: Option<i32>,
        effect_certainty: EffectCertainty,
        stdout: Option<&[u8]>,
        stderr: Option<&[u8]>,
        source_stages: &[StageResult],
        stage_limit: usize,
    ) -> Self {
        let stage_count = source_stages.len();
        let retained_stage_count = stage_count.min(stage_limit);
        let prefix_count = retained_stage_count.saturating_sub(1);
        let last_index = stage_count.checked_sub(1);
        let stages = source_stages
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                *index < prefix_count || (retained_stage_count > 0 && Some(*index) == last_index)
            })
            .map(|(index, stage)| StageMetadata::from_stage(index, stage))
            .collect();
        Self {
            line,
            step_id: super::current_step_id().map(|id| id.to_string()),
            applet: applet.into(),
            kind,
            outcome: outcome.map(str::to_owned),
            status,
            signal,
            effect_certainty,
            stdout_captured: stdout.is_some(),
            stdout_bytes: stdout.map_or(0, <[u8]>::len),
            stderr_captured: stderr.is_some(),
            stderr_bytes: stderr.map_or(0, <[u8]>::len),
            stages,
            stages_dropped: stage_count - retained_stage_count,
        }
    }

    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::new();
        fields.insert("line".into(), Value::Integer(self.line as i64));
        fields.insert("step_id".into(), optional_text(self.step_id.as_deref()));
        fields.insert("applet".into(), Value::String(self.applet.clone()));
        fields.insert("kind".into(), Value::String(self.kind.as_str().into()));
        fields.insert("outcome".into(), optional_text(self.outcome.as_deref()));
        fields.insert("status".into(), optional_integer(self.status));
        fields.insert("signal".into(), optional_integer(self.signal));
        fields.insert(
            "effect_certainty".into(),
            Value::String(self.effect_certainty.as_str().into()),
        );
        fields.insert("stdout_captured".into(), Value::Bool(self.stdout_captured));
        fields.insert(
            "stdout_bytes".into(),
            Value::Integer(self.stdout_bytes as i64),
        );
        fields.insert("stderr_captured".into(), Value::Bool(self.stderr_captured));
        fields.insert(
            "stderr_bytes".into(),
            Value::Integer(self.stderr_bytes as i64),
        );
        fields.insert(
            "stages".into(),
            Value::List(self.stages.iter().map(StageMetadata::to_value).collect()),
        );
        fields.insert(
            "stages_dropped".into(),
            Value::Integer(self.stages_dropped as i64),
        );
        Value::Record(fields)
    }
}

pub const MAX_EXECUTION_RECORDS: usize = 256;
pub const MAX_EXECUTION_STAGES: usize = 4096;

/// Retain a bounded execution history without copying captured byte buffers.
/// Callers can expose `executions` and the drop counters additively while
/// keeping ordinary workflow stdout unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionHistory {
    pub executions: Vec<ExecutionRecord>,
    pub dropped_executions: usize,
    pub dropped_stages: usize,
}

impl ExecutionHistory {
    pub fn push(&mut self, line: usize, applet: impl Into<String>, result: &ExecutionResult) {
        if self.executions.len() >= MAX_EXECUTION_RECORDS {
            self.dropped_executions = self.dropped_executions.saturating_add(1);
            self.dropped_stages = self.dropped_stages.saturating_add(result.stages.len());
            return;
        }

        let retained_stages: usize = self.executions.iter().map(|item| item.stages.len()).sum();
        let remaining = MAX_EXECUTION_STAGES.saturating_sub(retained_stages);
        let record = ExecutionRecord::from_result_with_stage_limit(line, applet, result, remaining);
        self.dropped_stages = self.dropped_stages.saturating_add(record.stages_dropped);
        self.executions.push(record);
    }

    /// Retain metadata for a completed invocation directly from borrowed
    /// captures. Returns the stored record when it also matches the normal
    /// per-invocation stage projection, so a requested trace can reuse it.
    pub fn push_outcome(
        &mut self,
        line: usize,
        applet: impl Into<String>,
        outcome: &Outcome,
        stdout: Option<&[u8]>,
        stderr: Option<&[u8]>,
        stages: &[StageResult],
    ) -> Option<&ExecutionRecord> {
        if self.executions.len() >= MAX_EXECUTION_RECORDS {
            self.dropped_executions = self.dropped_executions.saturating_add(1);
            self.dropped_stages = self.dropped_stages.saturating_add(stages.len());
            return None;
        }

        let retained_stages: usize = self.executions.iter().map(|item| item.stages.len()).sum();
        let remaining = MAX_EXECUTION_STAGES.saturating_sub(retained_stages);
        let trace_stages = stages.len().min(MAX_EXECUTION_STAGES);
        let record = ExecutionRecord::from_outcome_with_stage_limit(
            line, applet, outcome, stdout, stderr, stages, remaining,
        );
        self.dropped_stages = self.dropped_stages.saturating_add(record.stages_dropped);
        self.executions.push(record);
        (remaining >= trace_stages).then(|| self.executions.last().expect("record was just pushed"))
    }

    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::new();
        fields.insert(
            "executions".into(),
            Value::List(
                self.executions
                    .iter()
                    .map(ExecutionRecord::to_value)
                    .collect(),
            ),
        );
        fields.insert(
            "dropped_executions".into(),
            Value::Integer(self.dropped_executions as i64),
        );
        fields.insert(
            "dropped_stages".into(),
            Value::Integer(self.dropped_stages as i64),
        );
        Value::Record(fields)
    }
}

/// Complete applet result available to explicit source-level recovery.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionResult {
    pub kind: ExecutionKind,
    pub outcome: Option<String>,
    /// Only populated when the backend returned an actual process/app-result
    /// status. Timeouts, signals, launch errors and transport errors have none.
    pub status: Option<i32>,
    pub signal: Option<i32>,
    /// Captured terminal stdout. `None` means it was not retained.
    pub stdout: Option<Vec<u8>>,
    /// Decoded typed output when its schema accepted the successful response.
    pub value: Option<Value>,
    pub stderr: Option<Vec<u8>>,
    pub stages: Vec<StageResult>,
    pub message: Option<String>,
    pub effect_certainty: EffectCertainty,
}

impl ExecutionResult {
    pub fn from_outcome(
        outcome: Outcome,
        stdout: Option<Vec<u8>>,
        stderr: Option<Vec<u8>>,
        value: Option<Value>,
        stages: Vec<StageResult>,
    ) -> Self {
        let effect_certainty = combine_effect_certainty(
            std::iter::once(outcome.effect_certainty)
                .chain(stages.iter().map(|stage| stage.effect_certainty)),
        );
        Self {
            kind: ExecutionKind::from_outcome(outcome.kind),
            outcome: Some(outcome.name),
            status: Some(outcome.status),
            signal: None,
            stdout,
            value,
            stderr,
            stages,
            message: None,
            effect_certainty,
        }
    }

    pub fn runtime_failure(
        kind: ExecutionKind,
        message: impl Into<String>,
        effect_certainty: EffectCertainty,
    ) -> Self {
        Self {
            kind,
            outcome: None,
            status: None,
            signal: None,
            stdout: None,
            value: None,
            stderr: None,
            stages: Vec::new(),
            message: Some(message.into()),
            effect_certainty,
        }
    }

    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::new();
        fields.insert("kind".into(), Value::String(self.kind.as_str().into()));
        fields.insert("outcome".into(), optional_text(self.outcome.as_deref()));
        fields.insert("status".into(), optional_integer(self.status));
        fields.insert("signal".into(), optional_integer(self.signal));
        fields.insert(
            "stdout".into(),
            self.stdout
                .as_deref()
                .map(bytes_value)
                .unwrap_or_else(|| Value::List(Vec::new())),
        );
        fields.insert("stdout_captured".into(), Value::Bool(self.stdout.is_some()));
        fields.insert("value".into(), self.value.clone().unwrap_or(Value::Null));
        fields.insert("value_captured".into(), Value::Bool(self.value.is_some()));
        fields.insert(
            "stderr".into(),
            self.stderr
                .as_deref()
                .map(bytes_value)
                .unwrap_or_else(|| Value::List(Vec::new())),
        );
        fields.insert("stderr_captured".into(), Value::Bool(self.stderr.is_some()));
        fields.insert(
            "stages".into(),
            Value::List(self.stages.iter().map(StageResult::to_value).collect()),
        );
        fields.insert("message".into(), optional_text(self.message.as_deref()));
        fields.insert(
            "effect_certainty".into(),
            Value::String(self.effect_certainty.as_str().into()),
        );
        Value::Record(fields)
    }

    /// Build a source-visible execution record. Unlike the bounded run
    /// history and trace projection, this explicit binding includes captured
    /// terminal bytes and a decoded value when one was requested. Stage
    /// metadata remains bounded by `MAX_EXECUTION_STAGES`.
    pub fn to_source_value(&self, line: usize, applet: impl Into<String>) -> Value {
        let Value::Record(mut fields) = ExecutionRecord::from_result(line, applet, self).to_value()
        else {
            unreachable!("execution metadata projection is a record")
        };
        fields.insert(
            "stdout".into(),
            self.stdout
                .as_deref()
                .map(bytes_value)
                .unwrap_or_else(|| Value::List(Vec::new())),
        );
        fields.insert("value".into(), self.value.clone().unwrap_or(Value::Null));
        fields.insert("value_captured".into(), Value::Bool(self.value.is_some()));
        fields.insert(
            "stderr".into(),
            self.stderr
                .as_deref()
                .map(bytes_value)
                .unwrap_or_else(|| Value::List(Vec::new())),
        );
        fields.insert("message".into(), optional_text(self.message.as_deref()));
        Value::Record(fields)
    }
}

/// Typed error wrapper used by backend adapters that know why an invocation
/// failed. It carries no implied status code.
#[derive(Debug, Clone)]
pub struct ExecutionFailure {
    pub result: ExecutionResult,
}

impl ExecutionFailure {
    pub fn new(result: ExecutionResult) -> Self {
        Self { result }
    }

    pub fn error_value(&self) -> Value {
        self.result.to_value()
    }
}

/// Classify a rejected successful response while retaining the actual
/// completed status and any bytes already captured from its stage.
pub fn contract_decode_failure(
    error: &anyhow::Error,
    outcome: Outcome,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
    stages: Vec<StageResult>,
) -> ExecutionFailure {
    output_decode_failure(error, Some(outcome), stdout, stderr, stages)
}

/// Classify a boundary that rejected bytes after a backend returned a valid
/// completed status. Unlike a native streaming cap, this case retains the
/// actual response status and declared outcome.
pub fn completed_output_limit_failure(
    message: impl Into<String>,
    outcome: Outcome,
    line: usize,
    applet: impl Into<String>,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
) -> ExecutionFailure {
    let mut result = ExecutionResult::from_outcome(
        outcome.clone(),
        stdout,
        stderr.clone(),
        None,
        vec![StageResult::exited(
            line,
            applet,
            outcome.status,
            outcome,
            stderr,
        )],
    );
    result.kind = ExecutionKind::OutputLimit;
    result.message = Some(message.into());
    ExecutionFailure::new(result)
}

/// Classify any rejected output boundary, including legacy UTF-8 binding,
/// while retaining an optional actual status/outcome and all captured data.
pub fn output_decode_failure(
    error: &anyhow::Error,
    outcome: Option<Outcome>,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
    stages: Vec<StageResult>,
) -> ExecutionFailure {
    let mut result = if let Some(outcome) = outcome {
        ExecutionResult::from_outcome(outcome, stdout, stderr, None, stages)
    } else {
        ExecutionResult::runtime_failure(
            ExecutionKind::ContractDecodeError,
            format!("{error:#}"),
            EffectCertainty::Uncertain,
        )
    };
    result.kind = ExecutionKind::ContractDecodeError;
    // A response that cannot be decoded does not establish whether the
    // applet's declared effect completed as intended.
    result.effect_certainty = EffectCertainty::Uncertain;
    result.message = Some(format!("{error:#}"));
    ExecutionFailure::new(result)
}

impl fmt::Display for ExecutionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(message) = &self.result.message {
            formatter.write_str(message)
        } else {
            formatter.write_str(self.result.kind.as_str())
        }
    }
}

impl std::error::Error for ExecutionFailure {}

/// Convert a runtime error into the stable value exposed by `catch`.
/// Unrecognized errors are conservative: the effect may have happened.
pub fn error_value(error: &anyhow::Error) -> Value {
    result_for_error(error).to_value()
}

/// Preserve the typed execution metadata carried by a runtime error.
pub(crate) fn result_for_error(error: &anyhow::Error) -> ExecutionResult {
    if let Some(failure) = error.downcast_ref::<ExecutionFailure>() {
        return failure.result.clone();
    }
    if let Some(error) = error.downcast_ref::<super::AppletInvocationError>() {
        return result_for_error(&error.source);
    }

    if let Some(error) = error.downcast_ref::<super::jobs::JobSpawnFailure>() {
        return error.result.clone();
    }

    if let Some(error) = error.downcast_ref::<super::CommandFailed>() {
        return command_failure_result(error);
    }

    if error.downcast_ref::<super::CommandTimedOut>().is_some() {
        return ExecutionResult::runtime_failure(
            ExecutionKind::Timeout,
            error.to_string(),
            // A bare runtime deadline can occur after earlier steps have
            // completed, so only a backend that knows no work began may
            // claim NotStarted.
            EffectCertainty::Uncertain,
        );
    }

    if error.downcast_ref::<std::io::Error>().is_some() {
        return ExecutionResult::runtime_failure(
            ExecutionKind::IoError,
            format!("{error:#}"),
            EffectCertainty::Uncertain,
        );
    }

    if let Some(error) = error.downcast_ref::<super::worker_backend::WorkerError>() {
        return worker_error_result(error, None);
    }

    #[cfg(not(target_arch = "wasm32"))]
    if let Some(error) = error.downcast_ref::<crate::contracts::RemoteError>() {
        let (kind, message) = remote_error_kind(error);
        return ExecutionResult::runtime_failure(
            kind,
            message,
            remote_effect_certainty(error.effect_certainty),
        );
    }

    ExecutionResult::runtime_failure(
        ExecutionKind::UnknownError,
        error.to_string(),
        EffectCertainty::Uncertain,
    )
}

/// Convert a typed worker failure into an execution record, attaching the
/// applet's source location and declared outcome when the runtime has them.
pub(crate) fn worker_failure(
    error: &super::worker_backend::WorkerError,
    line: usize,
    applet: impl Into<String>,
    outcome: Option<Outcome>,
) -> ExecutionFailure {
    ExecutionFailure::new(worker_error_result(
        error,
        Some((line, applet.into(), outcome)),
    ))
}

/// Convert a remote invocation failure while preserving the transport's
/// explicit effect certainty. A missing response is represented by an
/// unknown/not-started stage; it never acquires a fabricated process status.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn remote_failure(
    error: &crate::contracts::RemoteError,
    line: usize,
    applet: impl Into<String>,
) -> ExecutionFailure {
    let (kind, message) = remote_error_kind(error);
    let certainty = remote_effect_certainty(error.effect_certainty);
    let stage = if certainty == EffectCertainty::NotStarted {
        StageResult::not_started(line, applet)
    } else {
        let mut stage = StageResult::unknown(line, applet);
        stage.kind = Some(kind);
        stage.effect_certainty = certainty;
        stage
    };
    let mut result = ExecutionResult::runtime_failure(kind, message, certainty);
    result.stages.push(stage);
    ExecutionFailure::new(result)
}

fn worker_error_result(
    error: &super::worker_backend::WorkerError,
    stage_context: Option<(usize, String, Option<Outcome>)>,
) -> ExecutionResult {
    use super::worker_backend::WorkerErrorKind;
    let kind = match error.kind {
        WorkerErrorKind::Timeout => ExecutionKind::Timeout,
        WorkerErrorKind::Cancelled => ExecutionKind::Cancelled,
        WorkerErrorKind::SpawnError => ExecutionKind::SpawnError,
        WorkerErrorKind::TransportError => ExecutionKind::TransportError,
        WorkerErrorKind::ProtocolError => ExecutionKind::ContractDecodeError,
        WorkerErrorKind::OutputLimit => ExecutionKind::OutputLimit,
        WorkerErrorKind::IoError => ExecutionKind::IoError,
        WorkerErrorKind::UnknownError => ExecutionKind::UnknownError,
        WorkerErrorKind::CapacityError => ExecutionKind::CapacityError,
    };
    let stage_outcome = stage_context
        .as_ref()
        .and_then(|(_, _, outcome)| outcome.clone());
    let certainty = if error.uncertain_effect {
        EffectCertainty::Uncertain
    } else if let Some(outcome) = stage_outcome.as_ref() {
        outcome.effect_certainty
    } else if error.completed_status.is_some() {
        // Without a manifest, the known response is the strongest available
        // fact. The applet adapter supplies its declared certainty below.
        EffectCertainty::KnownResponse
    } else {
        EffectCertainty::NotStarted
    };
    let mut result = ExecutionResult::runtime_failure(kind, error.to_string(), certainty);
    result.status = error.completed_status;
    result.outcome = stage_outcome.as_ref().map(|outcome| outcome.name.clone());
    result.stdout = error.captured_stdout.clone();
    result.stderr = error.captured_stderr.clone();
    if let Some((line, applet, _)) = stage_context {
        let state = match error.completed_status {
            Some(_) => StageState::Exited,
            None if error.kind == WorkerErrorKind::SpawnError => StageState::SpawnFailed,
            None if certainty == EffectCertainty::NotStarted => StageState::NotStarted,
            None => StageState::Unknown,
        };
        result.stages.push(StageResult {
            line,
            applet,
            state,
            status: error.completed_status,
            signal: None,
            outcome: stage_outcome.as_ref().map(|outcome| outcome.name.clone()),
            kind: stage_outcome
                .as_ref()
                .map(|outcome| ExecutionKind::from_outcome(outcome.kind))
                .or(Some(kind)),
            effect_certainty: certainty,
            stderr: error.captured_stderr.clone(),
        });
    }
    result
}

/// Read certainty directly from typed runtime failures. Unknown errors remain
/// conservative so orchestration layers never mistake missing metadata for a
/// safe-to-retry result.
pub fn effect_certainty(error: &anyhow::Error) -> EffectCertainty {
    if let Some(failure) = error.downcast_ref::<ExecutionFailure>() {
        return failure.result.effect_certainty;
    }
    if let Some(error) = error.downcast_ref::<super::AppletInvocationError>() {
        return effect_certainty(&error.source);
    }

    if let Some(error) = error.downcast_ref::<super::jobs::JobSpawnFailure>() {
        return error.result.effect_certainty;
    }
    if let Some(error) = error.downcast_ref::<super::CommandFailed>() {
        return command_failure_result(error).effect_certainty;
    }
    if error.downcast_ref::<super::CommandTimedOut>().is_some() {
        return EffectCertainty::Uncertain;
    }
    if let Some(error) = error.downcast_ref::<super::worker_backend::WorkerError>() {
        if error.uncertain_effect {
            EffectCertainty::Uncertain
        } else if error.completed_status.is_some() {
            EffectCertainty::KnownResponse
        } else {
            EffectCertainty::NotStarted
        }
    } else {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(error) = error.downcast_ref::<crate::contracts::RemoteError>() {
            return remote_effect_certainty(error.effect_certainty);
        }
        EffectCertainty::Uncertain
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn remote_effect_certainty(certainty: crate::contracts::EffectCertainty) -> EffectCertainty {
    match certainty {
        crate::contracts::EffectCertainty::NotStarted => EffectCertainty::NotStarted,
        crate::contracts::EffectCertainty::KnownResponse => EffectCertainty::KnownResponse,
        crate::contracts::EffectCertainty::Uncertain => EffectCertainty::Uncertain,
    }
}

fn command_failure_result(error: &super::CommandFailed) -> ExecutionResult {
    if let Some(status) = error.status.code() {
        let outcome = error.outcome.clone().unwrap_or(Outcome {
            status,
            name: format!("exit_{status}"),
            kind: OutcomeKind::Failure,
            effect_certainty: EffectCertainty::Uncertain,
        });
        let mut result = ExecutionResult::from_outcome(
            outcome.clone(),
            error.stdout.clone(),
            error.stderr.clone(),
            None,
            if error.stages.is_empty() {
                vec![StageResult::exited(
                    error.line,
                    error.operation.clone(),
                    status,
                    outcome,
                    error.stderr.clone(),
                )]
            } else {
                error.stages.clone()
            },
        );
        result.effect_certainty =
            combine_effect_certainty(result.stages.iter().map(|stage| stage.effect_certainty));
        result.message = Some(if error.diagnostic.is_empty() {
            error.to_string()
        } else {
            error.diagnostic.clone()
        });
        result
    } else {
        let signal = exit_signal(error.status);
        let mut result = ExecutionResult::runtime_failure(
            if signal.is_some() {
                ExecutionKind::Signaled
            } else {
                ExecutionKind::UnknownError
            },
            error.diagnostic.clone(),
            EffectCertainty::Uncertain,
        );
        result.signal = signal;
        result.stdout = error.stdout.clone();
        result.stderr = error.stderr.clone();
        if result.message.as_deref() == Some("") {
            result.message = Some(error.to_string());
        }
        result.stages = if error.stages.is_empty() {
            vec![if let Some(signal) = signal {
                StageResult::signaled(
                    error.line,
                    error.operation.clone(),
                    Some(signal),
                    error.stderr.clone(),
                )
            } else {
                StageResult::unknown(error.line, error.operation.clone())
            }]
        } else {
            error.stages.clone()
        };
        result
    }
}

fn combine_effect_certainty(
    certainties: impl IntoIterator<Item = EffectCertainty>,
) -> EffectCertainty {
    let mut saw_response = false;
    for certainty in certainties {
        match certainty {
            EffectCertainty::Uncertain => return EffectCertainty::Uncertain,
            EffectCertainty::KnownResponse => saw_response = true,
            EffectCertainty::NotStarted => {}
        }
    }
    if saw_response {
        EffectCertainty::KnownResponse
    } else {
        EffectCertainty::NotStarted
    }
}

#[cfg(unix)]
pub(crate) fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
pub(crate) fn exit_signal(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn remote_error_kind(error: &crate::contracts::RemoteError) -> (ExecutionKind, String) {
    use crate::contracts::RemoteErrorKind;
    let kind = match &error.kind {
        RemoteErrorKind::Cancelled => ExecutionKind::Cancelled,
        RemoteErrorKind::Timeout => ExecutionKind::Timeout,
        RemoteErrorKind::Transport
        | RemoteErrorKind::HttpStatus(_)
        | RemoteErrorKind::StreamClosed => ExecutionKind::TransportError,
        RemoteErrorKind::InvalidResponse
        | RemoteErrorKind::RequestIdMismatch
        | RemoteErrorKind::UnsupportedProtocolVersion(_)
        | RemoteErrorKind::InvalidStream => ExecutionKind::ContractDecodeError,
        RemoteErrorKind::ResponseTooLarge | RemoteErrorKind::StreamTooLarge => {
            ExecutionKind::OutputLimit
        }
        RemoteErrorKind::RequestTooLarge => ExecutionKind::CapacityError,
        _ => ExecutionKind::UnknownError,
    };
    (kind, error.to_string())
}

fn optional_integer(value: Option<i32>) -> Value {
    value
        .map(|value| Value::Integer(i64::from(value)))
        .unwrap_or(Value::Null)
}

fn optional_text(value: Option<&str>) -> Value {
    value
        .map(|value| Value::String(value.to_owned()))
        .unwrap_or(Value::Null)
}

fn bytes_value(bytes: &[u8]) -> Value {
    Value::List(
        bytes
            .iter()
            .map(|byte| Value::Integer(i64::from(*byte)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn fields(value: Value) -> BTreeMap<String, Value> {
        let Value::Record(fields) = value else {
            panic!("expected a record");
        };
        fields
    }

    #[test]
    fn unknown_errors_are_conservative_and_have_no_synthetic_status_or_captures() {
        let result = fields(error_value(&anyhow!("opaque runtime failure")));

        assert_eq!(
            result.get("kind"),
            Some(&Value::String("unknown_error".into()))
        );
        assert_eq!(result.get("status"), Some(&Value::Null));
        assert_eq!(result.get("stdout_captured"), Some(&Value::Bool(false)));
        assert_eq!(result.get("stderr_captured"), Some(&Value::Bool(false)));
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
        assert_eq!(
            result.get("message"),
            Some(&Value::String("opaque runtime failure".into()))
        );
    }

    #[test]
    fn bare_runtime_timeout_is_conservative_without_backend_start_evidence() {
        let error = anyhow::Error::new(super::super::CommandTimedOut);
        let result = fields(error_value(&error));

        assert_eq!(result.get("kind"), Some(&Value::String("timeout".into())));
        assert_eq!(result.get("status"), Some(&Value::Null));
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
    }

    #[test]
    fn io_errors_keep_a_distinct_kind_and_conservative_certainty() {
        let error = anyhow::Error::new(std::io::Error::other("pipe closed"));
        let result = fields(error_value(&error));

        assert_eq!(result.get("kind"), Some(&Value::String("io_error".into())));
        assert_eq!(result.get("status"), Some(&Value::Null));
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn remote_timeout_keeps_backend_effect_certainty_and_timeout_kind() {
        let error = anyhow::Error::new(crate::contracts::RemoteError {
            kind: crate::contracts::RemoteErrorKind::Timeout,
            effect_certainty: crate::contracts::EffectCertainty::Uncertain,
        });
        let result = fields(error_value(&error));

        assert_eq!(result.get("kind"), Some(&Value::String("timeout".into())));
        assert_eq!(result.get("status"), Some(&Value::Null));
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn remote_failure_keeps_stage_context_without_inventing_status() {
        let failure = remote_failure(
            &crate::contracts::RemoteError {
                kind: crate::contracts::RemoteErrorKind::Cancelled,
                effect_certainty: crate::contracts::EffectCertainty::Uncertain,
            },
            12,
            "fixture.remote",
        );
        let result = fields(failure.error_value());

        assert_eq!(result.get("kind"), Some(&Value::String("cancelled".into())));
        assert_eq!(result.get("status"), Some(&Value::Null));
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
        let Some(Value::List(stages)) = result.get("stages") else {
            panic!("expected remote stage record");
        };
        let stage = fields(stages[0].clone());
        assert_eq!(stage.get("line"), Some(&Value::Integer(12)));
        assert_eq!(stage.get("state"), Some(&Value::String("unknown".into())));
        assert_eq!(stage.get("status"), Some(&Value::Null));
    }

    #[test]
    fn contract_decode_failures_keep_the_completed_status_and_captured_stdout() {
        let error = anyhow!("output did not match the declared JSON schema");
        let failure = contract_decode_failure(
            &error,
            Outcome {
                status: 0,
                name: "success".into(),
                kind: OutcomeKind::Success,
                effect_certainty: EffectCertainty::KnownResponse,
            },
            Some(Vec::new()),
            None,
            vec![StageResult::exited(
                9,
                "fixture",
                0,
                Outcome {
                    status: 0,
                    name: "success".into(),
                    kind: OutcomeKind::Success,
                    effect_certainty: EffectCertainty::KnownResponse,
                },
                None,
            )],
        );
        let result = fields(failure.error_value());

        assert_eq!(
            result.get("kind"),
            Some(&Value::String("contract_decode_error".into()))
        );
        assert_eq!(result.get("status"), Some(&Value::Integer(0)));
        assert_eq!(result.get("stdout_captured"), Some(&Value::Bool(true)));
        assert_eq!(result.get("stdout"), Some(&Value::List(Vec::new())));
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
        assert_eq!(
            result.get("stages"),
            Some(&Value::List(vec![StageResult::exited(
                9,
                "fixture",
                0,
                Outcome {
                    status: 0,
                    name: "success".into(),
                    kind: OutcomeKind::Success,
                    effect_certainty: EffectCertainty::KnownResponse,
                },
                None,
            )
            .to_value()]))
        );
    }

    #[test]
    fn completed_response_output_limit_keeps_its_real_status_and_empty_capture() {
        let failure = completed_output_limit_failure(
            "response exceeds the configured output limit",
            Outcome {
                status: 0,
                name: "success".into(),
                kind: OutcomeKind::Success,
                effect_certainty: EffectCertainty::KnownResponse,
            },
            4,
            "fixture",
            Some(Vec::new()),
            Some(Vec::new()),
        );
        let result = fields(failure.error_value());

        assert_eq!(
            result.get("kind"),
            Some(&Value::String("output_limit".into()))
        );
        assert_eq!(result.get("status"), Some(&Value::Integer(0)));
        assert_eq!(result.get("stdout_captured"), Some(&Value::Bool(true)));
        assert_eq!(result.get("stderr_captured"), Some(&Value::Bool(true)));
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("known_response".into()))
        );
    }

    #[test]
    fn execution_history_bounds_stage_and_record_metadata() {
        let outcome = Outcome {
            status: 0,
            name: "success".into(),
            kind: OutcomeKind::Success,
            effect_certainty: EffectCertainty::KnownResponse,
        };
        let stages = (0..MAX_EXECUTION_STAGES + 4)
            .map(|index| {
                StageResult::exited(
                    index + 1,
                    format!("stage_{index}"),
                    0,
                    outcome.clone(),
                    (index == 0).then(Vec::new),
                )
            })
            .collect();
        let result = ExecutionResult::from_outcome(
            outcome,
            Some(Vec::new()),
            Some(b"warn".to_vec()),
            None,
            stages,
        );
        let mut history = ExecutionHistory::default();
        history.push(3, "fixture", &result);

        assert_eq!(history.executions.len(), 1);
        assert_eq!(history.executions[0].stages.len(), MAX_EXECUTION_STAGES);
        assert_eq!(history.executions[0].stages_dropped, 4);
        assert_eq!(
            history.executions[0].stages.last().map(|stage| stage.index),
            Some(MAX_EXECUTION_STAGES + 3)
        );
        assert_eq!(history.dropped_stages, 4);
        assert!(history.executions[0].stdout_captured);
        assert_eq!(history.executions[0].stdout_bytes, 0);
        assert!(history.executions[0].stderr_captured);
        assert_eq!(history.executions[0].stderr_bytes, 4);

        let one_stage_result = ExecutionResult::from_outcome(
            Outcome {
                status: 0,
                name: "success".into(),
                kind: OutcomeKind::Success,
                effect_certainty: EffectCertainty::KnownResponse,
            },
            None,
            None,
            None,
            vec![StageResult::exited(
                2,
                "late_stage",
                0,
                Outcome {
                    status: 0,
                    name: "success".into(),
                    kind: OutcomeKind::Success,
                    effect_certainty: EffectCertainty::KnownResponse,
                },
                None,
            )],
        );
        history.push(5, "later", &one_stage_result);
        assert!(history.executions[1].stages.is_empty());
        assert_eq!(history.executions[1].stages_dropped, 1);
        assert_eq!(history.dropped_stages, 5);

        let small_result = ExecutionResult::from_outcome(
            Outcome {
                status: 0,
                name: "success".into(),
                kind: OutcomeKind::Success,
                effect_certainty: EffectCertainty::KnownResponse,
            },
            None,
            None,
            None,
            Vec::new(),
        );
        for _ in 0..MAX_EXECUTION_RECORDS - history.executions.len() {
            history.push(4, "fixture", &small_result);
        }
        assert_eq!(history.executions.len(), MAX_EXECUTION_RECORDS);
        history.push(4, "overflow", &small_result);
        assert_eq!(history.dropped_executions, 1);
    }

    #[test]
    fn completed_result_certainty_includes_uncertain_earlier_stages() {
        let earlier_outcome = Outcome {
            status: 0,
            name: "completed_with_uncertain_effect".into(),
            kind: OutcomeKind::Success,
            effect_certainty: EffectCertainty::Uncertain,
        };
        let terminal_outcome = Outcome {
            status: 0,
            name: "success".into(),
            kind: OutcomeKind::Success,
            effect_certainty: EffectCertainty::KnownResponse,
        };
        let result = ExecutionResult::from_outcome(
            terminal_outcome.clone(),
            Some(Vec::new()),
            None,
            None,
            vec![
                StageResult::exited(1, "earlier", 0, earlier_outcome, None),
                StageResult::exited(2, "terminal", 0, terminal_outcome, None),
            ],
        );

        assert_eq!(result.effect_certainty, EffectCertainty::Uncertain);
        assert_eq!(
            fields(result.to_value()).get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
        assert_eq!(
            ExecutionRecord::from_result(2, "pipeline", &result).effect_certainty,
            EffectCertainty::Uncertain
        );
    }

    #[test]
    fn trace_projection_is_bounded_metadata_and_omits_payloads() {
        let outcome = Outcome {
            status: 0,
            name: "success".into(),
            kind: OutcomeKind::Success,
            effect_certainty: EffectCertainty::KnownResponse,
        };
        let stages = (0..MAX_EXECUTION_STAGES + 17)
            .map(|index| {
                StageResult::exited(
                    index + 1,
                    format!("stage_{index}"),
                    0,
                    outcome.clone(),
                    Some(b"secret stage diagnostic".to_vec()),
                )
            })
            .collect();
        let result = ExecutionResult::from_outcome(
            outcome,
            Some(b"secret standard output".to_vec()),
            Some(b"secret standard error".to_vec()),
            Some(Value::String("secret decoded value".into())),
            stages,
        );

        // Successful invocation trace events use this same bounded metadata
        // projection instead of `ExecutionResult::to_value`, which is the
        // full-fidelity source-level error/catch representation.
        let projection = ExecutionRecord::from_result(8, "fixture", &result).to_value();
        let projection = fields(projection);
        assert!(!projection.contains_key("stdout"));
        assert!(!projection.contains_key("stderr"));
        assert!(!projection.contains_key("value"));
        assert_eq!(projection.get("stdout_captured"), Some(&Value::Bool(true)));
        assert_eq!(
            projection.get("stdout_bytes"),
            Some(&Value::Integer(b"secret standard output".len() as i64))
        );
        assert_eq!(
            projection.get("stderr_bytes"),
            Some(&Value::Integer(b"secret standard error".len() as i64))
        );
        assert_eq!(projection.get("stages_dropped"), Some(&Value::Integer(17)));
        let Some(Value::List(stages)) = projection.get("stages") else {
            panic!("expected bounded stage metadata");
        };
        assert_eq!(stages.len(), MAX_EXECUTION_STAGES);
        let terminal_stage = fields(stages.last().expect("terminal stage").clone());
        assert_eq!(
            terminal_stage.get("index"),
            Some(&Value::Integer((MAX_EXECUTION_STAGES + 16) as i64))
        );
        assert!(!terminal_stage.contains_key("stderr"));
        assert_eq!(
            terminal_stage.get("stderr_bytes"),
            Some(&Value::Integer(b"secret stage diagnostic".len() as i64))
        );
    }

    #[test]
    fn applet_invocation_wrapper_retains_the_inner_structured_failure() {
        let source = anyhow::Error::new(ExecutionFailure::new(ExecutionResult::runtime_failure(
            ExecutionKind::TransportError,
            "connection dropped after request transmission",
            EffectCertainty::Uncertain,
        )));
        let error = anyhow::Error::new(super::super::AppletInvocationError {
            source,
            launched_processes: 0,
        });
        let result = fields(error_value(&error));

        assert_eq!(
            result.get("kind"),
            Some(&Value::String("transport_error".into()))
        );
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
        assert_eq!(effect_certainty(&error), EffectCertainty::Uncertain);
    }

    #[cfg(unix)]
    #[test]
    fn command_failure_preserves_real_status_and_captured_empty_streams() {
        use std::os::unix::process::ExitStatusExt;

        let outcome = Outcome {
            status: 7,
            name: "invalid_input".into(),
            kind: OutcomeKind::Failure,
            effect_certainty: EffectCertainty::Uncertain,
        };
        let error = super::super::CommandFailed {
            line: 3,
            operation: "example".into(),
            status: std::process::ExitStatus::from_raw(7 << 8),
            outcome: Some(outcome.clone()),
            diagnostic: String::new(),
            stderr: Some(Vec::new()),
            stdout: Some(Vec::new()),
            stages: vec![StageResult::exited(
                3,
                "example",
                7,
                outcome,
                Some(Vec::new()),
            )],
        };
        let error = anyhow::Error::new(error);
        let result = fields(error_value(&error));

        assert_eq!(result.get("kind"), Some(&Value::String("failure".into())));
        assert_eq!(
            result.get("outcome"),
            Some(&Value::String("invalid_input".into()))
        );
        assert_eq!(result.get("status"), Some(&Value::Integer(7)));
        assert_eq!(result.get("stdout_captured"), Some(&Value::Bool(true)));
        assert_eq!(result.get("stdout"), Some(&Value::List(Vec::new())));
        assert_eq!(result.get("stderr_captured"), Some(&Value::Bool(true)));
        assert_eq!(result.get("stderr"), Some(&Value::List(Vec::new())));
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn pipeline_failure_certainty_includes_every_observed_stage() {
        use std::os::unix::process::ExitStatusExt;

        let completed = Outcome {
            status: 0,
            name: "success".into(),
            kind: OutcomeKind::Success,
            effect_certainty: EffectCertainty::KnownResponse,
        };
        let not_started = Outcome {
            status: 75,
            name: "busy".into(),
            kind: OutcomeKind::Retryable,
            effect_certainty: EffectCertainty::NotStarted,
        };
        let error = super::super::CommandFailed {
            line: 2,
            operation: "retryable-stage".into(),
            status: std::process::ExitStatus::from_raw(75 << 8),
            outcome: Some(not_started.clone()),
            diagnostic: String::new(),
            stderr: None,
            stdout: None,
            stages: vec![
                StageResult::exited(1, "upstream", 0, completed, None),
                StageResult::exited(2, "retryable-stage", 75, not_started, None),
            ],
        };
        let result = fields(error_value(&anyhow::Error::new(error)));

        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("known_response".into()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn signaled_command_has_no_exit_status_and_keeps_signal_separate() {
        use std::os::unix::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(libc::SIGTERM);
        let error = super::super::CommandFailed {
            line: 4,
            operation: "sleep".into(),
            status,
            outcome: None,
            diagnostic: "interrupted".into(),
            stderr: None,
            stdout: None,
            stages: vec![StageResult::signaled(4, "sleep", Some(libc::SIGTERM), None)],
        };
        let error = anyhow::Error::new(error);
        let result = fields(error_value(&error));

        assert_eq!(result.get("kind"), Some(&Value::String("signaled".into())));
        assert_eq!(result.get("status"), Some(&Value::Null));
        assert_eq!(
            result.get("signal"),
            Some(&Value::Integer(libc::SIGTERM as i64))
        );
        assert_eq!(
            result.get("effect_certainty"),
            Some(&Value::String("uncertain".into()))
        );
    }
}
