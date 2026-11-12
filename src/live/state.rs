use crate::ast::{Program, StepKind, Trigger, Value};
use crate::manifest::Registry;
use crate::runtime::{self, live::NodeState};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value as Json};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};

pub(crate) const MAX_MESSAGE: usize = 1024 * 1024;
pub(crate) const MAX_STATE: usize = 8 * 1024 * 1024;
pub(crate) const MAX_QUEUE: usize = 128;

// Count the exact serialized representation without allocating another state-sized
// buffer. Stop as soon as the budget is exceeded, including JSON escaping.
fn serialized_within(value: &impl Serialize, limit: usize) -> Result<bool> {
    struct Counter {
        remaining: usize,
        exceeded: bool,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                self.exceeded = true;
                return Err(std::io::Error::other("serialized state exceeds budget"));
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        remaining: limit,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut counter, value);
    if counter.exceeded {
        return Ok(false);
    }
    result?;
    Ok(true)
}

pub(super) fn within_state_limit(doc: &Document) -> Result<bool> {
    serialized_within(doc, MAX_STATE)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum Data {
    Text { text: String },
    Bytes { bytes: Vec<u8> },
}
impl Default for Data {
    fn default() -> Self {
        Self::Text {
            text: String::new(),
        }
    }
}
impl Data {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => Self::Text { text },
            Err(error) => Self::Bytes {
                bytes: error.into_bytes(),
            },
        }
    }
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Text { text } => text.as_bytes().to_vec(),
            Self::Bytes { bytes } => bytes.clone(),
        }
    }
    pub fn len(&self) -> usize {
        match self {
            Self::Text { text } => text.len(),
            Self::Bytes { bytes } => bytes.len(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    pub source: String,
    pub cwd: PathBuf,
    pub bindings: BTreeMap<String, Json>,
    pub input: Data,
    pub cursor: usize,
    pub inbox: VecDeque<Data>,
    pub user: Map<String, Json>,
    #[serde(default)]
    pub ui: Json,
}

#[derive(Clone)]
pub(crate) struct Plan {
    pub program: Program,
    pub stream: Option<crate::stream::StreamProgram>,
    pub body_start: usize,
    pub screen: Option<crate::ui::syntax::Screen>,
}
#[cfg(test)]
thread_local! {
    /// Test-only count of `Plan::parse` calls on this thread, so tests can prove how
    /// often a source is parsed.
    static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_parse() {
    PARSES.with(|count| count.set(count.get() + 1));
}

impl Plan {
    pub fn parse(source: &str) -> Result<Self> {
        #[cfg(test)]
        count_parse();
        if source.len() > 65536 {
            bail!("source exceeds 64 KiB");
        }
        if crate::ui::syntax::is_screen(source) {
            return Ok(Self {
                program: Program {
                    definitions: Default::default(),
                    inputs: Vec::new(),
                    steps: Vec::new(),
                    output: None,
                    trigger: None,
                },
                stream: None,
                body_start: 0,
                screen: Some(crate::ui::syntax::parse(source)?),
            });
        }
        if crate::stream::is_stream(source) {
            let stream = crate::stream::parse(source)?;
            return Ok(Self {
                program: stream.pipeline.clone(),
                stream: Some(stream),
                body_start: 0,
                screen: None,
            });
        }
        let program = crate::parser::parse(source)?;
        let line = program.trigger.as_ref().map(|trigger| match trigger {
            Trigger::Every { line, .. }
            | Trigger::Cron { line, .. }
            | Trigger::Listen { line, .. } => *line,
        });
        let body_start = line.map_or(0, |line| {
            program
                .steps
                .iter()
                .take_while(|step| step.line < line)
                .count()
        });
        Ok(Self {
            program,
            stream: None,
            body_start,
            screen: None,
        })
    }
    pub fn locals(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut index = 0;
        while let Some(step) = self.program.steps.get(index) {
            match &step.kind {
                // A reusable call's body is callee-local; only the names the
                // call returns to this scope are inventoried.
                StepKind::CallProgram { call, end, .. } => {
                    push_call_names(call, &mut names);
                    index += end;
                    continue;
                }
                // A loop body runs in this scope. Its writes are either outer
                // names, which reset must restore, or iteration-local names,
                // which reset removes when unseeded. Walk the body like other steps.
                StepKind::Loop {
                    kind: crate::ast::LoopKind::ForEach { binding, .. },
                    ..
                } => names.push(binding.clone()),
                StepKind::Read { binding, .. } | StepKind::ReadStdin { binding } => {
                    names.extend(binding.iter().cloned())
                }
                StepKind::ParseJson { binding }
                | StepKind::TakeJson { binding, .. }
                | StepKind::TakeJsonDynamic { binding, .. }
                | StepKind::BindValue { binding, .. }
                | StepKind::CaptureBytes { binding }
                | StepKind::InspectJob { binding, .. }
                | StepKind::Catch { binding, .. }
                | StepKind::TempScope { binding, .. } => names.push(binding.clone()),
                StepKind::AwaitJob {
                    status_binding,
                    result_binding,
                    ..
                } => {
                    names.extend(status_binding.iter().cloned());
                    names.extend(result_binding.iter().cloned());
                }
                StepKind::TaskGroup {
                    binding,
                    tasks,
                    each,
                    ..
                } => {
                    names.push(binding.clone());
                    names.extend(each.iter().map(|each| each.binding.clone()));
                    for call in tasks {
                        push_call_names(call, &mut names);
                    }
                }
                StepKind::SpawnJob { stages, .. } => {
                    for stage in stages {
                        push_call_names(&stage.call, &mut names);
                    }
                }
                StepKind::Invoke { call, .. } => push_call_names(call, &mut names),
                _ => {}
            }
            index += 1;
        }
        if let Some(Trigger::Listen { binding, .. }) = &self.program.trigger {
            names.push(binding.clone());
        }
        names
    }
    pub fn end(&self, cursor: usize) -> usize {
        runtime::live::node_end(&self.program, cursor)
    }
}

/// The names a native or program call writes into the scope that invokes it.
fn push_call_names(call: &crate::ast::Invocation, names: &mut Vec<String>) {
    names.extend(call.binding.iter().cloned());
    names.extend(call.status_binding.iter().cloned());
    names.extend(call.result_binding.iter().cloned());
}

/// Reset or restart: each script-written name returns to its startup value, or is
/// removed when the startup seed does not provide one. Other bindings stay put.
fn restore_locals(
    bindings: &mut BTreeMap<String, Json>,
    locals: &[String],
    initial: &BTreeMap<String, Json>,
) {
    for name in locals {
        match initial.get(name) {
            Some(value) => {
                bindings.insert(name.clone(), value.clone());
            }
            None => {
                bindings.remove(name);
            }
        }
    }
}

pub(crate) fn argument(value: &Json) -> Value {
    match value {
        Json::String(text) => Value::String(text.clone()),
        Json::Bool(boolean) => Value::Bool(*boolean),
        Json::Number(number) if number.as_i64().is_some() => {
            Value::Integer(number.as_i64().unwrap())
        }
        Json::Array(values) => Value::List(values.iter().map(argument).collect()),
        Json::Object(values) => Value::Record(
            values
                .iter()
                .map(|(key, value)| (key.clone(), argument(value)))
                .collect(),
        ),
        Json::Null => Value::Null,
        Json::Number(number) => Value::Number(number.clone()),
    }
}
pub(crate) fn plain(value: Value) -> Json {
    match value {
        Value::String(text) => json!(text),
        Value::Integer(n) => json!(n),
        Value::Bool(b) => json!(b),
        Value::List(values) => Json::Array(values.into_iter().map(plain).collect()),
        Value::Reference(name) => json!(name),
        Value::Record(values) => Json::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, plain(value)))
                .collect(),
        ),
        Value::Null => Json::Null,
        Value::Number(number) => Json::Number(number),
    }
}
impl Document {
    // The caller holds the state mutex. Roll back before returning any error;
    // no observer can see the temporary candidate, and unrelated fields stay put.
    pub fn enqueue(&mut self, record: Data) -> Result<bool> {
        self.inbox.push_back(record);
        let result = within_state_limit(self);
        if !matches!(result, Ok(true)) {
            self.inbox.pop_back();
        }
        result
    }
    // The caller holds the state mutex. The event is pushed onto the queue in place and
    // popped again if `check` rejects the edited document, so an event never copies the
    // rest of the document (sources, bindings, input, inbox, user data).
    pub fn queue_ui_event(
        &mut self,
        event: Option<Json>,
        check: impl FnOnce(&Document) -> Result<()>,
    ) -> Result<()> {
        let events = self
            .ui
            .get_mut("events")
            .and_then(Json::as_array_mut)
            .context("missing UI events queue")?;
        if events.len() >= MAX_QUEUE {
            bail!("UI event queue is full");
        }
        events.push(event.context("event requires value")?);
        if let Err(error) = check(self) {
            if let Some(events) = self.ui.get_mut("events").and_then(Json::as_array_mut) {
                events.pop();
            }
            return Err(error);
        }
        Ok(())
    }
    pub fn replace_ui(&mut self, ui: Json) -> Result<bool> {
        let previous = std::mem::replace(&mut self.ui, ui);
        let result = within_state_limit(self);
        if !matches!(result, Ok(true)) {
            self.ui = previous;
        }
        result
    }
    pub fn replace_input(&mut self, data: Data) -> Result<()> {
        let previous = std::mem::replace(&mut self.input, data);
        let result = within_state_limit(self);
        if !matches!(result, Ok(true)) {
            self.input = previous;
        }
        if !result? {
            bail!("state exceeds 8 MiB");
        }
        Ok(())
    }
    pub fn begin_record(&mut self, binding: &str) -> Result<()> {
        let record = self.inbox.pop_front().context("listener inbox is empty")?;
        let text = match &record {
            Data::Text { text } => text.clone(),
            // Preserve the existing rejection policy: consume invalid UTF-8 so
            // a later valid record can recover after an explicit resume.
            Data::Bytes { bytes } => std::str::from_utf8(bytes)
                .context("listener records must be UTF-8 text")?
                .to_owned(),
        };
        let previous_input = std::mem::replace(&mut self.input, record);
        let previous_binding = self.bindings.insert(binding.to_owned(), Json::String(text));
        let result = within_state_limit(self);
        if !matches!(result, Ok(true)) {
            let record = std::mem::replace(&mut self.input, previous_input);
            self.inbox.push_front(record);
            if let Some(previous) = previous_binding {
                self.bindings.insert(binding.to_owned(), previous);
            } else {
                self.bindings.remove(binding);
            }
        }
        if !result? {
            bail!("state exceeds 8 MiB");
        }
        Ok(())
    }
    pub fn node_state(&self) -> NodeState {
        NodeState {
            cwd: self.cwd.clone(),
            bindings: self
                .bindings
                .iter()
                .map(|(key, value)| (key.clone(), argument(value)))
                .collect(),
            data: self.input.bytes(),
        }
    }
    pub fn apply_node(&mut self, state: NodeState) {
        self.cwd = state.cwd;
        self.bindings = state
            .bindings
            .into_iter()
            .map(|(key, value)| {
                let json = self
                    .bindings
                    .get(&key)
                    .filter(|original| argument(original) == value)
                    .cloned()
                    .unwrap_or_else(|| plain(value));
                (key, json)
            })
            .collect();
        self.input = Data::from_bytes(state.data);
    }
}

