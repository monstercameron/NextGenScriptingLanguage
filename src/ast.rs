use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    String(String),
    Integer(i64),
    Bool(bool),
    List(Vec<Value>),
    Record(BTreeMap<String, Value>),
    Null,
    /// Unsigned or fractional JSON numbers; never implicitly coerced to an integer.
    Number(serde_json::Number),
    Reference(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Connection {
    Start,
    Then,
    Pipe,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
    pub operation: String,
    /// A quoted executable name bypasses the named applet catalog.
    #[serde(default, skip_serializing_if = "is_false")]
    pub direct: bool,
    /// Attach native stderr to the operating system's null device.
    #[serde(default, skip_serializing_if = "is_false")]
    pub discard_stderr: bool,
    pub parameters: BTreeMap<String, Value>,
    pub binding: Option<String>,
    pub cache: bool,
    /// Explicitly retain a native exit code, including ordinary nonzero exits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_binding: Option<String>,
    /// Retain a successful structured execution result, including pipeline
    /// stage metadata and explicitly captured output bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_binding: Option<String>,
    /// `and retry up to N times`: run again after a declared retryable failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
}

/// A declared retry for one command. Retries happen only for outcomes the
/// contract marks retryable (or a nonzero exit of a direct executable), and only
/// when the effects are retry-safe or the step says `even if uncertain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Retries after the first attempt.
    pub retries: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub even_if_uncertain: bool,
    /// Delay before the first retry, in milliseconds. Absent means no delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_ms: Option<u64>,
    /// Double the delay before each later retry.
    #[serde(default, skip_serializing_if = "is_false")]
    pub doubling: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A typed comparison. Equality supports JSON values; containment is limited to
/// strings, arrays, and object keys and never performs implicit coercion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOperator {
    #[default]
    Equals,
    Contains,
}

/// Typed comparison; there is no shell expansion or implicit truthiness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub left: Value,
    pub right: Value,
    pub negated: bool,
    /// Omitted from legacy serialized ASTs when it has the old equality meaning.
    #[serde(default, skip_serializing_if = "is_equals_operator")]
    pub operator: ConditionOperator,
}

fn is_equals_operator(operator: &ConditionOperator) -> bool {
    *operator == ConditionOperator::Equals
}

/// A bounded loop; values resolve in the enclosing lexical scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LoopKind {
    Repeat {
        count: Value,
    },
    ForEach {
        binding: String,
        values: Value,
    },
    While {
        condition: Condition,
    },
    /// `while run <command> succeeds|fails`: the command runs at each header.
    WhileCommand {
        call: Invocation,
        expect_success: bool,
    },
}

/// How a finite task group combines independently invoked applets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGroupMode {
    /// Start every task subject to the configured bound and require success.
    Join,
    /// Return the first successful task and cancel/reap all remaining tasks.
    Race,
}

/// `parallel for each <binding> in <values>`: the group's single template task
/// runs once per element, with the element bound to `binding`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEach {
    pub binding: String,
    pub values: Value,
}

