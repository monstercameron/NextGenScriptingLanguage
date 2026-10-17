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
impl Plan {
    pub fn parse(source: &str) -> Result<Self> {
        if source.len() > 65536 {
            bail!("source exceeds 64 KiB");
        }
        if crate::ui::syntax::is_screen(source) {
            return Ok(Self {
                program: Program {
                    definitions: Default::default(),
                    inputs: Vec::new(),
                    steps: Vec::new(),
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
                StepKind::CallProgram { call, end, .. } => {
                    names.extend(call.binding.iter().cloned());
                    index += end;
                    continue;
                }
                StepKind::Loop { end, .. } => {
                    // Names created inside a loop are iteration-local. Outer
                    // names rebound by its body are already tracked outside it.
                    index += end;
                    continue;
                }
                StepKind::Read { binding, .. } | StepKind::ReadStdin { binding } => {
                    names.extend(binding.iter().cloned())
                }
                StepKind::ParseJson { binding } | StepKind::TakeJson { binding, .. } => {
                    names.push(binding.clone())
                }
                StepKind::Invoke { call, .. } => {
                    names.extend(call.binding.iter().cloned());
                    names.extend(call.status_binding.iter().cloned());
                }
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
    let plan = Plan::parse(&doc.source)?;
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
        return Ok(plan);
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
                                    kind: StepKind::Invoke {
                                        call: call.clone(),
                                        connection: crate::ast::Connection::Start,
                                    },
                                }],
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
            trigger: None,
        };
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
    Ok(plan)
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
    /// Set by a run-mode program's `exit with`/`fail with`, or by a failed command.
    pub exit: Option<ExitRecord>,
    /// Breakpoints and the statement a run is paused before.
    pub debug: super::debug::Control,
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
        let mut value = json!({"pid":std::process::id(),"execute_version":env!("CARGO_PKG_VERSION"),"status":self.status,"running":self.running,"in_flight":self.in_flight,"active_node":self.active,"effects_may_be_incomplete":self.uncertain,"error":self.error,"runs_completed":self.runs,"nodes_completed":self.nodes,"completed_native_launches":self.launches,"cache_hits":self.cache_hits,"producer_done":self.producer_done,"producer_error":self.producer_error,"frame":self.ui_frame,"ui_applied_revision":self.ui_applied_revision,"ui_errors":self.ui_errors});
        value["debug"] = self.debug.json();
        if output {
            value["last_output"] = json!(self.last_output);
        }
        value
    }
    pub fn snapshot(&self) -> Json {
        json!({"state":self.doc,"runtime":self.runtime(true),"events":self.events})
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
                    (json!(self.last_output), 2)
                } else {
                    (self.runtime(parts.len() == 1), 1)
                }
            }
            "events" => (json!(self.events), 1),
            "bindings" if parts.len() > 1 => (
                self.doc
                    .bindings
                    .get(&parts[1])
                    .cloned()
                    .ok_or_else(|| fail("not_found", "path does not exist"))?,
                2,
            ),
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
            "bindings" => (json!(self.doc.bindings), 1),
            "user" => (json!(self.doc.user), 1),
            "inbox" => (json!(self.doc.inbox), 1),
            "input" => (json!(self.doc.input), 1),
            "ui" => {
                return self
                    .doc
                    .ui
                    .pointer(&path.unwrap()[3..])
                    .cloned()
                    .ok_or_else(|| fail("not_found", "path does not exist"));
            }
            "source" => (json!(self.doc.source), 1),
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
pub(crate) struct Shared {
    pub state: Mutex<State>,
    pub wake: Condvar,
    pub stop: Arc<AtomicBool>,
    pub registry: Registry,
    pub max_bytes: usize,
    pub initial_cwd: PathBuf,
    pub initial_bindings: BTreeMap<String, Json>,
    pub initial_input: Data,
    pub headless: bool,
    /// `execute run --control`: stream uncaptured stdout to this terminal and allow
    /// the program to exit.
    pub run_mode: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub op: String,
    pub path: Option<String>,
    pub value: Option<Json>,
    pub operations: Option<Vec<Json>>,
    pub action: Option<String>,
    pub if_revision: Option<u64>,
    pub after: Option<u64>,
    pub timeout_ms: Option<u64>,
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
                    for name in state.plan.locals() {
                        if let Some(initial) = shared.initial_bindings.get(&name) {
                            candidate.bindings.insert(name, initial.clone());
                        } else {
                            candidate.bindings.remove(&name);
                        }
                    }
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
                _ => bail!("unknown control action"),
            }
            state.changed(request.action.as_deref().unwrap());
        }
        "event" => {
            if state.plan.screen.is_none() {
                bail!("event requires a live UI program");
            }
            let mut candidate = state.doc.clone();
            let events = candidate
                .ui
                .get_mut("events")
                .and_then(Json::as_array_mut)
                .context("missing UI events queue")?;
            if events.len() >= MAX_QUEUE {
                bail!("UI event queue is full");
            }
            events.push(request.value.context("event requires value")?);
            validate(
                &candidate,
                &shared.registry,
                shared.max_bytes,
                shared.run_mode,
            )?;
            state.doc = candidate;
            state.ui_dirty = true;
            state.changed("ui_event_queued");
        }
        "send" => {
            if !matches!(state.plan.program.trigger, Some(Trigger::Listen { .. })) {
                bail!("send requires a listener program");
            }
            let record: Data =
                serde_json::from_value(request.value.context("send requires a text/bytes value")?)?;
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
                vec![
                    json!({"op":request.op,"path":request.path.context("mutation requires path")?,"value":request.value}),
                ]
            };
            if operations.is_empty() || operations.len() > 128 {
                bail!("patch needs 1..128 operations");
            }
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
            if candidate.source != state.doc.source {
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
            }
            let plan = validate(
                &candidate,
                &shared.registry,
                shared.max_bytes,
                shared.run_mode,
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
}