/// `run_mode` is true for `execute run --control`, a finite run that ends; only it
/// may exit or fail. A served live program keeps its state and never ends.
pub(crate) fn validate(
    doc: &Document,
    registry: &Registry,
    max_bytes: usize,
    run_mode: bool,
) -> Result<Plan> {
    let plan = Plan::parse(&doc.source)?;
    validate_parsed(doc, &plan, registry, max_bytes, run_mode)?;
    Ok(plan)
}

/// `validate` for a plan that was already parsed from `doc.source`. The caller owns
/// that pairing: `State.plan` is parsed from `State.doc.source` and is replaced in the
/// same step as every write of the source (see the list at `State::plan`), so an event
/// that only queues UI input reuses it instead of parsing the source again.
fn validate_parsed(
    doc: &Document,
    plan: &Plan,
    registry: &Registry,
    max_bytes: usize,
    run_mode: bool,
) -> Result<()> {
    if !doc.cwd.is_absolute() || !doc.cwd.is_dir() || doc.cwd.to_str().is_none() {
        bail!("cwd must be an existing absolute directory");
    }
    if doc.input.len() > max_bytes
        || doc.inbox.len() > MAX_QUEUE
        || doc.inbox.iter().any(|v| v.len() > max_bytes)
    {
        bail!("input/record size or inbox capacity exceeded");
    }
    for name in doc.bindings.keys() {
        if name.is_empty()
            || matches!(name.as_str(), "true" | "false")
            || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            || name.as_bytes()[0].is_ascii_digit()
        {
            bail!(
                "invalid binding name {name:?}; use letters, digits and underscores, starting with a letter or underscore"
            );
        }
    }
    if registry
        .host_state::<Option<crate::capabilities::CapabilityPolicy>>()
        .is_some()
    {
        if let Some(screen) = &plan.screen {
            let policy_program = Program {
                definitions: Default::default(),
                inputs: Vec::new(),
                steps: screen
                    .components
                    .values()
                    .flat_map(|component| &component.handlers)
                    .flat_map(|handler| &handler.actions)
                    .filter_map(|action| match action {
                        crate::ui::syntax::Action::Native(step) => Some(step.clone()),
                        _ => None,
                    })
                    .collect(),
                output: None,
                trigger: None,
            };
            crate::capabilities::preflight(&policy_program, registry, &doc.cwd)?;
        } else {
            // `Plan::program` is the full prospective program, including
            // definitions and service triggers, even when the cursor is later.
            crate::capabilities::preflight(&plan.program, registry, &doc.cwd)?;
        }
    }
    crate::inputs::validate(&plan.program.inputs)?;
    let mut declared = BTreeMap::new();
    for input in &plan.program.inputs {
        let value = doc.bindings.get(&input.name).with_context(|| {
            format!(
                "missing declared input {:?}; supply /bindings/{} with the source",
                input.name, input.name
            )
        })?;
        declared.insert(input.name.clone(), input.parse_json(value)?);
    }
    if let Some(screen) = &plan.screen {
        crate::ui::syntax::validate(screen, registry)?;
        crate::ui::validate_live_state(screen, &doc.ui)?;
        if doc.cursor != 0 || !doc.bindings.is_empty() {
            bail!(
                "UI state uses component inputs/widgets; cursor must be 0 and root bindings must be empty"
            );
        }
        if !within_state_limit(doc)? {
            bail!("state exceeds 8 MiB");
        }
        return Ok(());
    }
    if !doc.ui.is_null() {
        bail!("ui state requires a screen program");
    }
    if doc.cursor > plan.program.steps.len() {
        bail!("cursor is past the end of the program");
    }
    let mut index = 0;
    while index < doc.cursor {
        index = plan.end(index);
    }
    if index != doc.cursor {
        bail!("cursor must be at a native pipeline or control-block boundary");
    }
    if let Some(stream) = &plan.stream {
        runtime::preflight_inputs(&stream.pipeline, registry, &declared)?;
        crate::stream::validate_with_inputs(
            stream,
            registry,
            doc.bindings.keys().cloned().collect(),
        )?;
        if doc.cursor != 0 && doc.cursor != plan.program.steps.len() {
            bail!("a stream is one indivisible native pipeline");
        }
    } else {
        let mut names: std::collections::BTreeSet<String> = doc.bindings.keys().cloned().collect();
        if let Some(trigger) = &plan.program.trigger {
            match trigger {
                Trigger::Every { interval_ms, .. }
                    if *interval_ms == 0
                        || std::time::Instant::now()
                            .checked_add(std::time::Duration::from_millis(*interval_ms))
                            .is_none() =>
                {
                    bail!("interval must be positive")
                }
                Trigger::Cron { expression, .. } => {
                    crate::schedule::CronSchedule::parse(expression)?;
                }
                Trigger::Listen {
                    binding, source, ..
                } => {
                    if let crate::ast::ListenSource::Applet(call) = source {
                        let producer = crate::stream::StreamProgram {
                            pipeline: Program {
                                definitions: Default::default(),
                                inputs: plan.program.inputs.clone(),
                                steps: vec![crate::ast::Step {
                                    line: 0,
                                    label: None,
                                    kind: StepKind::Invoke {
                                        call: call.clone(),
                                        connection: crate::ast::Connection::Start,
                                    },
                                }],
                                output: None,
                                trigger: None,
                            },
                            line_limit: None,
                        };
                        crate::stream::validate_with_inputs(
                            &producer,
                            registry,
                            doc.bindings.keys().cloned().collect(),
                        )?;
                        runtime::preflight_inputs(&producer.pipeline, registry, &declared)?;
                    }
                    names.insert(binding.clone());
                }
                _ => {}
            }
        }
        let tail = Program {
            definitions: plan.program.definitions.clone(),
            inputs: plan.program.inputs.clone(),
            steps: plan.program.steps[doc.cursor..].to_vec(),
            output: None,
            trigger: None,
        };
        // Live documents refuse hand-over and detached launch anywhere in the program.
        crate::runtime::handoff::reject_in_live(&plan.program)?;
        runtime::preflight_inputs(&tail, registry, &declared)?;
        runtime::validate_live(
            &tail,
            registry,
            names,
            doc.cursor > 0
                || !doc.bindings.is_empty()
                || doc.input.len() > 0
                || matches!(plan.program.trigger, Some(Trigger::Listen { .. })),
            run_mode,
        )?;
        for step in &plan.program.steps[..plan.body_start] {
            match &step.kind {
                StepKind::UseFolder { .. } => {}
                StepKind::Invoke { call, .. }
                    if registry.resolve_call(call)?.builtin
                        == Some(crate::manifest::Builtin::Cd) => {}
                _ => bail!("service setup requires a folder directive or cd builtin"),
            }
        }
    }
    if !within_state_limit(doc)? {
        bail!("state exceeds 8 MiB");
    }
    Ok(())
}