/// One native command in a named background pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobStage {
    pub line: usize,
    pub call: Invocation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StepKind {
    /// Bounded, structured fan-out of independent named applet calls.
    TaskGroup {
        mode: TaskGroupMode,
        limit: usize,
        binding: String,
        tasks: Vec<Invocation>,
        /// Present for `parallel for each`; `tasks` then holds one template.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        each: Option<TaskEach>,
    },
    /// Start one owned native pipeline and retain its handle in the current
    /// finite or reusable-program scope.
    SpawnJob {
        name: String,
        stages: Vec<JobStage>,
    },
    /// Read a nonblocking snapshot of a named job into a JSON binding.
    InspectJob {
        name: String,
        binding: String,
    },
    /// Join a named job and optionally bind its terminal status and record.
    AwaitJob {
        name: String,
        status_binding: Option<String>,
        result_binding: Option<String>,
    },
    /// Request cancellation and wait for the named job's process tree to reap.
    CancelJob {
        name: String,
    },
    /// `end` is relative to the header and points just beyond EndLoop.
    Loop {
        kind: LoopKind,
        end: usize,
    },
    /// `back` is a backward offset from this marker to its Loop header.
    EndLoop {
        back: usize,
    },
    /// Parser-lowered forward offsets, relative to this statement. `end` points
    /// just beyond EndIf; `otherwise` points to its body, or equals `end`.
    If {
        condition: Condition,
        otherwise: usize,
        end: usize,
    },
    /// `if run <command> succeeds|fails`. Same offsets as `If`; the command runs
    /// with closed stdin and its output is discarded, and the pipeline value is
    /// unchanged. `expect_success` selects the branch taken on a match.
    IfCommand {
        call: Invocation,
        expect_success: bool,
        otherwise: usize,
        end: usize,
    },
    Otherwise {
        end: usize,
    },
    EndIf,
    /// Expanded finite program body; end points just beyond its EndCall.
    CallProgram {
        call: Invocation,
        inputs: Vec<InputDeclaration>,
        end: usize,
        connection: Connection,
    },
    EndCall,
    /// Exit the innermost active loop, restoring its outer lexical bindings.
    BreakLoop,
    /// End the current iteration of the innermost active loop.
    ContinueLoop,
    /// Exit the current reusable program call after unwinding its local scopes.
    Return,
    /// End the whole finite run with an explicit status (0-255) after running
    /// pending finally blocks and reaping owned jobs. Not catchable.
    Exit {
        status: Value,
    },
    /// Write the message to stderr and end the whole finite run. The status
    /// defaults to 1 and must be 1-255.
    Fail {
        message: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<Value>,
    },
    /// Structured error and cleanup block. Offsets are relative to this header;
    /// catch/finally point at their marker steps and end points past EndTry.
    Try {
        catch: Option<usize>,
        finally: Option<usize>,
        end: usize,
    },
    Catch {
        binding: String,
        end: usize,
    },
    Finally,
    EndTry,
    /// `with temporary file|directory as NAME`: binds NAME to a fresh private path
    /// that is removed when the body ends, on every exit from the run. `end` is the
    /// offset from this step to the matching EndTempScope, past it.
    TempScope {
        kind: TempKind,
        binding: String,
        end: usize,
    },
    /// Closes a TempScope. `back` is the offset from this step to its TempScope.
    EndTempScope {
        back: usize,
    },
    /// A reusable body's first `read from stdin` reads its call input.
    ReadCallInput {
        binding: Option<String>,
    },
    UseFolder {
        path: Value,
    },
    /// Sets the pipeline input to a text value's UTF-8 bytes; launches no process.
    UseValue {
        source: Value,
    },
    /// `then capture bytes as NAME`: binds the pipeline bytes, unchanged and unchecked for
    /// UTF-8, to a byte binding; the pipeline continues with the same bytes.
    CaptureBytes {
        binding: String,
    },
    /// `use bytes NAME`: sets the pipeline input to a byte binding's bytes.
    UseBytes {
        binding: String,
    },
    Read {
        path: Value,
        binding: Option<String>,
    },
    ReadStdin {
        binding: Option<String>,
    },
    ParseJson {
        binding: String,
    },
    TakeJson {
        pointer: String,
        source: Value,
        kind: InputType,
        binding: String,
    },
    /// Dynamic JSON pointer variant, used only when the pointer is a text result.
    /// Keeping the literal variant above preserves serialized AST compatibility.
    TakeJsonDynamic {
        pointer: Value,
        source: Value,
        kind: InputType,
        binding: String,
    },
    EncodeJson {
        source: Value,
    },
    Invoke {
        call: Invocation,
        connection: Connection,
    },
    Save {
        path: Value,
        connection: Connection,
    },
    /// `then append to <value>`: append the pipeline value to a file, keeping it
    /// as the pipeline value. Unlike `save`, the file is not replaced.
    Append {
        path: Value,
    },
    /// Per-command environment for the next native command: `set` values are text
    /// literals or text bindings, and `unset` names are removed first. It must
    /// immediately precede a `Redirect` or the command itself.
    Environment {
        set: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unset: Vec<String>,
    },
    /// `interactively`: the next command takes the terminal for its run (terminal
    /// handoff). It must immediately precede one fresh, unpiped native command.
    Interactive,
    /// `hand over to "program" ...`: the next command replaces this script. It must
    /// be the last statement of the program and precede one fresh native command.
    HandOver,
    /// `launch detached "program" ...`: the next command starts unowned. When `output`
    /// is given it is a write target like a redirect: its stdout and stderr go to that
    /// file, which is truncated. It must precede one fresh native command.
    LaunchDetached {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<Value>,
    },
    /// Opens a top-level `on interrupt` body inside its cleanup block. The body runs only
    /// while a drain is in progress; otherwise control jumps to `end` (relative).
    OnInterrupt {
        end: usize,
    },
    /// Routes the next command's stderr or stdout. It must immediately precede
    /// one `Invoke` that starts a fresh, unpiped command.
    Redirect {
        target: RedirectTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<Value>,
    },
}

/// What a temporary scope creates: an empty file or an empty directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TempKind {
    File,
    Directory,
}

/// Where a `Redirect` step sends the next command's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RedirectTarget {
    /// `and stderr to <path>`: replace the file with the command's stderr.
    StderrFile,
    /// `and stderr appended to <path>`: append the command's stderr to the file.
    StderrAppend,
    /// `and stderr merged`: add the command's stderr after its stdout.
    StderrMerged,
    /// `and stdout to stderr`: write the command's stdout to this stderr.
    StdoutToStderr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub line: usize,
    pub kind: StepKind,
    /// The `fetch:` label written before the statement; it identifies the statement's
    /// first step across edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Stable id for each step, in order: its label when present, else a derived id made of
/// the step kind, a short hash of its canonical JSON and its occurrence among identical
/// steps. Line numbers never enter a derived id, so inserting a statement above a step
/// changes only the derived ids of identical steps, and never a labelled id.
pub fn step_ids(steps: &[Step]) -> Vec<String> {
    use sha2::{Digest, Sha256};
    let mut occurrences: BTreeMap<String, usize> = BTreeMap::new();
    steps
        .iter()
        .map(|step| {
            if let Some(label) = &step.label {
                return label.clone();
            }
            let canonical = serde_json::to_string(&step.kind).unwrap_or_default();
            let kind = match serde_json::to_value(&step.kind) {
                Ok(serde_json::Value::Object(map)) => map.keys().next().cloned(),
                Ok(serde_json::Value::String(name)) => Some(name),
                _ => None,
            }
            .unwrap_or_else(|| "step".to_owned());
            let digest = Sha256::digest(canonical.as_bytes());
            let short: String = digest
                .iter()
                .take(6)
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let key = format!("{kind}:{short}");
            let occurrence = occurrences.entry(key.clone()).or_insert(0);
            *occurrence += 1;
            format!("{key}#{occurrence}")
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    #[serde(default, skip_serializing_if = "definitions_empty")]
    pub definitions: Arc<BTreeMap<String, ReusableProgram>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputDeclaration>,
    pub steps: Vec<Step>,
    pub trigger: Option<Trigger>,
}

fn definitions_empty(definitions: &Arc<BTreeMap<String, ReusableProgram>>) -> bool {
    definitions.is_empty()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReusableProgram {
    pub inputs: Vec<InputDeclaration>,
    pub steps: Vec<Step>,
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputType {
    Text,
    Integer,
    Boolean,
    TextList,
    Json,
    /// A raw byte capture (`then capture bytes as NAME`). Never valid as text.
    Bytes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputDeclaration {
    pub name: String,
    pub kind: InputType,
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<InputSource>,
    pub line: usize,
    /// Human-readable text from `described as "TEXT"`, shown by inspect and help.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Allowed values from `one of [...]` on a text input, in declaration order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputSource {
    Environment(String),
    Argument(usize),
    Arguments,
    /// `from program directory`: the canonical directory of the program file.
    /// The loader supplies it as the input's default, so a `--input` still wins.
    ProgramDirectory,
    /// `from program path`: the canonical path of the program file.
    ProgramPath,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Trigger {
    Every {
        interval_ms: u64,
        line: usize,
    },
    Cron {
        expression: String,
        line: usize,
    },
    Listen {
        source: ListenSource,
        binding: String,
        line: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ListenSource {
    Stdin,
    Applet(Invocation),
}