/// How a run-mode program ended: the status it reports to the process, and the
/// `fail with message` text, if any.
#[derive(Clone, Debug)]
pub(crate) struct ExitRecord {
    pub status: i32,
    pub message: Option<String>,
}

pub(crate) struct State {
    pub doc: Document,
    pub job_session: Arc<runtime::live::JobSession>,
    pub redactor: Arc<crate::security::SecretRedactor>,
    pub registry: Registry,
    /// Set by a run-mode program's `exit with`/`fail with`, or by a failed command.
    pub exit: Option<ExitRecord>,
    /// Breakpoints and the statement a run is paused before.
    pub debug: super::debug::Control,
    /// `Plan::parse(&doc.source)`, kept as a cache of the source. Every write of
    /// `doc.source` replaces this in the same step, or writes the same text back:
    /// the start in `mod.rs` (`serve`, from `validate`), the edit in `request`
    /// (`create`/`update`/`delete`/`patch`, which parses the candidate source and
    /// sets this plan only after validation), and the reset in `request`, which
    /// clones the document and keeps its source. The worker in `worker.rs` clones
    /// the document and applies a node's bindings and input, never the source.
    pub plan: Plan,
    pub revision: u64,
    pub running: bool,
    pub in_flight: bool,
    pub status: String,
    pub uncertain: bool,
    pub error: Option<String>,
    pub active: Option<(usize, usize)>,
    pub step_budget: Option<usize>,
    pub cycle: bool,
    pub runs: usize,
    pub nodes: usize,
    pub launches: usize,
    pub cache_hits: usize,
    pub last_output: Data,
    pub active_cancel: Arc<AtomicBool>,
    pub producer_cancel: Arc<AtomicBool>,
    pub producer_done: bool,
    pub producer_error: Option<String>,
    pub producer_epoch: u64,
    pub events: VecDeque<Json>,
    pub ui_epoch: u64,
    pub ui_dirty: bool,
    pub ui_frame: String,
    pub ui_applied_revision: u64,
    pub ui_errors: Json,
}
impl State {
    pub fn changed(&mut self, event: &str) {
        self.revision += 1;
        if self.events.len() == 64 {
            self.events.pop_front();
        }
        self.events.push_back(json!({"revision":self.revision,"event":event,"cursor":self.doc.cursor,"status":self.status}));
    }
    fn runtime(&self, output: bool) -> Json {
        let error = self
            .error
            .as_deref()
            .map(|text| self.redactor.redact_text(text));
        let producer_error = self
            .producer_error
            .as_deref()
            .map(|text| self.redactor.redact_text(text));
        let mut value = json!({"pid":std::process::id(),"execute_version":env!("CARGO_PKG_VERSION"),"status":self.status,"running":self.running,"in_flight":self.in_flight,"active_node":self.active,"effects_may_be_incomplete":self.uncertain,"error":error,"runs_completed":self.runs,"nodes_completed":self.nodes,"completed_native_launches":self.launches,"cache_hits":self.cache_hits,"producer_done":self.producer_done,"producer_error":producer_error,"frame":self.ui_frame,"ui_applied_revision":self.ui_applied_revision,"ui_errors":self.ui_errors});
        value["debug"] = self.debug.json();
        if output {
            value["last_output"] = self.last_output_json();
        }
        value
    }
    pub fn snapshot(&self) -> Json {
        json!({"state":self.masked_document(),"runtime":self.runtime(true),"events":self.events})
    }
    /// The names of the program's secret prompt inputs. Their answers are withheld from
    /// every read, as records withhold them.
    pub(crate) fn secret_names(&self) -> Vec<String> {
        crate::runtime::sensitive_binding_names(&self.plan.program, &self.registry)
            .into_iter()
            .collect()
    }
    fn last_output_json(&self) -> Json {
        if self.secret_names().is_empty() {
            json!(self.last_output)
        } else {
            withheld_marker()
        }
    }
    /// The state document with every secret answer replaced by the withheld marker.
    fn masked_document(&self) -> Json {
        let mut value = serde_json::to_value(&self.doc).unwrap_or(Json::Null);
        value["source"] = json!(self.redactor.redact_text(&self.doc.source));
        let masked = withhold_secret_bindings(&self.doc.bindings, &self.secret_names());
        value["bindings"] = json!(masked);
        if !self.secret_names().is_empty() {
            value["input"] = withheld_marker();
        }
        value
    }
    fn ack(&self) -> Json {
        json!({"ok":true,"revision":self.revision,"result":{"status":self.status,"running":self.running,"in_flight":self.in_flight,"cursor":self.doc.cursor,"effects_may_be_incomplete":self.uncertain}})
    }
    fn read(&self, path: Option<&str>) -> Result<Json> {
        let parts = tokens(path.unwrap_or(""))?;
        if parts.is_empty() {
            return Ok(self.snapshot());
        }
        let (value, offset) = match parts[0].as_str() {
            "runtime" => {
                if parts.get(1).is_some_and(|part| part == "last_output") {
                    (self.last_output_json(), 2)
                } else {
                    (self.runtime(parts.len() == 1), 1)
                }
            }
            "events" => (json!(self.events), 1),
            "bindings" if parts.len() > 1 => {
                let value = self
                    .doc
                    .bindings
                    .get(&parts[1])
                    .cloned()
                    .ok_or_else(|| fail("not_found", "path does not exist"))?;
                let withheld = self.secret_names().contains(&parts[1]);
                (if withheld { withheld_marker() } else { value }, 2)
            }
            "user" if parts.len() > 1 => (
                self.doc
                    .user
                    .get(&parts[1])
                    .cloned()
                    .ok_or_else(|| fail("not_found", "path does not exist"))?,
                2,
            ),
            "inbox" if parts.len() > 1 => {
                let index = array_index(&parts[1], self.doc.inbox.len(), false)?;
                (json!(self.doc.inbox[index]), 2)
            }
            "bindings" => (
                json!(withhold_secret_bindings(
                    &self.doc.bindings,
                    &self.secret_names()
                )),
                1,
            ),
            "user" => (json!(self.doc.user), 1),
            "inbox" => (json!(self.doc.inbox), 1),
            "input" => (
                if self.secret_names().is_empty() {
                    json!(self.doc.input)
                } else {
                    withheld_marker()
                },
                1,
            ),
            "ui" => {
                return self
                    .doc
                    .ui
                    .pointer(&path.unwrap()[3..])
                    .cloned()
                    .ok_or_else(|| fail("not_found", "path does not exist"));
            }
            "source" => (json!(self.redactor.redact_text(&self.doc.source)), 1),
            "cwd" => (json!(self.doc.cwd), 1),
            "cursor" => (json!(self.doc.cursor), 1),
            _ => return Err(fail("not_found", "path does not exist")),
        };
        let mut selected = &value;
        for part in &parts[offset..] {
            selected = match selected {
                Json::Object(map) => map.get(part),
                Json::Array(array) => array_index(part, array.len(), false)
                    .ok()
                    .and_then(|index| array.get(index)),
                _ => None,
            }
            .ok_or_else(|| fail("not_found", "path does not exist"))?;
        }
        Ok(selected.clone())
    }
}
/// The value a read shows in place of a withheld secret answer.
pub(crate) fn withheld_marker() -> Json {
    json!({ "withheld": "secret" })
}

/// Whether a value is exactly the withheld marker. Any other value, including a JSON
/// object that merely has a `withheld` field, is an ordinary answer.
pub(crate) fn is_withheld_marker(value: &Json) -> bool {
    *value == withheld_marker()
}

/// A copy of `bindings` with each present secret answer replaced by the marker.
pub(crate) fn withhold_secret_bindings(
    bindings: &BTreeMap<String, Json>,
    secret: &[String],
) -> BTreeMap<String, Json> {
    let mut masked = bindings.clone();
    for name in secret {
        if masked.contains_key(name) {
            masked.insert(name.clone(), withheld_marker());
        }
    }
    masked
}

/// Put back the stored answer wherever a write carried the marker for a secret. A
/// marker with nothing stored is removed, so the marker is never kept as an answer.
pub(crate) fn restore_secret_bindings(
    candidate: &mut BTreeMap<String, Json>,
    stored: &BTreeMap<String, Json>,
    secret: &[String],
) {
    for name in secret {
        if candidate.get(name).is_some_and(is_withheld_marker) {
            match stored.get(name) {
                Some(answer) => {
                    candidate.insert(name.clone(), answer.clone());
                }
                None => {
                    candidate.remove(name);
                }
            }
        }
    }
}

#[cfg(test)]
mod locals_inventory_tests {
    use super::*;

    fn locals(source: &str) -> Vec<String> {
        Plan::parse(source).unwrap().locals()
    }

    fn has(names: &[String], name: &str) -> bool {
        names.iter().any(|candidate| candidate == name)
    }

    #[test]
    fn reusable_native_job_and_group_outputs_are_inventoried() {
        let names = locals(concat!(
            "define a program named tool\n",
            "  accept x as text\n",
            "  run \"printf\" with arguments [\"%s\", x] and keep status as callee_status\n",
            "run program tool with x \"v\" and keep status as reusable_status\n",
            "run \"printf\" with arguments [\"a\"] and keep status as native_status and keep result as native_result\n",
            "spawn as build\n",
            "  run \"printf\" with arguments [\"a\"]\n",
            "inspect build as build_state\n",
            "await build and keep status as build_status and keep result as build_result\n",
            "parallel for each item in [\"a\", \"b\"] at most 2 as results\n",
            "  run \"printf\" with arguments [\"%s\", item]\n",
        ));
        for name in [
            "reusable_status",
            "native_status",
            "native_result",
            "build_state",
            "build_status",
            "build_result",
            "results",
        ] {
            assert!(has(&names, name), "{name} missing from {names:?}");
        }
        assert!(
            !has(&names, "callee_status"),
            "callee-local names stay private: {names:?}"
        );
    }

    #[test]
    fn loop_body_writes_are_inventoried() {
        let names = locals(concat!(
            "repeat 2 times\n",
            "  run \"printf\" with arguments [\"a\"] and keep status as loop_status\n",
            "  run \"printf\" with arguments [\"b\"] and keep result as loop_result\n",
        ));
        assert!(has(&names, "loop_status"), "{names:?}");
        assert!(has(&names, "loop_result"), "{names:?}");
    }

    #[test]
    fn reset_restores_seeded_names_and_clears_unseeded_ones() {
        // `s` is seeded with 7 and a run left 0 in it; `extra` has no seed.
        // A user binding that no program writes must survive the reset.
        let mut bindings = BTreeMap::from([
            ("s".to_owned(), json!(0)),
            ("extra".to_owned(), json!(1)),
            ("note".to_owned(), json!("user")),
        ]);
        let initial = BTreeMap::from([("s".to_owned(), json!(7))]);
        let names = locals("run \"printf\" with arguments [\"a\"] and keep status as s\nrun \"printf\" with arguments [\"b\"] and keep status as extra\n");
        restore_locals(&mut bindings, &names, &initial);
        assert_eq!(bindings["s"], json!(7));
        assert!(!bindings.contains_key("extra"));
        assert_eq!(bindings["note"], json!("user"));
    }
}

#[cfg(test)]
mod secret_binding_tests {
    use super::*;

    fn bindings() -> BTreeMap<String, Json> {
        BTreeMap::from([
            ("token".to_owned(), json!("hunter2")),
            ("name".to_owned(), json!("ada")),
        ])
    }

    #[test]
    fn reads_withhold_only_present_secret_answers() {
        let masked =
            withhold_secret_bindings(&bindings(), &["token".to_owned(), "absent".to_owned()]);
        assert_eq!(masked["token"], withheld_marker());
        assert_eq!(masked["name"], json!("ada"));
        assert!(!masked.contains_key("absent"));
        assert!(!serde_json::to_string(&masked).unwrap().contains("hunter2"));
    }

    #[test]
    fn a_write_that_carries_the_marker_keeps_the_stored_answer() {
        let mut candidate = BTreeMap::from([
            ("token".to_owned(), withheld_marker()),
            ("name".to_owned(), json!("grace")),
        ]);
        restore_secret_bindings(&mut candidate, &bindings(), &["token".to_owned()]);
        assert_eq!(candidate["token"], json!("hunter2"));
        assert_eq!(
            candidate["name"],
            json!("grace"),
            "ordinary bindings are not touched"
        );
    }

    #[test]
    fn a_marker_with_nothing_stored_is_removed_not_kept() {
        let mut candidate = BTreeMap::from([("token".to_owned(), withheld_marker())]);
        restore_secret_bindings(&mut candidate, &BTreeMap::new(), &["token".to_owned()]);
        assert!(!candidate.contains_key("token"));
    }

    #[test]
    fn a_real_answer_written_by_a_client_is_accepted() {
        let mut candidate = BTreeMap::from([("token".to_owned(), json!("new-answer"))]);
        restore_secret_bindings(&mut candidate, &bindings(), &["token".to_owned()]);
        assert_eq!(candidate["token"], json!("new-answer"));
    }

    #[test]
    fn only_the_exact_sentinel_is_the_marker() {
        assert!(is_withheld_marker(&withheld_marker()));
        assert!(!is_withheld_marker(&json!({"withheld": "old"})));
        assert!(!is_withheld_marker(
            &json!({"withheld": "secret", "enabled": true})
        ));
        assert!(!is_withheld_marker(&json!("secret")));
    }

    #[test]
    fn secret_json_with_a_withheld_field_replaces_an_existing_answer() {
        let stored = BTreeMap::from([("config".to_owned(), json!({"withheld": "old"}))]);
        let new_value = json!({"withheld": "new", "enabled": true});
        let mut candidate = BTreeMap::from([("config".to_owned(), new_value.clone())]);
        restore_secret_bindings(&mut candidate, &stored, &["config".to_owned()]);
        assert_eq!(
            candidate["config"], new_value,
            "the requested change is kept"
        );
    }

    #[test]
    fn secret_json_with_a_withheld_field_is_accepted_when_nothing_is_stored() {
        let new_value = json!({"withheld": "new", "enabled": true});
        let mut candidate = BTreeMap::from([("config".to_owned(), new_value.clone())]);
        restore_secret_bindings(&mut candidate, &BTreeMap::new(), &["config".to_owned()]);
        assert_eq!(candidate["config"], new_value);
    }

    #[test]
    fn the_genuine_marker_keeps_a_stored_json_answer() {
        let stored = BTreeMap::from([("config".to_owned(), json!({"withheld": "old", "n": 1}))]);
        let mut candidate = BTreeMap::from([("config".to_owned(), withheld_marker())]);
        restore_secret_bindings(&mut candidate, &stored, &["config".to_owned()]);
        assert_eq!(candidate["config"], json!({"withheld": "old", "n": 1}));
    }

    #[test]
    fn masking_a_stored_json_answer_exposes_no_value() {
        let stored = BTreeMap::from([(
            "config".to_owned(),
            json!({"withheld": "old", "key": "hunter3"}),
        )]);
        let masked = withhold_secret_bindings(&stored, &["config".to_owned()]);
        assert_eq!(masked["config"], withheld_marker());
        let text = serde_json::to_string(&masked).unwrap();
        assert!(!text.contains("hunter3") && !text.contains("old"));
    }
}

#[cfg(test)]
mod flood_budget_tests {
    use super::*;

    fn at(start: std::time::Instant, millis: u64) -> std::time::Instant {
        start + std::time::Duration::from_millis(millis)
    }

    #[test]
    fn burst_admits_then_sheds_until_refill() {
        let mut budget = FloodBudget::new();
        let start = std::time::Instant::now();
        for _ in 0..FLOOD_BURST {
            assert!(budget.try_admit(start));
        }
        assert!(
            !budget.try_admit(start),
            "past the burst, non-priority sheds"
        );
        // A full second refills exactly SUSTAINED_ACCEPTS_PER_SECOND tokens.
        let later = at(start, 1000);
        for _ in 0..SUSTAINED_ACCEPTS_PER_SECOND {
            assert!(budget.try_admit(later));
        }
        assert!(!budget.try_admit(later));
    }

    #[test]
    fn refill_never_exceeds_the_burst() {
        let mut budget = FloodBudget::new();
        let start = std::time::Instant::now();
        assert!(budget.try_admit(start));
        // Even an hour of idleness refills only up to the burst cap.
        let much_later = at(start, 3_600_000);
        for _ in 0..FLOOD_BURST {
            assert!(budget.try_admit(much_later));
        }
        assert!(!budget.try_admit(much_later));
    }
}

#[cfg(test)]
mod patch_test_permission {
    use super::*;

    #[test]
    fn every_patch_test_requires_read_scope_regardless_of_json_pointer_path() {
        for path in [
            "", // document root
            "/inbox",
            "/inbox/0",
            "/inbox/0/text",
            "/ui",
            "/ui/events",
            "/ui/events/0/type",
            "/ui/events/~10", // escaped slash within a descendant token
            "/user/note",
            "/cwd",
            "/bindings/public",
            "/source",
        ] {
            let request = json!({"operations":[{"op":"test","path":path,"value":"guess"}]});
            assert!(patch_contains_test("patch", &request), "{path}");
        }

        for (operation, request) in [
            (
                "patch",
                json!({"operations":[{"op":"replace","path":"/inbox/0/text","value":"guess"}]}),
            ),
            ("update", json!({"path":"/inbox/0/text","value":"guess"})),
        ] {
            assert!(
                !patch_contains_test(operation, &request),
                "{operation}: {request}"
            );
        }
    }
}

/// Burst-tolerant flood budget for non-priority admissions on the live control
/// endpoint. A fresh server starts with [`FLOOD_BURST`] immediate non-priority
/// admissions — enough for a legitimate burst such as eight workers driving a
/// thousand back-to-back CRUD operations — and the budget refills at
/// [`SUSTAINED_ACCEPTS_PER_SECOND`] per second. Past the burst, a sustained
/// same-user flood of non-priority traffic sheds with `busy` instead of costing
/// a handler thread (Unix) or reaching policy/state work. Reserve-eligible
/// priority (`read`, `control stop` that may claim the reserve lane) never
/// spends the budget; fake-priority frames demoted to the general lane do,
/// like any other general-lane traffic. Partial connections that never
/// deliver a full request never spend it either.
pub(crate) const FLOOD_BURST: usize = 2048;
pub(crate) const SUSTAINED_ACCEPTS_PER_SECOND: u64 = 64;

/// Token bucket behind [`FLOOD_BURST`]/[`SUSTAINED_ACCEPTS_PER_SECOND`].
/// Checked under its lock in the accept loop on Unix (before any thread or
/// slot is claimed) and in the handler on Windows (after parsing, before
/// policy/state work). Refills lazily by the clock, so shedding lifts promptly
/// once a burst ends, and the balance is capped so the tracker itself stays
/// bounded however long a flood lasts.
#[derive(Debug)]
pub(crate) struct FloodBudget {
    tokens: f64,
    updated: std::time::Instant,
}

impl FloodBudget {
    pub(crate) fn new() -> Self {
        Self {
            tokens: FLOOD_BURST as f64,
            updated: std::time::Instant::now(),
        }
    }

    fn refill(&mut self, now: std::time::Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * SUSTAINED_ACCEPTS_PER_SECOND as f64)
                .min(FLOOD_BURST as f64);
            self.updated = now;
        }
    }

    /// Spend one token for a non-priority admission, refilling first by the
    /// clock. Returns false past the burst: the caller sheds with `busy`
    /// without spawning a handler or touching policy/state.
    pub(crate) fn try_admit(&mut self, now: std::time::Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub(crate) struct Shared {
    pub state: Mutex<State>,
    pub wake: Condvar,
    pub stop: Arc<AtomicBool>,
    pub registry: Registry,
    /// Burst-tolerant flood budget (see [`FloodBudget`]). Unix checks it in the
    /// accept loop before spawning; Windows checks it in the handler after
    /// parsing but before policy/state work. Reserve-eligible priority never
    /// spends it; demoted fake-priority frames do.
    pub flood: Mutex<FloodBudget>,
    pub max_bytes: usize,
    pub initial_cwd: PathBuf,
    pub initial_bindings: BTreeMap<String, Json>,
    pub initial_input: Data,
    pub headless: bool,
    /// `execute run --control`: stream uncaptured stdout to this terminal and allow
    /// the program to exit.
    pub run_mode: bool,
    /// The `--agent-policy` file the server started with, if any. `control
    /// reload-agents` re-reads this path so revocations and new grants take
    /// effect without restarting the server.
    pub agent_policy_path: Option<PathBuf>,
    /// Explicit `serve --allow-unauthenticated` opt-in. When false and no
    /// agent policy is installed, mutating live operations are refused.
    pub allow_unauthenticated: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub op: String,
    pub path: Option<String>,
    #[serde(default)]
    pub value: RequestValue,
    pub operations: Option<Vec<Json>>,
    pub action: Option<String>,
    pub if_revision: Option<u64>,
    pub after: Option<u64>,
    pub timeout_ms: Option<u64>,
}

/// A request value that keeps a missing field distinct from explicit JSON
/// null. `Option<Json>` cannot represent both because serde maps a JSON null
/// directly to `None`.
#[derive(Debug, Default)]
pub(crate) enum RequestValue {
    #[default]
    Missing,
    Present(Json),
}

impl RequestValue {
    fn as_ref(&self) -> Option<&Json> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }

    fn into_option(self) -> Option<Json> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }
}

impl<'de> Deserialize<'de> for RequestValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Json::deserialize(deserializer).map(Self::Present)
    }
}

#[derive(Debug)]
pub(crate) struct ApiError {
    pub code: &'static str,
    pub message: String,
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ApiError {}
fn fail(code: &'static str, message: &str) -> anyhow::Error {
    ApiError {
        code,
        message: message.into(),
    }
    .into()
}

fn tokens(path: &str) -> Result<Vec<String>> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if !path.starts_with('/') {
        bail!("path must be an RFC 6901 JSON pointer");
    }
    path[1..]
        .split('/')
        .map(|part| {
            let mut text = String::new();
            let mut chars = part.chars();
            while let Some(c) = chars.next() {
                if c != '~' {
                    text.push(c);
                    continue;
                }
                text.push(match chars.next() {
                    Some('0') => '~',
                    Some('1') => '/',
                    _ => bail!("invalid JSON pointer escape"),
                });
            }
            Ok(text)
        })
        .collect()
}
fn array_index(text: &str, len: usize, append: bool) -> Result<usize> {
    if append && text == "-" {
        return Ok(len);
    }
    if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
        bail!("array index must contain decimal digits");
    }
    if text.len() > 1 && text.starts_with('0') {
        bail!("array indices cannot have leading zeroes");
    }
    let index = text.parse::<usize>().context("invalid array index")?;
    if index > len || (!append && index == len) {
        bail!("array index is out of bounds");
    }
    Ok(index)
}
fn mutate(root: &mut Json, op: &str, path: &str, value: Option<Json>) -> Result<()> {
    let parts = tokens(path)?;
    if parts.is_empty() {
        bail!("replace individual state fields; the document root is fixed");
    }
    let mut parent = root;
    for part in &parts[..parts.len() - 1] {
        parent = match parent {
            Json::Object(map) => map.get_mut(part).context("path parent does not exist")?,
            Json::Array(array) => {
                let i = array_index(part, array.len(), false)?;
                &mut array[i]
            }
            _ => bail!("path parent is not a container"),
        };
    }
    let key = parts.last().unwrap();
    let value = || value.clone().context("operation requires value");
    match parent {
        Json::Object(map) => {
            let exists = map.contains_key(key);
            match op {
                "create" if exists => {
                    return Err(fail("already_exists", "create requires an absent path"));
                }
                "update" | "replace" | "remove" | "delete" | "test" if !exists => {
                    return Err(fail("not_found", "path does not exist"));
                }
                _ => {}
            }
            match op {
                "create" | "add" | "update" | "replace" => {
                    map.insert(key.clone(), value()?);
                }
                "remove" | "delete" => {
                    map.remove(key);
                }
                "test" => {
                    if map.get(key) != Some(&value()?) {
                        return Err(fail("test_failed", "patch test failed"));
                    }
                }
                _ => bail!("unsupported patch operation"),
            }
        }
        Json::Array(array) => {
            let insert = matches!(op, "create" | "add");
            let index = array_index(key, array.len(), insert)?;
            match op {
                "create" | "add" => array.insert(index, value()?),
                "update" | "replace" => array[index] = value()?,
                "remove" | "delete" => {
                    array.remove(index);
                }
                "test" => {
                    if array[index] != value()? {
                        return Err(fail("test_failed", "patch test failed"));
                    }
                }
                _ => bail!("unsupported patch operation"),
            }
        }
        _ => bail!("path parent is not a container"),
    }
    Ok(())
}

/// Whether a patch includes a `test` operation. Every test is an equality
/// oracle over state and therefore requires the same read permission as a
/// direct `read`, regardless of its path.
pub(crate) fn patch_contains_test(operation: &str, request: &Json) -> bool {
    if operation != "patch" {
        return false;
    }
    request
        .get("operations")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .any(|entry| entry.get("op").and_then(Json::as_str) == Some("test"))
}

/// A JSON Patch test must not compare against the stored value of a secret
/// binding. Reject the binding map itself (an ancestor of every secret), and
/// every path at or below a secret binding. `parts` has already been decoded
/// as an RFC 6901 pointer, so escaped binding names are handled consistently
/// with `mutate`.
fn test_targets_redacted_state(
    parts: &[String],
    secret_names: &[String],
    source_is_redacted: bool,
) -> bool {
    match parts.first().map(String::as_str) {
        // `/bindings` is an ancestor of every secret binding. A secret name
        // also protects the binding itself and every nested path below it.
        Some("bindings") => match parts.get(1) {
            None => true,
            Some(name) => secret_names.iter().any(|secret| secret == name),
        },
        // Reads replace the whole input record whenever the program contains
        // a secret binding, and source reads redact any registered secret text.
        Some("input") => !secret_names.is_empty(),
        Some("source") => source_is_redacted,
        _ => false,
    }
}

pub(crate) fn request(shared: &Shared, request: Request) -> Result<Json> {
    let mut state = shared.state.lock().unwrap();
    if request.op == "wait" {
        let after = request.after.context("wait requires after revision")?;
        let timeout = request.timeout_ms.unwrap_or(1000);
        if timeout > 10000 {
            bail!("wait timeout may not exceed 10000 ms");
        }
        if after > state.revision {
            bail!("after revision is ahead of the server");
        }
        let (guard, _) = shared
            .wake
            .wait_timeout_while(state, std::time::Duration::from_millis(timeout), |s| {
                s.revision <= after && !shared.stop.load(Ordering::Relaxed)
            })
            .unwrap();
        state = guard;
        return Ok(
            json!({"ok":true,"revision":state.revision,"result":{"changed":state.revision>after,"snapshot":state.read(request.path.as_deref())?}}),
        );
    }
    if request.op == "read" {
        let result = state.read(request.path.as_deref())?;
        return Ok(json!({"ok":true,"revision":state.revision,"result":result}));
    }
    if request
        .if_revision
        .is_some_and(|revision| revision != state.revision)
    {
        return Err(fail(
            "conflict",
            "revision changed; read current state before retrying",
        ));
    }
    if shared.stop.load(Ordering::Relaxed) {
        return Err(fail("stopping", "session is stopping"));
    }
    match request.op.as_str() {
        "control" => {
            match request
                .action
                .as_deref()
                .context("control requires action")?
            {
                "breakpoint" => {
                    state.debug.breakpoints = super::debug::lines(request.value.as_ref())?;
                }
                "continue" if state.debug.paused.is_none() => {
                    return Err(fail(
                        "not_paused",
                        "continue applies only to a run paused at a breakpoint",
                    ));
                }
                // Paused at a breakpoint: release the statement. `step` pauses again
                // before the next one; `continue` runs to the next breakpoint.
                "resume" | "step" | "continue" if state.debug.paused.is_some() => {
                    state.debug.stepping = request.action.as_deref() == Some("step");
                    state.debug.release = true;
                    shared.wake.notify_all();
                }
                "pause" => {
                    state.running = false;
                    if !state.in_flight {
                        state.status = "paused".into();
                    }
                }
                "cancel" => {
                    state.running = false;
                    state.active_cancel.store(true, Ordering::Relaxed);
                    if !state.in_flight {
                        state.status = "paused".into();
                    }
                }
                "stop" => {
                    shared.stop.store(true, Ordering::Relaxed);
                    state.running = false;
                    state.status = "stopping".into();
                    state.active_cancel.store(true, Ordering::Relaxed);
                    state.producer_cancel.store(true, Ordering::Relaxed);
                }
                "resume" | "step" | "retry" => {
                    if state.in_flight {
                        if state.plan.stream.is_some() && request.action.as_deref() != Some("retry")
                        {
                            state.running = true;
                            state.step_budget = if request.action.as_deref() == Some("step") {
                                Some(1)
                            } else {
                                None
                            };
                            state.changed("stream_resumed");
                            shared.wake.notify_all();
                            return Ok(state.ack());
                        }
                        return Err(fail(
                            "busy",
                            "wait for the active group to reach its boundary",
                        ));
                    }
                    if state.doc.cursor == state.plan.program.steps.len()
                        && state.plan.program.trigger.is_none()
                        && state.plan.screen.is_none()
                    {
                        return Err(fail(
                            "completed",
                            "edit cursor or use restart to run this completed program again",
                        ));
                    }
                    if state.uncertain && request.action.as_deref() != Some("retry") {
                        return Err(fail(
                            "uncertain_effects",
                            "use retry to explicitly authorize re-execution, or edit cursor to skip the failed group",
                        ));
                    }
                    validate(
                        &state.doc,
                        &shared.registry,
                        shared.max_bytes,
                        shared.run_mode,
                    )?;
                    state.uncertain = false;
                    state.error = None;
                    state.running = true;
                    state.status = "ready".into();
                    state.step_budget = if request.action.as_deref() == Some("step") {
                        Some(1)
                    } else {
                        None
                    };
                }
                "reset" | "restart" => {
                    if state.in_flight {
                        return Err(fail("busy", "pause/cancel and wait before resetting"));
                    }
                    let mut candidate = state.doc.clone();
                    restore_locals(
                        &mut candidate.bindings,
                        &state.plan.locals(),
                        &shared.initial_bindings,
                    );
                    candidate.cursor = 0;
                    candidate.cwd = shared.initial_cwd.clone();
                    candidate.input = shared.initial_input.clone();
                    if let Some(screen) = &state.plan.screen {
                        candidate.ui = crate::ui::initial_live_state(screen)?;
                    }
                    validate(
                        &candidate,
                        &shared.registry,
                        shared.max_bytes,
                        shared.run_mode,
                    )?;
                    state.ui_epoch += 1;
                    state.ui_dirty = true;
                    state.doc = candidate;
                    state.cycle = false;
                    state.uncertain = false;
                    state.error = None;
                    state.running = request.action.as_deref() == Some("restart");
                    state.step_budget = None;
                    state.status = if state.running { "ready" } else { "paused" }.into();
                    state.producer_epoch += 1;
                    state.producer_cancel.store(true, Ordering::Relaxed);
                }
                "restart-source" => {
                    state.producer_epoch += 1;
                    state.producer_cancel.store(true, Ordering::Relaxed);
                }
                "reload-agents" => {
                    // Re-read the startup agent policy so revocations and new
                    // grants apply to this running server. The swap is one
                    // atomic handle replacement: in-flight handlers keep the
                    // old policy, new requests see the new one. Fail closed:
                    // an unreadable or invalid file keeps the old policy and
                    // reports the error instead.
                    let path = shared
                        .agent_policy_path
                        .as_ref()
                        .context("reload-agents requires the server to run with --agent-policy")?;
                    let policy = super::auth::AgentAuthPolicy::load(path)
                        .with_context(|| format!("reloading agent policy {}", path.display()))?;
                    shared.registry.set_host_state(Some(policy));
                    // A policy reload is not a document change, so it answers
                    // without bumping the revision or waking `wait` pollers.
                    return Ok(state.ack());
                }
                _ => bail!("unknown control action"),
            }
            state.changed(request.action.as_deref().unwrap());
        }
        "event" => {
            if state.plan.screen.is_none() {
                bail!("event requires a live UI program");
            }
            // The event does not change the source, so the plan the state already holds
            // (parsed from `doc.source`, see `State::plan`) is validated against, not re-parsed.
            let State { doc, plan, .. } = &mut *state;
            doc.queue_ui_event(request.value.into_option(), |candidate| {
                validate_parsed(
                    candidate,
                    plan,
                    &shared.registry,
                    shared.max_bytes,
                    shared.run_mode,
                )
            })?;
            state.ui_dirty = true;
            state.changed("ui_event_queued");
        }
        "send" => {
            if !matches!(state.plan.program.trigger, Some(Trigger::Listen { .. })) {
                bail!("send requires a listener program");
            }
            let record: Data = serde_json::from_value(
                request
                    .value
                    .into_option()
                    .context("send requires a text/bytes value")?,
            )?;
            if record.len() > shared.max_bytes || state.doc.inbox.len() >= MAX_QUEUE {
                return Err(fail("capacity", "inbox or record capacity exceeded"));
            }
            if !state.doc.enqueue(record)? {
                return Err(fail("capacity", "state exceeds 8 MiB"));
            }
            state.changed("input_queued");
        }
        "create" | "update" | "delete" | "patch" => {
            let mut candidate = serde_json::to_value(&state.doc)?;
            let operations = if request.op == "patch" {
                request.operations.context("patch requires operations")?
            } else {
                let mut operation = json!({
                    "op": request.op,
                    "path": request.path.context("mutation requires path")?,
                });
                if let Some(value) = request.value.into_option() {
                    operation
                        .as_object_mut()
                        .expect("constructed operation is an object")
                        .insert("value".into(), value);
                }
                vec![operation]
            };
            if operations.is_empty() || operations.len() > 128 {
                bail!("patch needs 1..128 operations");
            }
            let mut secret_names = state.secret_names();
            let mut checked_source = state.doc.source.clone();
            let mut source_is_redacted =
                state.redactor.redact_text(&state.doc.source) != state.doc.source;
            for operation in operations {
                let op = operation
                    .get("op")
                    .and_then(Json::as_str)
                    .context("patch operation needs op")?;
                let path = operation
                    .get("path")
                    .and_then(Json::as_str)
                    .context("patch operation needs path")?;
                let parts = tokens(path)?;
                let current_source = candidate
                    .get("source")
                    .and_then(Json::as_str)
                    .context("state source must be text")?;
                if current_source != checked_source {
                    // A batch may introduce a secret input before testing its
                    // binding. Keep existing secret names too, so changing or
                    // removing the declaration cannot declassify a value in
                    // the middle of the same batch.
                    let candidate_plan = Plan::parse(current_source)?;
                    secret_names.extend(crate::runtime::sensitive_binding_names(
                        &candidate_plan.program,
                        &state.registry,
                    ));
                    source_is_redacted |=
                        candidate_plan.program.inputs.iter().any(|input| {
                            crate::ast::is_sensitive(input) && input.default.is_some()
                        });
                    checked_source = current_source.to_owned();
                }
                if op == "test"
                    && test_targets_redacted_state(&parts, &secret_names, source_is_redacted)
                {
                    return Err(fail(
                        "secret_test_denied",
                        "patch test cannot target redacted state",
                    ));
                }
                if state.in_flight
                    && !parts.first().is_some_and(|v| {
                        v == "user"
                            || v == "inbox"
                            || (v == "ui" && parts.get(1).is_some_and(|field| field == "events"))
                    })
                {
                    return Err(fail(
                        "busy",
                        "state edits require a safe boundary; pause or cancel, then wait",
                    ));
                }
                mutate(&mut candidate, op, path, operation.get("value").cloned())?;
            }
            let mut candidate: Document =
                serde_json::from_value(candidate).context("invalid state document")?;
            // A client that sends back a read of a secret binding sends the marker. The
            // stored answer stays; the marker is never stored in its place.
            restore_secret_bindings(
                &mut candidate.bindings,
                &state.doc.bindings,
                &state.secret_names(),
            );
            let redacted_source = state.redactor.redact_text(&state.doc.source);
            if candidate.source == redacted_source {
                // A read-modify-write of the visible redacted source must not
                // replace the stored program with its redacted presentation.
                candidate.source.clone_from(&state.doc.source);
            }
            // The plan for the candidate's source: parsed once when the source changes,
            // otherwise the plan the state already holds for the same text.
            let plan = if candidate.source != state.doc.source {
                let plan = Plan::parse(&candidate.source)?;
                candidate.ui = if let Some(screen) = &plan.screen {
                    crate::ui::normalize_live_state(
                        screen,
                        &candidate.ui,
                        state.plan.screen.as_ref(),
                    )?
                } else {
                    Json::Null
                };
                plan
            } else {
                state.plan.clone()
            };
            validate_parsed(
                &candidate,
                &plan,
                &shared.registry,
                shared.max_bytes,
                shared.run_mode,
            )?;
            for input in plan
                .program
                .inputs
                .iter()
                .filter(|input| crate::ast::is_sensitive(input))
            {
                if let Some(value) = candidate.bindings.get(&input.name) {
                    crate::security::register_sensitive_value(
                        &shared.registry,
                        &argument(value),
                        &shared.stop,
                        None,
                    )?;
                }
            }
            // Same control-stop bound as the serve path in `super`: a stop
            // during a wedged-terminal registration reports instead of
            // hanging the session.
            crate::security::register_sensitive_program(
                &shared.registry,
                &plan.program,
                &shared.stop,
                None,
            )?;
            if plan.screen.is_some() && !shared.headless {
                use std::io::IsTerminal;
                if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                    bail!("live TUI needs a terminal or a session started with --headless");
                }
            }
            if candidate.source != state.doc.source || candidate.cwd != state.doc.cwd {
                state.ui_epoch += 1;
            }
            if candidate.source != state.doc.source || candidate.ui != state.doc.ui {
                state.ui_dirty = true;
            }
            let producer = |plan: &Plan| match &plan.program.trigger {
                Some(Trigger::Listen {
                    source: crate::ast::ListenSource::Applet(call),
                    ..
                }) => Some(call.clone()),
                _ => None,
            };
            if producer(&plan) != producer(&state.plan) || candidate.cwd != state.doc.cwd {
                state.producer_epoch += 1;
                state.producer_cancel.store(true, Ordering::Relaxed);
            }
            if candidate.cursor != state.doc.cursor {
                state.uncertain = false;
                state.error = None;
            }
            state.doc = candidate;
            state.plan = plan;
            state.changed("state_edited");
        }
        _ => bail!("unknown operation"),
    }
    shared.wake.notify_all();
    Ok(state.ack())
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    fn document() -> Document {
        Document {
            source: String::new(),
            cwd: PathBuf::from("/tmp"),
            bindings: BTreeMap::new(),
            input: Data::default(),
            cursor: 0,
            inbox: VecDeque::new(),
            user: Map::new(),
            ui: Json::Null,
        }
    }

    #[test]
    fn serialized_budget_counts_escaping_bytes_and_exact_boundaries() {
        let cases = [
            json!("é🦀\n\t\0\"\\"),
            json!({"bytes": [0, 255, 10]}),
            json!({"nested": [{"a": true}, null, -128, ["é", "\n"]]}),
        ];
        for value in cases {
            let size = serde_json::to_vec(&value).unwrap().len();
            for limit in [0, size - 1, size, size + 1] {
                assert_eq!(serialized_within(&value, limit).unwrap(), size <= limit);
            }
        }
    }

    #[test]
    fn rejected_record_transitions_preserve_exact_document() {
        let mut doc = document();
        doc.bindings.insert("event".into(), json!("previous"));
        doc.user
            .insert("padding".into(), json!("x".repeat(MAX_STATE - 2048)));
        doc.inbox.push_back(Data::Text {
            text: "r".repeat(1024),
        });
        assert!(within_state_limit(&doc).unwrap());
        let before = serde_json::to_vec(&doc).unwrap();
        assert!(doc.begin_record("event").is_err());
        assert_eq!(serde_json::to_vec(&doc).unwrap(), before);
        assert!(!doc
            .enqueue(Data::Bytes {
                bytes: vec![255; 1024]
            })
            .unwrap());
        assert_eq!(serde_json::to_vec(&doc).unwrap(), before);
        assert!(doc
            .replace_input(Data::Text {
                text: "r".repeat(2048)
            })
            .is_err());
        assert_eq!(serde_json::to_vec(&doc).unwrap(), before);
        assert!(!doc.replace_ui(json!("r".repeat(2048))).unwrap());
        assert_eq!(serde_json::to_vec(&doc).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn serialization_errors_are_propagated_and_enqueue_rolls_back() {
        use std::os::unix::ffi::OsStringExt;
        let mut doc = document();
        doc.cwd = PathBuf::from(std::ffi::OsString::from_vec(vec![255]));
        assert!(within_state_limit(&doc).is_err());
        assert!(doc.enqueue(Data::default()).is_err());
        assert!(doc.inbox.is_empty());
        let previous = doc.ui.clone();
        assert!(doc.replace_ui(json!({"new": "view"})).is_err());
        assert_eq!(doc.ui, previous);
    }

    #[test]
    fn ui_events_queue_in_place_and_never_copy_the_document() {
        let mut doc = document();
        doc.ui = json!({"events": []});
        doc.user
            .insert("padding".into(), json!("x".repeat(4 * 1024 * 1024)));
        let padding = doc.user["padding"].as_str().unwrap().as_ptr();
        for _ in 0..1000 {
            doc.queue_ui_event(Some(json!({"type": "press"})), |_| Ok(()))
                .unwrap();
            // The UI worker drains the queue; draining here keeps the capacity limit out of the way.
            doc.ui["events"] = json!([]);
        }
        // A copied document would own a fresh buffer for its padding.
        assert_eq!(doc.user["padding"].as_str().unwrap().as_ptr(), padding);
    }

    #[test]
    fn rejected_ui_event_leaves_queue_and_document_unchanged() {
        let mut doc = document();
        doc.ui = json!({"events": [{"type": "press"}]});
        let before = serde_json::to_vec(&doc).unwrap();
        let error = doc
            .queue_ui_event(Some(json!({"type": "submit"})), |_| bail!("rejected"))
            .unwrap_err();
        assert_eq!(error.to_string(), "rejected");
        assert_eq!(serde_json::to_vec(&doc).unwrap(), before);
        assert!(doc.queue_ui_event(None, |_| Ok(())).is_err());
        assert_eq!(serde_json::to_vec(&doc).unwrap(), before);
    }

    #[test]
    fn byte_array_listener_records_keep_utf8_semantics() {
        let mut doc = document();
        doc.inbox.push_back(Data::Bytes {
            bytes: "é\n".as_bytes().to_vec(),
        });
        doc.begin_record("event").unwrap();
        assert_eq!(doc.bindings["event"], json!("é\n"));
        assert!(doc.inbox.is_empty());
        assert_eq!(doc.input.bytes(), "é\n".as_bytes());
    }

    const CLOCK: &str = "open a screen titled \"clock\"\nplace clock on the left\ndefine a program named clock\n  show a badge named status with text \"waiting\"\n  show a button named refresh labelled \"Tick\"\n  whenever refresh is pressed\n    show \"ticked\" in status\n";

    fn parses() -> usize {
        PARSES.with(|count| count.get())
    }

    fn reset_parses() {
        PARSES.with(|count| count.set(0));
    }

    /// A served screen program, built as `serve` builds it, with no registry policy.
    fn live_screen(source: &str) -> Arc<Shared> {
        let plan = Plan::parse(source).unwrap();
        let ui = crate::ui::initial_live_state(plan.screen.as_ref().unwrap()).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let state = State {
            doc: Document {
                source: source.to_owned(),
                cwd: cwd.clone(),
                bindings: BTreeMap::new(),
                input: Data::default(),
                cursor: 0,
                inbox: VecDeque::new(),
                user: Map::new(),
                ui,
            },
            job_session: Arc::new(crate::runtime::live::JobSession::new()),
            redactor: Arc::default(),
            registry: Registry::default(),
            exit: None,
            debug: Default::default(),
            plan,
            revision: 0,
            running: false,
            in_flight: false,
            status: "paused".into(),
            uncertain: false,
            error: None,
            active: None,
            step_budget: None,
            cycle: false,
            runs: 0,
            nodes: 0,
            launches: 0,
            cache_hits: 0,
            last_output: Data::default(),
            active_cancel: Arc::new(AtomicBool::new(false)),
            producer_cancel: Arc::new(AtomicBool::new(false)),
            producer_done: false,
            producer_error: None,
            producer_epoch: 0,
            events: VecDeque::new(),
            ui_epoch: 0,
            ui_dirty: true,
            ui_frame: String::new(),
            ui_applied_revision: 0,
            ui_errors: Json::Null,
        };
        Arc::new(Shared {
            state: Mutex::new(state),
            wake: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
            registry: Registry::default(),
            flood: Mutex::new(FloodBudget::new()),
            max_bytes: 262144,
            initial_cwd: cwd,
            initial_bindings: BTreeMap::new(),
            initial_input: Data::default(),
            headless: true,
            run_mode: false,
            agent_policy_path: None,
            allow_unauthenticated: true,
        })
    }

    fn send(shared: &Shared, op: &str, path: Option<&str>, value: Option<Json>) -> Result<Json> {
        request(
            shared,
            Request {
                op: op.to_owned(),
                path: path.map(str::to_owned),
                value: value.map_or(RequestValue::Missing, RequestValue::Present),
                operations: None,
                action: None,
                if_revision: None,
                after: None,
                timeout_ms: None,
            },
        )
    }

    /// One press, then drain the queue the UI worker would drain, so the queue
    /// capacity does not end the run.
    fn press(shared: &Shared) -> Result<Json> {
        let result = send(
            shared,
            "event",
            None,
            Some(json!({"type": "press", "program": "clock", "widget": "refresh"})),
        );
        shared.state.lock().unwrap().doc.ui["events"] = json!([]);
        result
    }

    #[test]
    fn a_thousand_ui_events_parse_the_source_once() {
        reset_parses();
        let shared = live_screen(CLOCK);
        assert_eq!(parses(), 1);
        for _ in 0..1000 {
            press(&shared).unwrap();
        }
        assert_eq!(parses(), 1);
    }

    #[test]
    fn a_source_edit_parses_once_and_later_events_use_the_new_plan() {
        reset_parses();
        let shared = live_screen(CLOCK);
        let renamed = CLOCK.replace("titled \"clock\"", "titled \"renamed\"");
        send(&shared, "update", Some("/source"), Some(json!(renamed))).unwrap();
        assert_eq!(parses(), 2);
        assert_eq!(
            shared
                .state
                .lock()
                .unwrap()
                .plan
                .screen
                .as_ref()
                .unwrap()
                .title,
            "renamed"
        );
        press(&shared).unwrap();
        assert_eq!(parses(), 2);
    }

    #[test]
    fn an_edit_that_keeps_the_source_reuses_the_plan() {
        reset_parses();
        let shared = live_screen(CLOCK);
        send(&shared, "create", Some("/user/note"), Some(json!("hello"))).unwrap();
        assert_eq!(parses(), 1);
        press(&shared).unwrap();
        assert_eq!(parses(), 1);
    }

    #[test]
    fn a_rejected_source_edit_keeps_the_plan_the_events_validate_against() {
        reset_parses();
        let shared = live_screen(CLOCK);
        let oversized = "x".repeat(65537);
        assert!(send(&shared, "update", Some("/source"), Some(json!(oversized))).is_err());
        assert_eq!(parses(), 2);
        {
            let state = shared.state.lock().unwrap();
            assert_eq!(state.doc.source, CLOCK);
            assert_eq!(state.plan.screen.as_ref().unwrap().title, "clock");
        }
        press(&shared).unwrap();
        assert_eq!(parses(), 2);
    }
}
