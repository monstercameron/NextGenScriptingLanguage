#[path = "live.rs"]
pub(crate) mod live;
use super::syntax::*;
use crate::{
    ast::{Program, Value as Argument},
    manifest::Registry,
    runtime::{self, RunOptions},
};
use anyhow::{bail, Context, Result};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{CrosstermBackend, TestBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    symbols,
    text::Line,
    widgets::{
        Axis, Bar, BarChart, BarGroup, Block, Borders, Chart, Dataset, GraphType, List, ListState,
        Paragraph, Row, Table, Wrap,
    },
    Frame, Terminal,
};
#[cfg(test)]
use serde_json::json;
use serde_json::Value as Json;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, IsTerminal},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone)]
struct View {
    text: String,
    selected: usize,
    rows: Vec<Json>,
    bars: Vec<(String, f64)>,
    series: BTreeMap<String, Vec<(f64, u64)>>,
    active: String,
}
impl View {
    fn new(widget: &Widget) -> Self {
        Self {
            text: widget.initial.clone(),
            selected: 0,
            rows: Vec::new(),
            bars: Vec::new(),
            series: BTreeMap::new(),
            active: String::new(),
        }
    }
}
#[derive(Clone)]
struct Execution {
    handler: usize,
    pc: usize,
    data: Vec<u8>,
    parsed_data: Option<Json>,
    item: String,
    error: String,
    bindings: BTreeMap<String, Argument>,
    widget_texts: Vec<Option<String>>,
    generation: u64,
}
struct Job {
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<()>,
    execution: Execution,
}
struct Instance {
    name: String,
    component: Component,
    views: Vec<View>,
    widget_layout: Layout,
    list_items: Vec<Vec<String>>,
    inputs: BTreeMap<String, String>,
    queue: VecDeque<(usize, String, String)>,
    generation: u64,
    job: Option<Job>,
    ticks: Vec<Option<Instant>>,
    required_inputs: Vec<BTreeSet<String>>,
    native_handlers: Vec<bool>,
    input_widget_references: Vec<Vec<usize>>,
    widget_references: Vec<Vec<usize>>,
    has_error_handler: bool,
    error: Option<String>,
}
struct Completion {
    instance: usize,
    result: Result<runtime::RunResult>,
}
struct App {
    screen: Screen,
    instances: Vec<Instance>,
    root_layout: Layout,
    columns_layout: Layout,
    shortcut_labels: String,
    focus: Vec<(usize, usize)>,
    focused: usize,
    registry: Arc<Registry>,
    options: RunOptions,
    sender: Sender<Completion>,
    receiver: Receiver<Completion>,
    wake_sender: Option<SyncSender<()>>,
    started: Instant,
    jobs_done: usize,
    completed_native_launches: usize,
    cache_hits: usize,
    dirty: bool,
    closing: bool,
}
fn native_references(handler: &Handler) -> BTreeSet<String> {
    fn references(value: &Argument, needed: &mut BTreeSet<String>) {
        match value {
            Argument::Reference(name) => {
                needed.insert(name.clone());
            }
            Argument::List(values) => {
                for value in values {
                    references(value, needed);
                }
            }
            _ => {}
        }
    }
    let mut needed = BTreeSet::new();
    for action in &handler.actions {
        if let Action::Native(step) = action {
            match &step.kind {
                crate::ast::StepKind::UseFolder { path }
                | crate::ast::StepKind::Read { path, .. }
                | crate::ast::StepKind::Save { path, .. } => references(path, &mut needed),
                crate::ast::StepKind::Invoke { call, .. }
                | crate::ast::StepKind::CallProgram { call, .. } => {
                    for value in call.parameters.values() {
                        references(value, &mut needed);
                    }
                }
                crate::ast::StepKind::If { condition, .. } => {
                    references(&condition.left, &mut needed);
                    references(&condition.right, &mut needed);
                }
                crate::ast::StepKind::TakeJson { source, .. }
                | crate::ast::StepKind::EncodeJson { source } => references(source, &mut needed),
                crate::ast::StepKind::ParseJson { .. } => {}
                crate::ast::StepKind::ReadStdin { .. }
                | crate::ast::StepKind::ReadCallInput { .. }
                | crate::ast::StepKind::EndCall
                | crate::ast::StepKind::Otherwise { .. }
                | crate::ast::StepKind::EndIf => {}
            }
        }
    }
    needed
}

fn widget_layout(widgets: &[Widget]) -> Layout {
    let constraints = widgets
        .iter()
        .map(|widget| {
            if let Some(rows) = widget.rows {
                Constraint::Length(rows)
            } else {
                match widget.kind {
                    Kind::Heading => Constraint::Length(1),
                    Kind::Input | Kind::Button => Constraint::Length(3),
                    Kind::Badge => Constraint::Length(2),
                    Kind::Table => Constraint::Length(5),
                    Kind::Chart => Constraint::Min(7),
                    Kind::Bars => Constraint::Min(12),
                    Kind::List => Constraint::Min(3),
                    Kind::Text => Constraint::Length(4),
                }
            }
        })
        .collect::<Vec<_>>();
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
}

fn referenced_widgets(component: &Component) -> Vec<Vec<usize>> {
    component
        .handlers
        .iter()
        .map(|handler| {
            let mut referenced = BTreeSet::new();
            for action in &handler.actions {
                let id = match action {
                    Action::Show(Value::Widget(id), _) | Action::Send(Value::Widget(id), _) => {
                        Some(id)
                    }
                    _ => None,
                };
                if let Some(id) = id {
                    if let Some(index) =
                        component.widgets.iter().position(|widget| &widget.id == id)
                    {
                        referenced.insert(index);
                    }
                }
            }
            referenced.into_iter().collect()
        })
        .collect()
}

impl App {
    fn new(screen: &Screen, registry: &Registry, options: &RunOptions) -> Self {
        let instances = screen
            .placements
            .iter()
            .map(|name| {
                let component = screen.components[name].clone();
                let views = component.widgets.iter().map(View::new).collect();
                let widget_layout = widget_layout(&component.widgets);
                let list_items = component
                    .widgets
                    .iter()
                    .map(|widget| {
                        widget
                            .items
                            .iter()
                            .map(|item| safe(item).into_owned())
                            .collect()
                    })
                    .collect();
                let ticks = component
                    .handlers
                    .iter()
                    .map(|h| {
                        if let Event::Every(ms) = h.event {
                            Instant::now().checked_add(Duration::from_millis(ms))
                        } else {
                            None
                        }
                    })
                    .collect();
                let native_references: Vec<BTreeSet<String>> =
                    component.handlers.iter().map(native_references).collect();
                let required_inputs = component
                    .handlers
                    .iter()
                    .zip(&native_references)
                    .map(|(_, references)| {
                        references
                            .intersection(&component.inputs)
                            .cloned()
                            .collect()
                    })
                    .collect();
                let native_handlers = component
                    .handlers
                    .iter()
                    .map(|handler| {
                        handler
                            .actions
                            .iter()
                            .any(|action| matches!(action, Action::Native(_)))
                    })
                    .collect();
                let input_widget_references = component
                    .widgets
                    .iter()
                    .enumerate()
                    .filter(|(_, widget)| widget.kind == Kind::Input)
                    .fold(
                        vec![Vec::new(); component.handlers.len()],
                        |mut result, (j, widget)| {
                            for (h, references) in native_references.iter().enumerate() {
                                if references.contains(&widget.id) {
                                    result[h].push(j);
                                }
                            }
                            result
                        },
                    );
                let widget_references = referenced_widgets(&component);
                let has_error_handler = component
                    .handlers
                    .iter()
                    .any(|handler| handler.event == Event::Error);
                Instance {
                    name: name.clone(),
                    component,
                    views,
                    widget_layout,
                    list_items,
                    inputs: BTreeMap::new(),
                    queue: VecDeque::new(),
                    generation: 0,
                    job: None,
                    ticks,
                    required_inputs,
                    native_handlers,
                    input_widget_references,
                    widget_references,
                    has_error_handler,
                    error: None,
                }
            })
            .collect::<Vec<_>>();
        let root_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ]);
        let columns_layout = if instances.len() == 2 {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(screen.first_width),
                    Constraint::Percentage(100 - screen.first_width),
                ])
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(100)])
        };
        let shortcut_labels = instances
            .iter()
            .flat_map(|instance| instance.component.handlers.iter())
            .filter_map(|handler| match &handler.event {
                Event::Key(key) => Some(key.label.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let focus = instances
            .iter()
            .enumerate()
            .flat_map(|(i, v)| {
                v.component
                    .widgets
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| matches!(w.kind, Kind::Input | Kind::List | Kind::Button))
                    .map(move |(j, _)| (i, j))
            })
            .collect();
        let (sender, receiver) = mpsc::channel();
        let mut app = Self {
            screen: screen.clone(),
            instances,
            root_layout,
            columns_layout,
            shortcut_labels,
            focus,
            focused: 0,
            registry: Arc::new(registry.clone()),
            options: RunOptions {
                inputs: Default::default(),
                working_directory: options.working_directory.clone(),
                cache_directory: options.cache_directory.clone(),
                trace: options.trace,
                max_output_bytes: options.max_output_bytes,
                command_timeout: Some(options.command_timeout.unwrap_or(Duration::from_secs(30))),
            },
            sender,
            receiver,
            wake_sender: None,
            started: Instant::now(),
            jobs_done: 0,
            completed_native_launches: 0,
            cache_hits: 0,
            dirty: true,
            closing: false,
        };
        for i in 0..app.instances.len() {
            app.enqueue(i, &Event::Open, "".into(), "".into());
        }
        app
    }
    fn enqueue(&mut self, instance: usize, event: &Event, item: String, error: String) {
        let instance = &mut self.instances[instance];
        for (index, handler) in instance
            .component
            .handlers
            .iter()
            .enumerate()
            .filter(|(_, h)| &h.event == event)
        {
            let _ = handler;
            if let Some(old) = instance.queue.iter_mut().find(|v| v.0 == index) {
                *old = (index, item.clone(), error.clone());
            } else if instance.queue.len() < 32 {
                instance
                    .queue
                    .push_back((index, item.clone(), error.clone()));
            }
        }
    }
    fn deliver(&mut self, source: usize, port: &str, value: String) {
        let links = self
            .screen
            .links
            .iter()
            .filter(|l| l.source == self.instances[source].name && l.output == port)
            .cloned()
            .collect::<Vec<_>>();
        for link in links {
            let target = self
                .instances
                .iter()
                .position(|v| v.name == link.target)
                .unwrap();
            let instance = &mut self.instances[target];
            if instance.inputs.get(&link.input) != Some(&value) {
                instance.generation = instance.generation.wrapping_add(1);
                if let Some(job) = &instance.job {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            instance.inputs.insert(link.input.clone(), value.clone());
            self.enqueue(target, &Event::Input(link.input), value.clone(), "".into());
        }
    }
    fn value(&self, i: usize, value: Value, execution: &Execution) -> Result<String> {
        Ok(match value {
            Value::Literal(s) => s,
            Value::Widget(id) => execution
                .widget_texts
                .get(self.widget(i, &id)?)
                .and_then(Option::as_ref)
                .context("unknown widget snapshot")?
                .clone(),
            Value::Item => execution.item.clone(),
            Value::Result => {
                String::from_utf8(execution.data.clone()).context("UI text must be UTF-8")?
            }
            Value::Error => execution.error.clone(),
        })
    }
    fn set_text(
        &mut self,
        i: usize,
        id: &str,
        text: String,
        execution: &mut Execution,
    ) -> Result<()> {
        let j = self.widget(i, id)?;
        if self.instances[i].component.widgets[j].kind == Kind::Input {
            validate_text_field(&text)?;
            if execution.bindings.contains_key(id) {
                execution
                    .bindings
                    .insert(id.into(), Argument::String(text.clone()));
            }
        }
        if let Some(snapshot) = execution.widget_texts.get_mut(j).and_then(Option::as_mut) {
            *snapshot = text.clone();
        }
        let changed = self.instances[i].views[j].text != text;
        self.instances[i].views[j].text = text;
        self.dirty |= changed;
        Ok(())
    }

    fn json_data<'a>(
        execution: &'a mut Execution,
        context: Option<&'static str>,
    ) -> Result<&'a Json> {
        if execution.parsed_data.is_none() {
            execution.parsed_data = Some(match context {
                Some(context) => serde_json::from_slice(&execution.data).context(context)?,
                None => serde_json::from_slice(&execution.data)?,
            });
        }
        Ok(execution.parsed_data.as_ref().unwrap())
    }

    fn execute(&mut self, i: usize, mut execution: Execution) -> Result<()> {
        // JSON actions share one parsed value until the next native command,
        // which starts a new result-data epoch. Locate the final consumer once
        // so its table projection can take ownership of parsed rows.
        let last_json_action = {
            let actions = &self.instances[i].component.handlers[execution.handler].actions;
            let remaining = &actions[execution.pc..];
            let native_offset = remaining
                .iter()
                .position(|action| matches!(action, Action::Native(_)))
                .unwrap_or(remaining.len());
            (execution.pc..execution.pc + native_offset)
                .rev()
                .find(|&index| {
                    matches!(
                        &actions[index],
                        Action::Rows(_)
                            | Action::Bars(_)
                            | Action::Field(_, _)
                            | Action::Point(_, _)
                    )
                })
        };
        while execution.pc
            < self.instances[i].component.handlers[execution.handler]
                .actions
                .len()
        {
            let action_index = execution.pc;
            let action = self.instances[i].component.handlers[execution.handler].actions
                [action_index]
                .clone();
            execution.pc += 1;
            if let Action::Native(first) = action {
                let mut steps = vec![first];
                while let Some(Action::Native(step)) = self.instances[i].component.handlers
                    [execution.handler]
                    .actions
                    .get(execution.pc)
                {
                    steps.push(step.clone());
                    execution.pc += 1;
                }
                let registry = self.registry.clone();
                let options = RunOptions {
                    inputs: Default::default(),
                    working_directory: self.options.working_directory.clone(),
                    cache_directory: self.options.cache_directory.clone(),
                    trace: self.options.trace,
                    max_output_bytes: self.options.max_output_bytes,
                    command_timeout: self.options.command_timeout,
                };
                execution.parsed_data = None;
                let bindings = std::mem::take(&mut execution.bindings);
                let input = std::mem::take(&mut execution.data);
                let sender = self.sender.clone();
                let wake_sender = self.wake_sender.clone();
                let cancel = Arc::new(AtomicBool::new(false));
                let token = cancel.clone();
                let handle = thread::spawn(move || {
                    let result = runtime::run_with_context(
                        &Program {
                            definitions: Default::default(),
                            inputs: Vec::new(),
                            steps,
                            trigger: None,
                        },
                        &registry,
                        &options,
                        bindings,
                        input,
                        token,
                    );
                    let _ = sender.send(Completion {
                        instance: i,
                        result,
                    });
                    if let Some(wake_sender) = wake_sender {
                        let _ = wake_sender.try_send(());
                    }
                });
                self.instances[i].job = Some(Job {
                    cancel,
                    handle,
                    execution,
                });
                return Ok(());
            }
            match action {
                Action::Press(id) => {
                    self.enqueue(i, &Event::Press(id), String::new(), execution.error.clone())
                }
                Action::CloseScreen => {
                    self.closing = true;
                    return Ok(());
                }
                Action::Send(value, port) => {
                    let value = self.value(i, value, &execution)?;
                    validate_message(&value)?;
                    self.deliver(i, &port, value);
                }
                Action::Show(value, id) => {
                    let value = self.value(i, value, &execution)?;
                    self.set_text(i, &id, value, &mut execution)?;
                }
                Action::Rows(id) => {
                    let j = self.widget(i, &id)?;
                    let can_move_rows = last_json_action == Some(action_index);
                    let (changed, cloned_rows) = {
                        let data =
                            Self::json_data(&mut execution, Some("table needs JSON records"))?;
                        let rows = data
                            .as_array()
                            .map(Vec::as_slice)
                            .unwrap_or_else(|| std::slice::from_ref(data));
                        if rows.len() > 10000 {
                            bail!("table exceeds 10000 rows");
                        }
                        for row in rows {
                            if !row.is_object() {
                                bail!("table rows must be objects");
                            }
                            for (_, field) in &self.instances[i].component.widgets[j].columns {
                                if row.get(field).is_none() {
                                    bail!("table record is missing field {field}");
                                }
                            }
                        }
                        // Compare borrowed slices before allocating a projection.
                        // Validation above still completes before any view changes.
                        let changed = self.instances[i].views[j].rows.as_slice() != rows;
                        let cloned_rows = (changed && !can_move_rows).then(|| rows.to_vec());
                        (changed, cloned_rows)
                    };
                    if changed {
                        let rows = match cloned_rows {
                            Some(rows) => rows,
                            None => match execution
                                .parsed_data
                                .take()
                                .expect("table JSON was parsed above")
                            {
                                Json::Array(rows) => rows,
                                value => vec![value],
                            },
                        };
                        self.instances[i].views[j].rows = rows;
                        self.dirty = true;
                    }
                }
                Action::Bars(id) => {
                    let j = self.widget(i, &id)?;
                    let widget = &self.instances[i].component.widgets[j];
                    let label_field = widget
                        .bar_label
                        .as_ref()
                        .context("missing bar label field")?;
                    let value_field = widget
                        .bar_value
                        .as_ref()
                        .context("missing bar size field")?;
                    let bars = {
                        let data =
                            Self::json_data(&mut execution, Some("bar chart needs JSON records"))?;
                        let rows = data.as_array().context("bar chart needs a JSON array")?;
                        if rows.len() > 256 {
                            bail!("bar chart exceeds 256 bars");
                        }
                        rows.iter()
                            .map(|row| {
                                let label = row
                                    .get(label_field)
                                    .and_then(Json::as_str)
                                    .with_context(|| format!("bar label {label_field} must be text"))?;
                                let value = row
                                    .get(value_field)
                                    .and_then(Json::as_f64)
                                    .filter(|v| v.is_finite() && *v >= 0.0)
                                    .with_context(|| {
                                        format!(
                                            "bar size {value_field} must be a finite nonnegative number"
                                        )
                                    })?;
                                Ok((label.to_owned(), value))
                            })
                            .collect::<Result<Vec<_>>>()?
                    };
                    if self.instances[i].views[j].bars != bars {
                        self.instances[i].views[j].bars = bars;
                        self.dirty = true;
                    }
                }
                Action::Field(field, id) => {
                    let text = if last_json_action == Some(action_index) {
                        Self::json_data(&mut execution, None)?;
                        let data = execution
                            .parsed_data
                            .take()
                            .expect("field JSON was parsed above");
                        let value = match data {
                            Json::Object(mut fields) => fields.remove(&field),
                            data => data.get(&field).cloned(),
                        }
                        .with_context(|| format!("missing field {field}"))?;
                        match value {
                            Json::String(text) => text,
                            value => display_value(&value),
                        }
                    } else {
                        let data = Self::json_data(&mut execution, None)?;
                        let value = data
                            .get(&field)
                            .with_context(|| format!("missing field {field}"))?;
                        display_value(value)
                    };
                    self.set_text(i, &id, text, &mut execution)?;
                }
                Action::Point(field, id) => {
                    let j = self.widget(i, &id)?;
                    let key_field = self.instances[i].component.widgets[j].key.as_ref();
                    let (value, key) = {
                        let data = Self::json_data(&mut execution, None)?;
                        let value = data.get(&field).and_then(Json::as_u64).with_context(|| {
                            format!("chart field {field} must be a nonnegative integer")
                        })?;
                        let key = if let Some(key) = key_field {
                            data.get(key)
                                .and_then(Json::as_str)
                                .context("chart key must be text")?
                                .to_owned()
                        } else {
                            String::new()
                        };
                        (value, key)
                    };
                    let view = &mut self.instances[i].views[j];
                    if !view.series.contains_key(&key) && view.series.len() >= 16 {
                        let old = view.series.keys().next().unwrap().clone();
                        view.series.remove(&old);
                    }
                    let series = view.series.entry(key.clone()).or_default();
                    series.push((self.started.elapsed().as_secs_f64(), value));
                    if series.len() > 120 {
                        series.remove(0);
                    }
                    view.active = key;
                    self.dirty = true;
                }
                Action::Native(_) => unreachable!(),
            }
        }
        // A successful ordinary handler clears its prior diagnostic. Recovery
        // handlers preserve the original failure until a native retry succeeds.
        if execution.error.is_empty()
            && self.instances[i].component.handlers[execution.handler].event != Event::Error
            && self.instances[i].error.take().is_some()
        {
            self.dirty = true;
        }
        Ok(())
    }
    fn widget(&self, i: usize, id: &str) -> Result<usize> {
        self.instances[i]
            .component
            .widgets
            .iter()
            .position(|w| w.id == id)
            .context("unknown widget")
    }
    fn fail(&mut self, i: usize, handler: usize, error: String, recovering: bool) {
        let handling_error =
            recovering || self.instances[i].component.handlers[handler].event == Event::Error;
        let changed = self.instances[i].error.as_deref() != Some(error.as_str());
        self.instances[i].error = Some(error.clone());
        if !handling_error {
            self.enqueue(i, &Event::Error, "".into(), error);
        }
        self.dirty |= changed;
    }
    fn advance(&mut self, ticks: bool) {
        self.advance_controlled(ticks, usize::MAX);
    }
    fn advance_controlled(&mut self, ticks: bool, budget: usize) -> usize {
        let mut started = 0;
        while let Ok(completion) = self.receiver.try_recv() {
            let i = completion.instance;
            let Some(job) = self.instances[i].job.take() else {
                continue;
            };
            let _ = job.handle.join();
            self.jobs_done += 1;
            if job.execution.generation != self.instances[i].generation {
                continue;
            }
            match completion.result {
                Ok(output) => {
                    self.completed_native_launches += output.launched_processes;
                    self.cache_hits += output.cache_hits;
                    let handler = job.execution.handler;
                    let recovering = !job.execution.error.is_empty();
                    let mut execution = job.execution;
                    execution.data = output.output;
                    execution.parsed_data = None;
                    if self.instances[i].error.take().is_some() {
                        self.dirty = true;
                    }
                    if let Err(error) = self.execute(i, execution) {
                        self.fail(i, handler, format!("{error:#}"), recovering);
                    }
                }
                Err(error) => self.fail(
                    i,
                    job.execution.handler,
                    format!("{error:#}"),
                    !job.execution.error.is_empty(),
                ),
            }
        }
        if budget == 0 {
            return started;
        }
        for i in 0..self.instances.len() {
            if ticks {
                let due = self.instances[i]
                    .ticks
                    .iter()
                    .enumerate()
                    .filter_map(|(h, d)| d.filter(|t| *t <= Instant::now()).map(|_| h))
                    .collect::<Vec<_>>();
                for h in due {
                    if let Event::Every(ms) = self.instances[i].component.handlers[h].event {
                        self.instances[i].ticks[h] =
                            Instant::now().checked_add(Duration::from_millis(ms));
                        self.enqueue(i, &Event::Every(ms), "".into(), "".into());
                    }
                }
            }
        }
        for _ in 0..64 {
            let mut progress = false;
            for i in 0..self.instances.len() {
                if self.closing || started >= budget {
                    return started;
                }
                // A script's UI-only close handler can interrupt a busy worker.
                // Native pipelines still serialize within their component.
                let next = if self.instances[i].job.is_some() {
                    let position = self.instances[i].queue.iter().position(|(handler, _, _)| {
                        let actions = &self.instances[i].component.handlers[*handler].actions;
                        actions.iter().any(|a| matches!(a, Action::CloseScreen))
                            && !actions.iter().any(|a| matches!(a, Action::Native(_)))
                    });
                    position.and_then(|p| self.instances[i].queue.remove(p))
                } else {
                    let instance = &self.instances[i];
                    let position = instance.queue.iter().position(|(handler, _, _)| {
                        instance.required_inputs[*handler]
                            .iter()
                            .all(|port| instance.inputs.contains_key(port))
                    });
                    position.and_then(|p| self.instances[i].queue.remove(p))
                };
                let Some((handler, item, error)) = next else {
                    continue;
                };
                progress = true;
                started += 1;
                let bindings = if self.instances[i].native_handlers[handler] {
                    let instance = &self.instances[i];
                    let mut bindings = BTreeMap::new();
                    for port in &instance.required_inputs[handler] {
                        if let Some(value) = instance.inputs.get(port) {
                            bindings.insert(port.clone(), Argument::String(value.clone()));
                        }
                    }
                    bindings.insert("error".into(), Argument::String(error.clone()));
                    for &j in &instance.input_widget_references[handler] {
                        let widget = &instance.component.widgets[j];
                        bindings.insert(
                            widget.id.clone(),
                            Argument::String(instance.views[j].text.clone()),
                        );
                    }
                    bindings
                } else {
                    BTreeMap::new()
                };
                let data = item.as_bytes().to_vec();
                // Read only the fields used by this handler, once. User edits
                // while a worker runs belong to the next handler execution.
                let widget_references = &self.instances[i].widget_references[handler];
                let mut widget_texts = if widget_references.is_empty() {
                    Vec::new()
                } else {
                    vec![None; self.instances[i].views.len()]
                };
                for &j in widget_references {
                    widget_texts[j] = Some(self.instances[i].views[j].text.clone());
                }
                let execution = Execution {
                    handler,
                    pc: 0,
                    data,
                    parsed_data: None,
                    item,
                    error,
                    bindings,
                    widget_texts,
                    generation: self.instances[i].generation,
                };
                let recovering = !execution.error.is_empty();
                if let Err(error) = self.execute(i, execution) {
                    self.fail(i, handler, format!("{error:#}"), recovering);
                }
            }
            if !progress {
                break;
            }
        }
        started
    }
    fn idle(&self) -> bool {
        self.instances
            .iter()
            .all(|v| v.job.is_none() && v.queue.is_empty())
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        if key.code == KeyCode::Tab && !self.focus.is_empty() {
            let focused = (self.focused + 1) % self.focus.len();
            if focused != self.focused {
                self.focused = focused;
                self.dirty = true;
            }
            return false;
        }
        if key.code == KeyCode::BackTab && !self.focus.is_empty() {
            let focused = (self.focused + self.focus.len() - 1) % self.focus.len();
            if focused != self.focused {
                self.focused = focused;
                self.dirty = true;
            }
            return false;
        }
        let mut handled = false;
        for i in 0..self.instances.len() {
            let handler_count = self.instances[i].component.handlers.len();
            for h in 0..handler_count {
                let event = {
                    let event = &self.instances[i].component.handlers[h].event;
                    if let Event::Key(shortcut) = event {
                        let code = match key.code {
                            KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
                            other => other,
                        };
                        (code == shortcut.code && key.modifiers == shortcut.modifiers)
                            .then(|| event.clone())
                    } else {
                        None
                    }
                };
                if let Some(event) = event {
                    self.enqueue(i, &event, String::new(), String::new());
                    handled = true;
                }
            }
        }
        if handled {
            return false;
        }
        let Some(&(i, j)) = self.focus.get(self.focused) else {
            return false;
        };
        let mut changed = false;
        match (&self.instances[i].component.widgets[j].kind, key.code) {
            (Kind::Input, KeyCode::Enter) => {
                let id = self.instances[i].component.widgets[j].id.clone();
                self.enqueue(i, &Event::Submit(id), String::new(), String::new());
            }
            (Kind::Input, KeyCode::Backspace) => {
                changed = self.instances[i].views[j].text.pop().is_some();
            }
            (Kind::Input, KeyCode::Char('u')) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                changed = !self.instances[i].views[j].text.is_empty();
                self.instances[i].views[j].text.clear();
            }
            (Kind::Input, KeyCode::Char(c))
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.instances[i].views[j].text.len() + c.len_utf8() <= 512 =>
            {
                self.instances[i].views[j].text.push(c);
                changed = true;
            }
            (Kind::List, KeyCode::Down) => {
                let item_count = self.instances[i].component.widgets[j].items.len();
                let view = &mut self.instances[i].views[j];
                let selected = (view.selected + 1).min(item_count.saturating_sub(1));
                changed = selected != view.selected;
                view.selected = selected;
            }
            (Kind::List, KeyCode::Up) => {
                let view = &mut self.instances[i].views[j];
                let selected = view.selected.saturating_sub(1);
                changed = selected != view.selected;
                view.selected = selected;
            }
            (Kind::List, KeyCode::Enter) => {
                let selected = self.instances[i].views[j].selected;
                let item = self.instances[i].component.widgets[j]
                    .items
                    .get(selected)
                    .cloned();
                if let Some(item) = item {
                    let id = self.instances[i].component.widgets[j].id.clone();
                    self.enqueue(i, &Event::Choose(id), item, String::new());
                }
            }
            (Kind::Button, KeyCode::Enter | KeyCode::Char(' ')) => {
                let id = self.instances[i].component.widgets[j].id.clone();
                self.enqueue(i, &Event::Press(id), String::new(), String::new());
            }
            _ => {}
        }
        if changed {
            self.dirty = true;
        }
        false
    }
    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 45 || area.height < 16 {
            frame.render_widget(
                Paragraph::new("Resize to at least 45 columns by 16 rows. Ctrl-C interrupts."),
                area,
            );
            return;
        }
        let root = self.root_layout.split(area);
        frame.render_widget(
            Paragraph::new(safe(&self.screen.title)).style(Style::default().fg(Color::Cyan)),
            root[0],
        );
        let columns = self.columns_layout.split(root[1]);
        for (i, instance) in self.instances.iter().enumerate() {
            let cells = instance.widget_layout.split(columns[i]);
            for (j, (widget, view)) in instance
                .component
                .widgets
                .iter()
                .zip(&instance.views)
                .enumerate()
            {
                let cell = cells[j];
                if cell.width == 0 || cell.height == 0 {
                    continue;
                }
                let focused = self.focus.get(self.focused) == Some(&(i, j));
                let color = if focused {
                    Color::Yellow
                } else {
                    Color::DarkGray
                };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(safe(&widget.id))
                    .border_style(Style::default().fg(color));
                match widget.kind {
                    Kind::Heading => frame.render_widget(
                        Paragraph::new(safe(&view.text)).style(Style::default().fg(Color::Cyan)),
                        cell,
                    ),
                    Kind::Input | Kind::Text | Kind::Badge | Kind::Button => {
                        let text: Cow<'_, str> = if widget.kind == Kind::Button {
                            Cow::Owned(format!("[ {} ]", safe(&view.text)))
                        } else {
                            safe(&view.text)
                        };
                        let width = text.width();
                        let scroll = width.saturating_sub(cell.width.saturating_sub(3) as usize);
                        let paragraph = if widget.kind == Kind::Input {
                            Paragraph::new(text).scroll((0, scroll.min(u16::MAX as usize) as u16))
                        } else {
                            Paragraph::new(text).wrap(Wrap { trim: false })
                        };
                        if focused
                            && widget.kind == Kind::Input
                            && cell.width >= 3
                            && cell.height >= 3
                        {
                            frame.set_cursor_position((
                                cell.x
                                    + 1
                                    + width.min(cell.width.saturating_sub(3) as usize) as u16,
                                cell.y + 1,
                            ));
                        }
                        if widget.kind == Kind::Badge {
                            frame.render_widget(
                                paragraph.style(Style::default().fg(if instance.error.is_some() {
                                    Color::Red
                                } else {
                                    Color::Green
                                })),
                                cell,
                            );
                        } else {
                            frame.render_widget(paragraph.block(block), cell);
                        }
                    }
                    Kind::List => {
                        let items = &instance.list_items[j];
                        let mut state = ListState::default().with_selected(if items.is_empty() {
                            None
                        } else {
                            Some(view.selected)
                        });
                        frame.render_stateful_widget(
                            List::new(items.iter().map(String::as_str))
                                .block(block)
                                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan)),
                            cell,
                            &mut state,
                        );
                    }
                    Kind::Table => render_table(frame, cell, block, widget, view),
                    Kind::Bars => render_bars(frame, cell, block, view, &widget.bar_suffix),
                    Kind::Chart => render_chart(
                        frame,
                        cell,
                        block,
                        view,
                        &widget.horizontal_label,
                        &widget.vertical_label,
                    ),
                }
            }
        }
        let footer = if let Some(instance) = self
            .instances
            .iter()
            .find(|v| v.error.is_some() && !v.has_error_handler)
        {
            format!(
                "{}: {}",
                instance.name,
                safe(instance.error.as_ref().unwrap())
            )
        } else if self.shortcut_labels.is_empty() {
            "Tab focus  Enter activate  Ctrl-U clear  Ctrl-C interrupt".into()
        } else {
            format!(
                "Keys {} | Tab focus  Enter activate  Ctrl-U clear  Ctrl-C interrupt",
                self.shortcut_labels
            )
        };
        frame.render_widget(
            Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
            root[2],
        );
    }
    fn rendered_frame(&self) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("in-memory terminal");
        terminal
            .draw(|frame| self.render(frame))
            .expect("in-memory render");
        let buffer = terminal.backend().buffer();
        let rendered_capacity = (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol().len())
                    .sum::<usize>()
            })
            .sum::<usize>()
            + 29;
        let mut rendered = String::with_capacity(rendered_capacity);
        for y in 0..30 {
            if y != 0 {
                rendered.push('\n');
            }
            for x in 0..100 {
                rendered.push_str(buffer[(x, y)].symbol());
            }
        }

        rendered
    }

    fn snapshot(&self) -> Json {
        let mut programs = serde_json::Map::new();
        for instance in &self.instances {
            let mut widgets = serde_json::Map::new();
            for (widget, view) in instance.component.widgets.iter().zip(&instance.views) {
                let mut bars = Vec::with_capacity(view.bars.len());
                for (label, value) in &view.bars {
                    let mut bar = serde_json::Map::new();
                    bar.insert("label".into(), Json::String(label.clone()));
                    bar.insert("value".into(), Json::from(*value));
                    bars.push(Json::Object(bar));
                }

                let mut series = serde_json::Map::new();
                for (name, samples) in &view.series {
                    let samples = samples
                        .iter()
                        .map(|(x, y)| Json::Array(vec![Json::from(*x), Json::from(*y)]))
                        .collect();
                    series.insert(name.clone(), Json::Array(samples));
                }

                let mut widget_value = serde_json::Map::new();
                widget_value.insert("text".into(), Json::String(view.text.clone()));
                widget_value.insert("rows".into(), Json::Array(view.rows.clone()));
                widget_value.insert("bars".into(), Json::Array(bars));
                widget_value.insert("active_series".into(), Json::String(view.active.clone()));
                widget_value.insert("series".into(), Json::Object(series));
                widgets.insert(widget.id.clone(), Json::Object(widget_value));
            }

            let mut inputs = serde_json::Map::new();
            for (name, value) in &instance.inputs {
                inputs.insert(name.clone(), Json::String(value.clone()));
            }
            let mut program = serde_json::Map::new();
            program.insert("inputs".into(), Json::Object(inputs));
            program.insert("widgets".into(), Json::Object(widgets));
            program.insert(
                "error".into(),
                instance
                    .error
                    .as_ref()
                    .map_or(Json::Null, |error| Json::String(error.clone())),
            );
            programs.insert(instance.name.clone(), Json::Object(program));
        }

        let rendered = self.rendered_frame();

        let mut snapshot = serde_json::Map::new();
        snapshot.insert("screen".into(), Json::String(self.screen.title.clone()));
        snapshot.insert("completed_jobs".into(), Json::from(self.jobs_done));
        snapshot.insert("programs".into(), Json::Object(programs));
        snapshot.insert("frame".into(), Json::String(rendered));
        Json::Object(snapshot)
    }
}
impl Drop for App {
    fn drop(&mut self) {
        for instance in &self.instances {
            if let Some(job) = &instance.job {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        for instance in &mut self.instances {
            if let Some(job) = instance.job.take() {
                let _ = job.handle.join();
            }
        }
    }
}
fn safe(text: &str) -> Cow<'_, str> {
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        Cow::Owned(
            text.chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}
fn display_value(value: &Json) -> String {
    match value {
        Json::String(v) => v.clone(),
        Json::Null => String::new(),
        _ => value.to_string(),
    }
}
fn display_cell(value: &Json) -> Cow<'_, str> {
    match value {
        Json::String(text) => safe(text),
        Json::Null => Cow::Borrowed(""),
        _ => {
            let mut text = value.to_string();
            if text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                text.retain(|c| !c.is_control() || c == '\n' || c == '\t');
            }
            Cow::Owned(text)
        }
    }
}
fn render_table(frame: &mut Frame, area: Rect, block: Block<'_>, widget: &Widget, view: &View) {
    // The bordered block takes two rows and the default one-line header takes one.
    // This stateless table starts at row zero, so rows after the visible viewport
    // cannot affect its output and need not be converted into owned cell strings.
    let visible_rows = area.height.saturating_sub(3) as usize;
    let rows = view.rows.iter().take(visible_rows).map(|row| {
        Row::new(
            widget
                .columns
                .iter()
                .map(|(_, field)| display_cell(&row[field]))
                .collect::<Vec<_>>(),
        )
    });
    let widths = widget
        .columns
        .iter()
        .enumerate()
        .map(|(n, _)| {
            if n == 0 {
                Constraint::Percentage(40)
            } else {
                Constraint::Percentage(
                    (60 / (widget.columns.len().saturating_sub(1).max(1))) as u16,
                )
            }
        })
        .collect::<Vec<_>>();
    let header = Row::new(widget.columns.iter().map(|(title, _)| safe(title)))
        .style(Style::default().fg(Color::Yellow));
    frame.render_widget(
        Table::new(rows, widths)
            .header(header)
            .block(block)
            .column_spacing(1),
        area,
    );
}
/// Keep horizontal labels bounded by display columns, including wide Unicode.
fn bounded_label(value: &str, width: usize) -> Cow<'_, str> {
    let mut text = safe(value);
    if text.contains(['\n', '\t']) {
        text = Cow::Owned(text.replace(['\n', '\t'], " "));
    }
    if text.width() <= width {
        return text;
    }
    if width == 0 {
        return Cow::Borrowed("");
    }
    let mut result = String::new();
    let mut used = 0;
    for c in text.chars() {
        let size = c.width().unwrap_or(0);
        if used + size > width.saturating_sub(1) {
            break;
        }
        result.push(c);
        used += size;
    }
    result.push('…');
    Cow::Owned(result)
}
fn render_bars(frame: &mut Frame, area: Rect, block: Block<'_>, view: &View, suffix: &str) {
    let inner = block.inner(area);
    if inner.is_empty() {
        frame.render_widget(block, area);
        return;
    }
    if view.bars.is_empty() {
        frame.render_widget(Paragraph::new("Waiting for samples.").block(block), area);
        return;
    }
    let maximum = view.bars.iter().map(|(_, v)| *v).fold(0.0, f64::max);
    let maximum = if maximum > 0.0 { maximum } else { 1.0 };
    let label_width = (inner.width as usize * 45 / 100)
        .min(34)
        .min(inner.width.saturating_sub(1) as usize);
    let safe_suffix = safe(suffix);
    let bars = view
        .bars
        .iter()
        .map(|(label, value)| {
            let text = if *value >= 1e9 {
                format!("{value:.1e}{safe_suffix}")
            } else {
                format!("{value:.1}{safe_suffix}")
            };
            Bar::default()
                .label(Line::from(bounded_label(label, label_width)))
                .value((*value / maximum * 1000.0).round() as u64)
                .text_value(text)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        BarChart::default()
            .block(block)
            .direction(Direction::Horizontal)
            .data(BarGroup::default().bars(&bars))
            .max(1000)
            .bar_width(1)
            .bar_gap(0)
            .bar_style(Style::default().fg(Color::Cyan))
            .value_style(Style::default().fg(Color::Black).bg(Color::Cyan)),
        area,
    );
}
fn render_chart(
    frame: &mut Frame,
    area: Rect,
    block: Block<'_>,
    view: &View,
    horizontal_label: &str,
    vertical_label: &str,
) {
    let values = view
        .series
        .get(&view.active)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if values.is_empty() {
        frame.render_widget(Paragraph::new("Waiting for samples.").block(block), area);
        return;
    }
    let points = values
        .iter()
        .map(|(time, value)| (*time, *value as f64))
        .collect::<Vec<_>>();
    let min = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let max = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let padding = ((max - min) * 0.1).max(1.0);
    let ymin = (min - padding).max(0.0);
    let ymax = max + padding;
    let start = points[0].0;
    let end = points.last().unwrap().0.max(start + 1.0);
    let dataset = Dataset::default()
        .name(safe(&view.active))
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(&points);
    frame.render_widget(
        Chart::new(vec![dataset])
            .block(block)
            .x_axis(
                Axis::default()
                    .title(safe(horizontal_label))
                    .bounds([start, end])
                    .labels([
                        Line::from(format!("{start:.0}")),
                        Line::from(format!("{end:.0}")),
                    ]),
            )
            .y_axis(
                Axis::default()
                    .title(safe(vertical_label))
                    .bounds([ymin, ymax])
                    .labels([
                        Line::from(format!("{ymin:.0}")),
                        Line::from(format!("{ymax:.0}")),
                    ]),
            ),
        area,
    );
}
struct Signals {
    cancel: Arc<AtomicBool>,
    ids: Vec<signal_hook::SigId>,
}
impl Signals {
    fn new() -> Result<Self> {
        let mut guard = Self {
            cancel: Arc::new(AtomicBool::new(false)),
            ids: Vec::new(),
        };
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT] {
            guard
                .ids
                .push(signal_hook::flag::register(signal, guard.cancel.clone())?);
        }
        Ok(guard)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for id in &self.ids {
            signal_hook::low_level::unregister(*id);
        }
    }
}
struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

struct InputReader {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl InputReader {
    fn start(
        sender: mpsc::SyncSender<std::io::Result<TerminalEvent>>,
        wake_sender: SyncSender<()>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::Builder::new()
            .name("flow-ui-input".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    let result = match event::poll(Duration::from_millis(20)) {
                        Ok(true) => event::read(),
                        Ok(false) => continue,
                        Err(error) => Err(error),
                    };
                    let failed = result.is_err();
                    if !send_terminal_event(result, &sender, &wake_sender, &thread_stop) {
                        break;
                    }
                    if failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    fn unpark(&self) {
        if let Some(handle) = &self.handle {
            handle.thread().unpark();
        }
    }
}

fn send_terminal_event(
    mut message: std::io::Result<TerminalEvent>,
    sender: &mpsc::SyncSender<std::io::Result<TerminalEvent>>,
    wake_sender: &SyncSender<()>,
    stop: &AtomicBool,
) -> bool {
    loop {
        match sender.try_send(message) {
            Ok(()) => {
                let _ = wake_sender.try_send(());
                return true;
            }
            Err(mpsc::TrySendError::Full(queued)) => {
                message = queued;
                if stop.load(Ordering::Acquire) {
                    return false;
                }
                thread::park();
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}
pub fn snapshot(screen: &Screen, registry: &Registry, options: &RunOptions) -> Result<Json> {
    super::syntax::validate(screen, registry)?;
    let signals = Signals::new()?;
    let mut app = App::new(screen, registry, options);
    let deadline = Instant::now()
        .checked_add(
            app.options
                .command_timeout
                .unwrap_or(Duration::from_secs(30))
                .saturating_add(Duration::from_secs(5)),
        )
        .context("UI timeout is too large")?;
    loop {
        if signals.cancel.load(Ordering::Relaxed) {
            bail!("UI cancelled by signal");
        }
        app.advance(false);
        if app.idle() || app.closing {
            break;
        }
        if app.instances.iter().all(|v| v.job.is_none()) {
            let missing = app
                .instances
                .iter()
                .flat_map(|instance| {
                    instance.queue.iter().flat_map(|(handler, _, _)| {
                        instance.required_inputs[*handler]
                            .iter()
                            .filter(|port| !instance.inputs.contains_key(*port))
                            .map(|port| format!("{}.{}", instance.name, port))
                    })
                })
                .collect::<BTreeSet<_>>();
            bail!(
                "headless UI is waiting for required inputs: {}",
                missing.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        if Instant::now() > deadline {
            bail!("headless UI exceeded initialization deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
    if let Some(instance) = app.instances.iter().find(|v| v.error.is_some()) {
        bail!("{}: {}", instance.name, instance.error.as_ref().unwrap());
    }
    Ok(app.snapshot())
}
pub fn run(
    screen: &Screen,
    registry: &Registry,
    options: &RunOptions,
    max_runs: Option<usize>,
) -> Result<()> {
    super::syntax::validate(screen, registry)?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("TUI requires a terminal; use --headless for a single JSON snapshot");
    }
    if options.trace {
        bail!("interactive TUI does not allow --trace; use --headless --trace");
    }
    let signals = Signals::new()?;
    let mut app = App::new(screen, registry, options);
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (wake_sender, wake_receiver) = mpsc::sync_channel(1);
    app.wake_sender = Some(wake_sender.clone());
    let (input_sender, input_receiver) = mpsc::sync_channel(64);
    let input_reader = InputReader::start(input_sender, wake_sender)?;
    loop {
        // Wake notifications coalesce; advance and the input queue remain the source of truth.
        let _ = wake_receiver.try_recv();
        if signals.cancel.load(Ordering::Relaxed) {
            break;
        }
        app.advance(max_runs.is_none_or(|max| app.jobs_done < max));
        if app.closing {
            break;
        }
        if app.dirty {
            terminal.draw(|frame| app.render(frame))?;
            app.dirty = false;
        }
        if max_runs.is_some_and(|max| app.jobs_done >= max) && app.idle() {
            break;
        }
        match input_receiver.try_recv() {
            Ok(message) => {
                input_reader.unpark();
                match message? {
                    TerminalEvent::Key(key) => {
                        if app.key(key) {
                            break;
                        }
                    }
                    TerminalEvent::Resize(_, _) => app.dirty = true,
                    TerminalEvent::Paste(value) => {
                        if let Some(&(i, j)) = app.focus.get(app.focused) {
                            if app.instances[i].component.widgets[j].kind == Kind::Input {
                                let clean = safe(&value).replace(['\n', '\t'], "");
                                let text = &mut app.instances[i].views[j].text;
                                for c in clean.chars() {
                                    if text.len() + c.len_utf8() > 512 {
                                        break;
                                    }
                                    text.push(c);
                                }
                                app.dirty = true;
                            }
                        }
                    }
                    _ => {}
                }
                // Advance before taking another event so each handler sees the prior result.
                continue;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => bail!("terminal input reader stopped"),
        }
        // The 20 ms wait remains a backstop for timers and process signals.
        let _ = wake_receiver.recv_timeout(Duration::from_millis(20));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn legacy_snapshot_projection(app: &App) -> Json {
        let programs = app
            .instances
            .iter()
            .map(|instance| {
                let widgets = instance
                    .component
                    .widgets
                    .iter()
                    .zip(&instance.views)
                    .map(|(widget, view)| {
                        (
                            widget.id.clone(),
                            json!({
                                "text": view.text,
                                "rows": view.rows,
                                "bars": view.bars.iter().map(|(label, value)| json!({
                                    "label": label,
                                    "value": value
                                })).collect::<Vec<_>>(),
                                "active_series": view.active,
                                "series": view.series
                            }),
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                (
                    instance.name.clone(),
                    json!({
                        "inputs": instance.inputs,
                        "widgets": widgets,
                        "error": instance.error
                    }),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        json!({
            "screen": app.screen.title,
            "completed_jobs": app.jobs_done,
            "programs": programs,
            "frame": rendered
        })
    }

    #[test]
    fn snapshot_projection_matches_legacy_for_unicode_control_and_multiple_components() {
        let source = concat!(
            "open a screen titled \"snapshot equivalence\"\n",
            "divide the screen into two columns\n",
            "place first on the left\n",
            "place second on the right\n",
            "define a program named first\n",
            "  accept input as text\n",
            "  show a text panel named status\n",
            "  show a table named records\n",
            "    column \"Value\" from \"value\"\n",
            "    column \"Count\" from \"count\"\n",
            "  show a bar chart named totals\n",
            "    label each bar from \"label\"\n",
            "    size each bar from \"count\"\n",
            "    label bar values with \"%\"\n",
            "  show a line chart named history with key \"group\"\n",
            "define a program named second\n",
            "  accept input as text\n",
            "  show a text field named query with value \"starting\"\n",
            "  show a text panel named notes\n",
        );
        let mut app = test_app(source);
        assert_eq!(app.instances.len(), 2);
        let first = app
            .instances
            .iter()
            .position(|instance| instance.name == "first")
            .unwrap();
        let second = app
            .instances
            .iter()
            .position(|instance| instance.name == "second")
            .unwrap();
        app.instances[first]
            .inputs
            .insert("input".into(), "input\u{0001}λ".into());
        app.instances[second]
            .inputs
            .insert("input".into(), "second\0input\n界".into());
        app.instances[first].error = Some("error\u{001b}[2Jλ".into());

        {
            let instance = &mut app.instances[first];
            for (widget, view) in instance.component.widgets.iter().zip(&mut instance.views) {
                match &widget.kind {
                    Kind::Text => view.text = "status\0\n東京".into(),
                    Kind::Table => {
                        view.rows = vec![
                            json!({
                                "value": "repository\u{0001}λ\n界",
                                "count": 7,
                                "extra": {"nested": [true, null, "✓"]}
                            }),
                            json!({"value": "control\0value", "count": 0}),
                        ];
                    }
                    Kind::Bars => {
                        view.bars = vec![("bar\u{0001}α".into(), 1.5), ("idle\0bar".into(), 0.0)];
                    }
                    Kind::Chart => {
                        let active = "series\u{0001}✓".to_string();
                        view.active = active.clone();
                        view.series
                            .insert(active, vec![(0.25, 7), (1.5, 0), (2.0, 19)]);
                    }
                    _ => {}
                }
            }
        }
        {
            let instance = &mut app.instances[second];
            for (widget, view) in instance.component.widgets.iter().zip(&mut instance.views) {
                match &widget.kind {
                    Kind::Input => view.text = "query\0draft\n雪".into(),
                    Kind::Text => view.text = "notes\u{001b}✓".into(),
                    _ => {}
                }
            }
        }

        assert_eq!(app.snapshot(), legacy_snapshot_projection(&app));
    }

    fn table_widget() -> Widget {
        Widget {
            id: "records".into(),
            kind: Kind::Table,
            initial: String::new(),
            items: Vec::new(),
            columns: vec![
                ("Repository".into(), "name".into()),
                ("Stars".into(), "stars".into()),
                ("Forks".into(), "forks".into()),
            ],
            rows: None,
            key: None,
            horizontal_label: String::new(),
            vertical_label: String::new(),
            bar_label: None,
            bar_value: None,
            bar_suffix: String::new(),
        }
    }

    fn legacy_render_table(
        frame: &mut Frame,
        area: Rect,
        block: Block<'_>,
        widget: &Widget,
        view: &View,
    ) {
        let rows = view.rows.iter().map(|row| {
            Row::new(
                widget
                    .columns
                    .iter()
                    .map(|(_, field)| safe(&display_value(&row[field])).into_owned())
                    .collect::<Vec<_>>(),
            )
        });
        let widths = widget
            .columns
            .iter()
            .enumerate()
            .map(|(n, _)| {
                if n == 0 {
                    Constraint::Percentage(40)
                } else {
                    Constraint::Percentage(
                        (60 / (widget.columns.len().saturating_sub(1).max(1))) as u16,
                    )
                }
            })
            .collect::<Vec<_>>();
        let header = Row::new(widget.columns.iter().map(|(title, _)| safe(title)))
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(
            Table::new(rows, widths)
                .header(header)
                .block(block)
                .column_spacing(1),
            area,
        );
    }

    #[test]
    fn table_viewport_render_matches_full_row_render() {
        let widget = table_widget();
        let mut view = View::new(&widget);
        view.rows = (0..40)
            .map(|index| {
                json!({
                    "name": format!("repo-{index:02}\0界"),
                    "stars": index,
                    "forks": index * 2,
                })
            })
            .collect();

        for (width, height, row_count) in [
            (100, 30, 40),
            (48, 10, 40),
            (45, 5, 40),
            (100, 4, 40),
            (20, 3, 40),
            (100, 30, 2),
            (100, 30, 0),
        ] {
            let mut case_view = view.clone();
            case_view.rows.truncate(row_count);
            let block = || {
                Block::default()
                    .borders(Borders::ALL)
                    .title(safe(&widget.id))
                    .border_style(Style::default().fg(Color::Yellow))
            };
            let mut expected = Terminal::new(TestBackend::new(width, height)).unwrap();
            expected
                .draw(|frame| {
                    legacy_render_table(frame, frame.area(), block(), &widget, &case_view)
                })
                .unwrap();
            let mut actual = Terminal::new(TestBackend::new(width, height)).unwrap();
            actual
                .draw(|frame| render_table(frame, frame.area(), block(), &widget, &case_view))
                .unwrap();

            assert!(
                actual.backend().buffer() == expected.backend().buffer(),
                "render differs at {width}x{height} with {row_count} rows"
            );
        }
        assert_eq!(
            view.rows.len(),
            40,
            "rendering must not truncate snapshot data"
        );

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let registry = Registry::load(&root.join("manifests")).unwrap();
        let screen =
            super::super::syntax::parse(include_str!("../../examples/top-cpu.program")).unwrap();
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: root.to_owned(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024 * 1024,
            command_timeout: None,
        };
        let mut app = App::new(&screen, &registry, &options);
        let (instance_index, widget_index) = app
            .instances
            .iter()
            .enumerate()
            .find_map(|(instance_index, instance)| {
                instance
                    .component
                    .widgets
                    .iter()
                    .position(|widget| widget.kind == Kind::Table)
                    .map(|widget_index| (instance_index, widget_index))
            })
            .expect("fixture has a table");
        app.instances[instance_index].views[widget_index].rows = view.rows.clone();
        let instance_name = app.instances[instance_index].name.clone();
        let widget_id = app.instances[instance_index].component.widgets[widget_index]
            .id
            .clone();
        let snapshot = app.snapshot();
        assert_eq!(
            snapshot["programs"][instance_name.as_str()]["widgets"][widget_id.as_str()]["rows"]
                .as_array()
                .unwrap()
                .len(),
            view.rows.len(),
            "snapshot must retain every source row"
        );
    }

    #[test]
    fn table_cow_cells_match_legacy_for_json_types_and_controls() {
        let widget = table_widget();
        let mut view = View::new(&widget);
        view.rows = vec![
            json!({
                "name": "repo\u{0001} λ\n界\tend",
                "stars": 7,
                "forks": {"nested": [true, null, "control\u{0085}✓"]}
            }),
            json!({
                "name": null,
                "stars": "escape\u{001b}[2J\tline\n雪",
                "forks": 0.25
            }),
            json!({
                "name": ["array", "cell"],
                "stars": {"nested": {"number": 3}},
                "forks": false
            }),
        ];
        for (width, height) in [(100, 30), (48, 10), (45, 5)] {
            let block = || {
                Block::default()
                    .borders(Borders::ALL)
                    .title(safe(&widget.id))
                    .border_style(Style::default().fg(Color::Yellow))
            };
            let mut expected = Terminal::new(TestBackend::new(width, height)).unwrap();
            expected
                .draw(|frame| legacy_render_table(frame, frame.area(), block(), &widget, &view))
                .unwrap();
            let mut actual = Terminal::new(TestBackend::new(width, height)).unwrap();
            actual
                .draw(|frame| render_table(frame, frame.area(), block(), &widget, &view))
                .unwrap();
            assert!(
                actual.backend().buffer() == expected.backend().buffer(),
                "borrowed table cells differ at {width}x{height}"
            );
        }
    }

    #[test]
    fn graph_renders_zero_flat_and_increasing_counts_and_small_screen() {
        for samples in [
            vec![],
            vec![(0.0, 0)],
            vec![(0.0, 5), (1.0, 5)],
            vec![(0.0, 5), (1.0, 9), (2.0, 20)],
        ] {
            let mut view = View::new(&Widget {
                id: "history".into(),
                kind: Kind::Chart,
                initial: String::new(),
                items: vec![],
                columns: vec![],
                rows: None,
                key: None,
                horizontal_label: "time".into(),
                vertical_label: "value".into(),
                bar_label: None,
                bar_value: None,
                bar_suffix: String::new(),
            });
            view.series.insert("repo".into(), samples);
            view.active = "repo".into();
            for size in [(60, 12), (1, 1)] {
                let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
                terminal
                    .draw(|frame| {
                        render_chart(
                            frame,
                            frame.area(),
                            Block::default().borders(Borders::ALL).title("history"),
                            &view,
                            "time",
                            "value",
                        )
                    })
                    .unwrap();
            }
        }
    }
    #[test]
    fn control_sequences_are_removed_from_plain_views() {
        assert_eq!(safe("hello\x1b[2J\x00\r\nworld"), "hello[2J\nworld");
    }

    #[test]
    fn incoming_json_keeps_repository_histories_separate_and_bounded() {
        let source = "open a screen titled \"test\"\nplace graph on the left\ndefine a program named graph\n  accept sample as text\n  show a line chart named history with key \"repo\"\n  whenever sample arrives\n    add field \"stars\" to history\n";
        let screen = super::super::syntax::parse(source).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let registry = Registry::load(&root.join("manifests")).unwrap();
        super::super::syntax::validate(&screen, &registry).unwrap();
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: root.to_owned(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 4096,
            command_timeout: None,
        };
        let mut app = App::new(&screen, &registry, &options);
        for value in 0..125 {
            app.enqueue(
                0,
                &Event::Input("sample".into()),
                json!({"repo":"one/repo","stars":value}).to_string(),
                String::new(),
            );
            app.advance(false);
        }
        let view = &app.instances[0].views[0];
        assert_eq!(view.series["one/repo"].len(), 120);
        assert_eq!(view.series["one/repo"][0].1, 5);
        for n in 0..20 {
            app.enqueue(
                0,
                &Event::Input("sample".into()),
                json!({"repo":format!("other/repo{n}"),"stars":900+n}).to_string(),
                String::new(),
            );
            app.advance(false);
        }
        assert_eq!(app.instances[0].views[0].series.len(), 16);
        assert_eq!(app.instances[0].views[0].active, "other/repo19");
        assert_eq!(app.instances[0].views[0].series["other/repo19"][0].1, 919);
    }
    #[test]
    fn timer_runs_only_when_due_and_headless_skips_ticks() {
        let source="open a screen titled \"test\"\nplace clock on the left\ndefine a program named clock\n  show a badge named status with text \"waiting\"\n  every 60 seconds\n    show \"ticked\" in status\n";
        let screen = super::super::syntax::parse(source).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let registry = Registry::load(&root.join("manifests")).unwrap();
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: root.to_owned(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 4096,
            command_timeout: None,
        };
        let mut app = App::new(&screen, &registry, &options);
        app.advance(true);
        assert_eq!(app.instances[0].views[0].text, "waiting");
        app.instances[0].ticks[0] = Some(Instant::now() - Duration::from_secs(1));
        app.advance(false);
        assert_eq!(app.instances[0].views[0].text, "waiting");
        app.advance(true);
        assert_eq!(app.instances[0].views[0].text, "ticked");
        assert!(app.instances[0].ticks[0].unwrap() > Instant::now());
    }

    fn test_app(source: &str) -> App {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let registry = Registry::load(&root.join("manifests")).unwrap();
        let screen = super::super::syntax::parse(source).unwrap();
        super::super::syntax::validate(&screen, &registry).unwrap();
        App::new(
            &screen,
            &registry,
            &RunOptions {
                inputs: Default::default(),
                working_directory: root.to_owned(),
                cache_directory: None,
                trace: false,
                max_output_bytes: 4096,
                command_timeout: None,
            },
        )
    }
    fn finish_jobs(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.idle() {
            assert!(Instant::now() < deadline, "worker did not complete");
            app.advance(false);
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn widget_snapshot_tracks_prior_script_writes_within_handler() {
        let mut app = test_app(concat!(
            "open a screen titled \"write-read snapshot\"\n",
            "place app on the left\n",
            "define a program named app\n",
            "  accept incoming as text\n",
            "  show a text field named query with value \"old\"\n",
            "  show a text panel named output\n",
            "  whenever incoming arrives\n",
            "    show \"updated\" in query\n",
            "    then show the value of query in output\n",
        ));
        app.instances[0].queue.clear();
        app.enqueue(
            0,
            &Event::Input("incoming".into()),
            "trigger".into(),
            String::new(),
        );
        app.advance(false);

        let query = app.widget(0, "query").unwrap();
        let output = app.widget(0, "output").unwrap();
        assert_eq!(app.instances[0].views[query].text, "updated");
        assert_eq!(app.instances[0].views[output].text, "updated");
    }

    #[test]
    #[ignore = "explicit release benchmark, not a regression assertion"]
    fn benchmark_ui_dispatch_32_widgets_32_literals() {
        use std::hint::black_box;

        const WIDGETS: usize = 32;
        const WARMUP_BATCHES: usize = 3;
        const BATCHES: usize = 23;
        const OPERATIONS_PER_BATCH: usize = 20;
        const LITERAL_BYTES: usize = 512;
        let mut source = String::from(
            "open a screen titled \"dispatch benchmark\"\nplace app on the left\ndefine a program named app\n  accept incoming as text\n",
        );
        for widget in 0..WIDGETS {
            source.push_str(&format!("  show a text panel named output{widget}\n"));
        }
        source.push_str("  whenever incoming arrives\n");
        let literal = "x".repeat(LITERAL_BYTES);
        for widget in 0..WIDGETS {
            source.push_str(&format!("    show \"{literal}\" in output{widget}\n"));
        }
        let mut app = test_app(&source);
        app.instances[0].queue.clear();
        let event = Event::Input("incoming".into());
        let mut samples_us = Vec::new();
        for batch in 0..BATCHES {
            let start = Instant::now();
            for _ in 0..OPERATIONS_PER_BATCH {
                app.enqueue(0, &event, String::new(), String::new());
                app.advance(false);
            }
            if batch >= WARMUP_BATCHES {
                samples_us.push(start.elapsed().as_secs_f64() * 1e6 / OPERATIONS_PER_BATCH as f64);
            }
        }
        samples_us.sort_by(f64::total_cmp);
        let percentile = |rank: usize| {
            let index = (samples_us.len() * rank).div_ceil(100).saturating_sub(1);
            samples_us[index]
        };
        let report = json!({
            "operation": "ui_dispatch_32_widgets_32_literals_512_bytes",
            "p50_us": percentile(50),
            "p95_us": percentile(95),
            "p99_us": percentile(99),
            "batches": BATCHES,
            "measured_batches": BATCHES - WARMUP_BATCHES,
            "operations_per_batch": OPERATIONS_PER_BATCH,
            "warmup_batches": WARMUP_BATCHES,
            "widgets": WIDGETS,
            "literal_actions": WIDGETS,
            "literal_bytes_each": LITERAL_BYTES,
            "fixture": "32 text panels; each input event writes 32 distinct 512-byte literals; event enqueue plus App::advance",
        });
        let path = std::env::var("UI_DISPATCH_BENCH_REPORT").expect("set UI_DISPATCH_BENCH_REPORT");
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        black_box(&app);
    }

    #[test]
    fn script_writes_update_native_bindings_and_subsequent_widget_reads() {
        let mut app = test_app("open a screen titled \"order\"\nplace app on the left\ndefine a program named app\n  show a text field named query with value \"old\"\n  show a text panel named output\n  whenever the program opens\n    show \"new\" in query\n    run printf with message query\n    then show the result in output\n");
        finish_jobs(&mut app);
        assert_eq!(app.instances[0].views[0].text, "new");
        assert_eq!(app.instances[0].views[1].text, "new\n");
    }
    #[test]
    fn pending_worker_and_post_worker_send_share_a_snapshot_despite_draft_edits() {
        let mut app = test_app("open a screen titled \"snapshot\"\nplace source on the left\nplace target on the right\nconnect done from source to received in target\ndefine a program named source\n  offer done as text\n  show a text field named query with value \"old\"\n  show a text panel named output\n  whenever the program opens\n    run printf with message query\n    then show the result in output\n    then send the value of query as done\ndefine a program named target\n  accept received as text\n  show a text panel named output\n  whenever received arrives\n    show the result in output\n");
        app.advance(false);
        assert!(app.instances[0].job.is_some());
        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "draft".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        finish_jobs(&mut app);
        assert_eq!(app.instances[0].views[0].text, "draft");
        assert_eq!(app.instances[0].views[1].text, "old\n");
        assert_eq!(app.instances[1].views[0].text, "old");
    }
    #[test]
    fn missing_required_port_defers_entire_handler_and_unused_ports_do_not_block() {
        let source = "open a screen titled \"inputs\"\nplace app on the left\ndefine a program named app\n  accept a as text\n  accept b as text\n  show a text panel named status with text \"waiting\"\n  whenever a arrives\n    show \"prefix\" in status\n    run printf with message b\n    then show the result in status\n";
        let mut app = test_app(source);
        app.instances[0].inputs.insert("a".into(), "first".into());
        app.enqueue(0, &Event::Input("a".into()), "first".into(), String::new());
        app.advance(false);
        assert_eq!(app.instances[0].views[0].text, "waiting");
        assert!(app.instances[0].job.is_none());
        assert_eq!(app.instances[0].queue.len(), 1);
        app.instances[0].inputs.insert("b".into(), "ready".into());
        finish_jobs(&mut app);
        assert_eq!(app.instances[0].views[0].text, "ready\n");
        assert_eq!(app.jobs_done, 1);
        let mut app = test_app(&source.replace("message b", "message a"));
        app.instances[0].inputs.insert("a".into(), "only-a".into());
        app.enqueue(0, &Event::Input("a".into()), "only-a".into(), String::new());
        finish_jobs(&mut app);
        assert_eq!(app.instances[0].views[0].text, "only-a\n");
    }
    #[test]
    fn invalid_dynamic_text_field_update_is_atomic() {
        let mut app = test_app("open a screen titled \"bounds\"\nplace app on the left\ndefine a program named app\n  accept incoming as text\n  show a text field named query with value \"original\"\n  whenever incoming arrives\n    show the result in query\n");
        for value in ["x".repeat(513), "bad\0value".into()] {
            app.enqueue(0, &Event::Input("incoming".into()), value, String::new());
            app.advance(false);
            assert!(app.instances[0].error.is_some());
            assert_eq!(app.instances[0].views[0].text, "original");
        }
    }
    #[test]
    fn shortcut_control_flow_changes_with_only_the_program_text() {
        let source="open a screen titled \"test\"\nplace panel on the left\ndefine a program named panel\n  show a badge named status with text \"waiting\"\n  show a button named refresh labelled \"Do work\"\n  whenever refresh is pressed\n    show \"first\" in status\n    then show \"second\" in status\n";
        let mut app = test_app(source);
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.advance(false);
        assert_eq!(
            app.instances[0].views[0].text, "waiting",
            "button name must not create a hidden shortcut"
        );
        let bound = format!("{source}  whenever key \"Ctrl-Y\" is pressed\n    press refresh\n");
        let renamed = bound.replace("refresh", "fetch");
        let mut app = test_app(&renamed);
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.advance(false);
        assert_eq!(app.instances[0].views[0].text, "waiting");
        app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        app.advance(false);
        assert_eq!(
            app.instances[0].views[0].text, "second",
            "script determines routing and action order"
        );
        let changed = renamed.replace("Ctrl-Y", "Ctrl-R");
        let mut app = test_app(&changed);
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.advance(false);
        assert_eq!(app.instances[0].views[0].text, "second");
    }
    #[test]
    fn quit_is_a_script_action_and_closes_before_later_queued_work() {
        let source="open a screen titled \"test\"\nplace panel on the left\ndefine a program named panel\n  show a badge named status with text \"waiting\"\n";
        let mut app = test_app(source);
        assert!(!app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)));
        app.advance(false);
        assert!(!app.closing);
        let bound=format!("{source}  whenever key \"Ctrl-Q\" is pressed\n    close the screen\n  whenever key \"Ctrl-Y\" is pressed\n    show \"should not run\" in status\n");
        let mut app = test_app(&bound);
        app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        app.advance(false);
        assert!(app.closing);
        assert_eq!(app.instances[0].views[0].text, "waiting");
        assert!(
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            "host emergency interruption stays available"
        );
    }
    #[test]
    fn application_error_display_requires_a_script_handler() {
        let source="open a screen titled \"test\"\nplace panel on the left\ndefine a program named panel\n  accept sample as text\n  show a badge named status with text \"untouched\"\n  show a line chart named history\n  whenever sample arrives\n    add field \"count\" to history\n";
        let mut app = test_app(source);
        app.enqueue(
            0,
            &Event::Input("sample".into()),
            "{}".into(),
            String::new(),
        );
        app.advance(false);
        assert!(app.instances[0].error.is_some());
        assert_eq!(app.instances[0].views[0].text, "untouched");
        let bound = format!("{source}  whenever a command fails\n    show the error in status\n");
        let mut app = test_app(&bound);
        app.enqueue(
            0,
            &Event::Input("sample".into()),
            "{}".into(),
            String::new(),
        );
        app.advance(false);
        assert!(app.instances[0].views[0]
            .text
            .contains("nonnegative integer"));
    }

    #[test]
    fn bars_accept_fractional_values_and_reject_invalid_data_without_partial_update() {
        let source="open a screen titled \"test\"\nplace panel on the left\ndefine a program named panel\n  accept sample as text\n  show a bar chart named usage\n    label each bar from \"name\"\n    size each bar from \"value\"\n    label bar values with \"%\"\n  whenever sample arrives\n    show JSON as bars in usage\n";
        let mut app = test_app(source);
        app.enqueue(0,&Event::Input("sample".into()),r#"[{"name":"fraction","value":0.1},{"name":"multi","value":150.5},{"name":"idle","value":0}]"#.into(),String::new());
        app.advance(false);
        assert_eq!(
            app.instances[0].views[0].bars,
            vec![
                ("fraction".into(), 0.1),
                ("multi".into(), 150.5),
                ("idle".into(), 0.0)
            ]
        );
        let original = app.instances[0].views[0].bars.clone();
        for data in [
            r#"[{"name":"bad","value":-1}]"#,
            r#"[{"name":"bad","value":"1"}]"#,
            r#"[{"name":"bad"}]"#,
            r#"[{"name":10,"value":1}]"#,
            r#"{}"#,
        ] {
            app.enqueue(
                0,
                &Event::Input("sample".into()),
                data.into(),
                String::new(),
            );
            app.advance(false);
            assert!(app.instances[0].error.is_some());
            assert_eq!(app.instances[0].views[0].bars, original);
        }
        app.enqueue(
            0,
            &Event::Input("sample".into()),
            r#"[{"name":"fixed","value":1.5}]"#.into(),
            String::new(),
        );
        app.advance(false);
        assert!(
            app.instances[0].error.is_none(),
            "valid new data must clear the previous validation failure"
        );
        for size in [(1, 1), (4, 4), (80, 16), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
            let mut view = app.instances[0].views[0].clone();
            view.bars[0].0 = "a long 東京 label with \x1b[2J controls".repeat(20);
            terminal
                .draw(|frame| {
                    render_bars(
                        frame,
                        frame.area(),
                        Block::default().borders(Borders::ALL),
                        &view,
                        "%",
                    )
                })
                .unwrap();
        }
    }

    #[test]
    fn bar_scale_uses_the_largest_value_even_when_below_one() {
        let widget = Widget {
            id: "usage".into(),
            kind: Kind::Bars,
            initial: String::new(),
            items: vec![],
            columns: vec![],
            rows: None,
            key: None,
            horizontal_label: String::new(),
            vertical_label: String::new(),
            bar_label: Some("name".into()),
            bar_value: Some("value".into()),
            bar_suffix: "%".into(),
        };
        let mut view = View::new(&widget);
        view.bars = vec![("a".into(), 0.1), ("b".into(), 0.2)];
        let mut terminal = Terminal::new(TestBackend::new(100, 5)).unwrap();
        terminal
            .draw(|frame| {
                render_bars(
                    frame,
                    frame.area(),
                    Block::default().borders(Borders::ALL),
                    &view,
                    "%",
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let full = (0..100).filter(|x| buffer[(*x, 2)].symbol() == "█").count();
        assert!(
            full > 85,
            "largest fractional bar should fill the available width, got {full}"
        );
        view.bars = vec![("idle".into(), 0.0)];
        terminal
            .draw(|frame| {
                render_bars(
                    frame,
                    frame.area(),
                    Block::default().borders(Borders::ALL),
                    &view,
                    "%",
                )
            })
            .unwrap();
    }
    #[test]
    fn final_field_projection_preserves_formats_raw_result_and_link_payload() {
        let source = concat!(
            "open a screen titled \"field result\"\n",
            "place source on the left\n",
            "place target on the right\n",
            "connect completed from source to received in target\n",
            "define a program named source\n",
            "  accept incoming as text\n",
            "  offer completed as text\n",
            "  show a text panel named numeric\n",
            "  show a text panel named null_value\n",
            "  show a text panel named nested\n",
            "  show a text panel named selected\n",
            "  show a text panel named original\n",
            "  whenever incoming arrives\n",
            "    show field \"count\" in numeric\n",
            "    then show field \"nothing\" in null_value\n",
            "    then show field \"nested\" in nested\n",
            "    then show field \"payload\" in selected\n",
            "    then show the result in original\n",
            "    then send the result as completed\n",
            "define a program named target\n",
            "  accept received as text\n",
            "  show a text panel named output\n",
            "  whenever received arrives\n",
            "    show the result in output\n",
        );
        let mut app = test_app(source);
        app.instances[0].queue.clear();
        let raw = r#"{"payload":"line\nλ","count":42,"nothing":null,"nested":{"answer":42}}"#;
        app.enqueue(
            0,
            &Event::Input("incoming".into()),
            raw.into(),
            String::new(),
        );
        app.advance(false);

        for (id, expected) in [
            ("numeric", "42"),
            ("null_value", ""),
            ("nested", r#"{"answer":42}"#),
            ("selected", "line\nλ"),
        ] {
            let widget = app.widget(0, id).unwrap();
            assert_eq!(app.instances[0].views[widget].text, expected, "{id}");
        }
        let original = app.widget(0, "original").unwrap();
        let output = app.widget(1, "output").unwrap();
        assert_eq!(app.instances[0].views[original].text, raw);
        assert_eq!(app.instances[1].views[output].text, raw);

        app.enqueue(
            0,
            &Event::Input("incoming".into()),
            r#"{"count":42,"nothing":null,"nested":{"answer":42},"other":"value"}"#.into(),
            String::new(),
        );
        app.advance(false);
        assert_eq!(
            app.instances[0].error.as_deref(),
            Some("missing field payload")
        );
        let selected = app.widget(0, "selected").unwrap();
        assert_eq!(app.instances[0].views[selected].text, "line\nλ");
    }

    #[test]
    #[ignore = "explicit release benchmark, not a regression assertion"]
    fn benchmark_ui_dispatch_large_field_projection() {
        use std::hint::black_box;

        const FIELD_BYTES: usize = 64 * 1024;
        const WARMUP_BATCHES: usize = 3;
        const BATCHES: usize = 13;
        const OPERATIONS_PER_BATCH: usize = 10;
        let source = concat!(
            "open a screen titled \"field dispatch\"\n",
            "place app on the left\n",
            "define a program named app\n",
            "  accept incoming as text\n",
            "  show a text panel named selected\n",
            "  whenever incoming arrives\n",
            "    show field \"payload\" in selected\n",
        );
        let mut app = test_app(source);
        app.instances[0].queue.clear();
        let value = "x".repeat(FIELD_BYTES);
        let raw = format!("{{\"payload\":{}}}", serde_json::to_string(&value).unwrap());
        let event = Event::Input("incoming".into());
        let mut samples_us = Vec::new();
        for batch in 0..BATCHES {
            let start = Instant::now();
            for _ in 0..OPERATIONS_PER_BATCH {
                app.enqueue(0, &event, raw.clone(), String::new());
                app.advance(false);
            }
            if batch >= WARMUP_BATCHES {
                samples_us.push(start.elapsed().as_secs_f64() * 1e6 / OPERATIONS_PER_BATCH as f64);
            }
        }
        samples_us.sort_by(f64::total_cmp);
        let percentile = |rank: usize| {
            let index = (samples_us.len() * rank).div_ceil(100).saturating_sub(1);
            samples_us[index]
        };
        let report = json!({
            "operation": "ui_dispatch_field_projection_64kib",
            "p50_us": percentile(50),
            "p95_us": percentile(95),
            "p99_us": percentile(99),
            "batches": BATCHES,
            "measured_batches": BATCHES - WARMUP_BATCHES,
            "operations_per_batch": OPERATIONS_PER_BATCH,
            "warmup_batches": WARMUP_BATCHES,
            "field_bytes": FIELD_BYTES,
            "fixture": "one accepted JSON event with one 64 KiB string field; timed enqueue and App::advance through parse and field projection",
        });
        let path = std::env::var("UI_FIELD_BENCH_REPORT").expect("set UI_FIELD_BENCH_REPORT");
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        black_box(&app);
    }

    #[test]
    fn final_rows_projection_keeps_result_text_and_link_payload() {
        let source = concat!(
            "open a screen titled \"rows move\"\n",
            "place source on the left\n",
            "place target on the right\n",
            "connect completed from source to received in target\n",
            "define a program named source\n",
            "  accept incoming as text\n",
            "  offer completed as text\n",
            "  show a table named first\n",
            "    column \"ID\" from \"id\"\n",
            "    column \"Value\" from \"value\"\n",
            "  show a table named second\n",
            "    column \"ID\" from \"id\"\n",
            "    column \"Value\" from \"value\"\n",
            "  show a text panel named receipt\n",
            "  whenever incoming arrives\n",
            "    show the result as rows in first\n",
            "    then show the result as rows in second\n",
            "    then show the result in receipt\n",
            "    then send the result as completed\n",
            "define a program named target\n",
            "  accept received as text\n",
            "  show a text panel named output\n",
            "  whenever received arrives\n",
            "    show the result in output\n",
        );
        let mut app = test_app(source);
        let raw = r#"[{"id":1,"value":"first λ"},{"id":2,"value":"line\n界"}]"#;
        app.enqueue(
            0,
            &Event::Input("incoming".into()),
            raw.into(),
            String::new(),
        );
        app.advance(false);

        let source = app
            .instances
            .iter()
            .position(|v| v.name == "source")
            .unwrap();
        let target = app
            .instances
            .iter()
            .position(|v| v.name == "target")
            .unwrap();
        let expected: Json = serde_json::from_str(raw).unwrap();
        let expected_rows = expected.as_array().unwrap();
        for table_id in ["first", "second"] {
            let widget = app.widget(source, table_id).unwrap();
            assert_eq!(&app.instances[source].views[widget].rows, expected_rows);
        }
        let receipt = app.widget(source, "receipt").unwrap();
        assert_eq!(app.instances[source].views[receipt].text, raw);
        let output = app.widget(target, "output").unwrap();
        assert_eq!(app.instances[target].views[output].text, raw);
    }

    #[test]
    fn rows_validation_stays_atomic_and_ordered_and_later_json_actions_still_work() {
        let source = concat!(
            "open a screen titled \"rows validation\"\n",
            "place app on the left\n",
            "define a program named app\n",
            "  accept incoming as text\n",
            "  accept detail as text\n",
            "  show a table named primary\n",
            "    column \"ID\" from \"id\"\n",
            "  show a table named secondary\n",
            "    column \"Required\" from \"required\"\n",
            "  show a text panel named detail\n",
            "  whenever incoming arrives\n",
            "    show the result as rows in primary\n",
            "    then show the result as rows in secondary\n",
            "  whenever detail arrives\n",
            "    show the result as rows in primary\n",
            "    then show field \"value\" in detail\n",
        );
        let mut app = test_app(source);
        let primary = app.widget(0, "primary").unwrap();
        let secondary = app.widget(0, "secondary").unwrap();
        app.instances[0].views[primary].rows = vec![json!({"id":"old"})];
        app.instances[0].views[secondary].rows = vec![json!({"required":"old"})];

        // The first action commits before a later action fails, preserving script order.
        app.enqueue(
            0,
            &Event::Input("incoming".into()),
            r#"[{"id":"new"}]"#.into(),
            String::new(),
        );
        app.advance(false);
        assert_eq!(
            app.instances[0].views[primary].rows,
            vec![json!({"id":"new"})]
        );
        assert_eq!(
            app.instances[0].views[secondary].rows,
            vec![json!({"required":"old"})]
        );
        assert!(app.instances[0]
            .error
            .as_deref()
            .unwrap()
            .contains("table record is missing field required"));

        // Every row is validated before this table is mutated.
        app.enqueue(
            0,
            &Event::Input("incoming".into()),
            r#"[{"id":"first"},{"required":"second"}]"#.into(),
            String::new(),
        );
        app.advance(false);
        assert_eq!(
            app.instances[0].views[primary].rows,
            vec![json!({"id":"new"})]
        );
        assert_eq!(
            app.instances[0].views[secondary].rows,
            vec![json!({"required":"old"})]
        );
        assert!(app.instances[0]
            .error
            .as_deref()
            .unwrap()
            .contains("table record is missing field id"));

        // A Rows action before another JSON consumer must leave the parsed
        // object available for the following Field action.
        app.enqueue(
            0,
            &Event::Input("detail".into()),
            r#"{"id":"later","value":"λ"}"#.into(),
            String::new(),
        );
        app.advance(false);
        assert_eq!(
            app.instances[0].views[primary].rows,
            vec![json!({"id":"later","value":"λ"})]
        );
        let detail = app.widget(0, "detail").unwrap();
        assert_eq!(app.instances[0].views[detail].text, "λ");
    }

    #[test]
    #[ignore = "explicit release benchmark, not a regression assertion"]
    fn benchmark_large_rows_dispatch_changed_and_unchanged() {
        use std::hint::black_box;

        const ROW_COUNT: usize = 10_000;
        const WARMUP_BATCHES: usize = 3;
        const BATCHES: usize = 13;
        const OPERATIONS_PER_BATCH: usize = 10;
        let source = concat!(
            "open a screen titled \"dispatch\"\n",
            "place app on the left\n",
            "define a program named app\n",
            "  accept incoming as text\n",
            "  show a table named first\n",
            "    column \"ID\" from \"id\"\n",
            "    column \"Value\" from \"value\"\n",
            "    column \"Count\" from \"count\"\n",
            "  show a table named second\n",
            "    column \"ID\" from \"id\"\n",
            "    column \"Value\" from \"value\"\n",
            "    column \"Count\" from \"count\"\n",
            "  whenever incoming arrives\n",
            "    show the result as rows in first\n",
            "    then show the result as rows in second\n",
        );
        let mut app = test_app(source);
        app.instances[0].queue.clear();
        let payload = |prefix: &str| {
            let rows = (0..ROW_COUNT)
                .map(|id| {
                    json!({
                        "id": id,
                        "value": format!("{prefix}-{id:05}"),
                        "count": (id % 1000) as u64
                    })
                })
                .collect::<Vec<_>>();
            serde_json::to_vec(&rows).unwrap()
        };
        let payload_a = payload("alpha");
        let payload_b = payload("bravo");
        let parsed_a: Json = serde_json::from_slice(&payload_a).unwrap();
        let rows_a = parsed_a.as_array().unwrap();
        let mut changed_app = app;
        let mut unchanged_app = test_app(source);
        unchanged_app.instances[0].queue.clear();
        let unchanged_first = unchanged_app.widget(0, "first").unwrap();
        let unchanged_second = unchanged_app.widget(0, "second").unwrap();
        unchanged_app.instances[0].views[unchanged_first].rows = rows_a.clone();
        unchanged_app.instances[0].views[unchanged_second].rows = rows_a.clone();

        let mut changed_samples = Vec::new();
        let mut unchanged_samples = Vec::new();
        for batch in 0..BATCHES {
            let inputs = (0..OPERATIONS_PER_BATCH)
                .map(|operation| {
                    if operation % 2 == 0 {
                        String::from_utf8(payload_a.clone()).unwrap()
                    } else {
                        String::from_utf8(payload_b.clone()).unwrap()
                    }
                })
                .collect::<Vec<_>>();
            let start = Instant::now();
            for input in inputs {
                changed_app.enqueue(0, &Event::Input("incoming".into()), input, String::new());
                changed_app.advance(false);
            }
            if batch >= WARMUP_BATCHES {
                changed_samples
                    .push(start.elapsed().as_secs_f64() * 1e6 / OPERATIONS_PER_BATCH as f64);
            }

            let inputs = (0..OPERATIONS_PER_BATCH)
                .map(|_| String::from_utf8(payload_a.clone()).unwrap())
                .collect::<Vec<_>>();
            let start = Instant::now();
            for input in inputs {
                unchanged_app.enqueue(0, &Event::Input("incoming".into()), input, String::new());
                unchanged_app.advance(false);
            }
            if batch >= WARMUP_BATCHES {
                unchanged_samples
                    .push(start.elapsed().as_secs_f64() * 1e6 / OPERATIONS_PER_BATCH as f64);
            }
        }
        changed_samples.sort_by(f64::total_cmp);
        unchanged_samples.sort_by(f64::total_cmp);
        let percentile = |samples: &[f64], rank: usize| {
            let index = (samples.len() * rank).div_ceil(100).saturating_sub(1);
            samples[index]
        };
        let report = json!({
            "operation": "two_table_rows_dispatch_10000_rows",
            "changed": {
                "p50_us": percentile(&changed_samples, 50),
                "p95_us": percentile(&changed_samples, 95),
                "p99_us": percentile(&changed_samples, 99)
            },
            "unchanged": {
                "p50_us": percentile(&unchanged_samples, 50),
                "p95_us": percentile(&unchanged_samples, 95),
                "p99_us": percentile(&unchanged_samples, 99)
            },
            "batches": BATCHES,
            "measured_batches": BATCHES - WARMUP_BATCHES,
            "operations_per_batch": OPERATIONS_PER_BATCH,
            "warmup_batches": WARMUP_BATCHES,
            "rows": ROW_COUNT,
            "tables": 2,
            "columns_per_table": 3,
            "timed_work": "event enqueue and App::advance through JSON parsing, validation, projection comparison and view update; payload strings prepared per batch"
        });
        let path = std::env::var("UI_ROWS_BENCH_REPORT").expect("set UI_ROWS_BENCH_REPORT");
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        black_box((&changed_app, &unchanged_app));
    }

    #[test]
    #[ignore = "explicit release benchmark, not a regression assertion"]
    fn benchmark_ui_operations() {
        use std::hint::black_box;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let registry = Registry::load(&root.join("manifests")).unwrap();
        let source = include_str!("../../examples/github-stars.program");
        let screen = super::super::syntax::parse(source).unwrap();
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: root.to_owned(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024 * 1024,
            command_timeout: None,
        };
        let mut app = App::new(&screen, &registry, &options);
        for instance in &mut app.instances {
            instance.queue.clear();
        }
        let chart = app.widget(1, "history").unwrap();
        app.instances[1].views[chart].active = "morluto/rea".into();
        app.instances[1].views[chart].series.insert(
            "morluto/rea".into(),
            (0..120).map(|n| (n as f64, 200000 + n)).collect(),
        );
        let table = app.widget(1, "metrics").unwrap();
        app.instances[1].views[table].rows =
            vec![json!({"nameWithOwner":"morluto/rea","stargazerCount":200120,"forkCount":50000})];
        let cpu_source = include_str!("../../examples/top-cpu.program");
        let cpu_screen = super::super::syntax::parse(cpu_source).unwrap();
        let mut cpu_app = App::new(&cpu_screen, &registry, &options);
        let bar = cpu_app.widget(0, "usage").unwrap();
        cpu_app.instances[0].views[bar].bars = (0..10)
            .map(|n| (format!("{} process-{n}", 100 + n), 100.0 / (n + 1) as f64))
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut results = BTreeMap::new();
        for operation in [
            "parse",
            "validate",
            "route_text_message",
            "render_100x30_120_points",
            "render_100x30_ten_bars",
        ] {
            let mut times = Vec::new();
            for batch in 0..110 {
                let start = Instant::now();
                for _ in 0..100 {
                    match operation {
                        "parse" => {
                            black_box(super::super::syntax::parse(black_box(source)).unwrap());
                        }
                        "validate" => {
                            super::super::syntax::validate(
                                black_box(&screen),
                                black_box(&registry),
                            )
                            .unwrap();
                        }
                        "route_text_message" => {
                            app.deliver(0, "repository", "morluto/rea".into());
                            app.instances[1].queue.clear();
                        }
                        "render_100x30_ten_bars" => {
                            terminal.draw(|frame| cpu_app.render(frame)).unwrap();
                        }
                        _ => {
                            terminal.draw(|frame| app.render(frame)).unwrap();
                        }
                    }
                }
                if batch >= 10 {
                    times.push(start.elapsed().as_secs_f64() * 1e6 / 100.0);
                }
            }
            times.sort_by(f64::total_cmp);
            results.insert(
                operation,
                json!({"p50_us":times[49],"p95_us":times[94],"p99_us":times[98]}),
            );
        }
        let report = json!({"operations":results,"batches":100,"operations_per_batch":100,"warmup_batches":10,"percentiles":"batch averages; render includes in-memory diff, excludes terminal I/O; routing excludes applets", "terminal_cells":[100,30],"chart_points":120});
        let path = std::env::var("UI_BENCH_REPORT").expect("set UI_BENCH_REPORT");
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }

    #[test]
    #[ignore = "explicit release benchmark, not a regression assertion"]
    fn benchmark_large_snapshot_projection() {
        use std::hint::black_box;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let registry = Registry::load(&root.join("manifests")).unwrap();
        let source = include_str!("../../examples/top-cpu.program");
        let screen = super::super::syntax::parse(source).unwrap();
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: root.to_owned(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024 * 1024,
            command_timeout: None,
        };
        let mut app = App::new(&screen, &registry, &options);
        for instance in &mut app.instances {
            instance.queue.clear();
        }
        let (instance_index, widget_index) = app
            .instances
            .iter()
            .enumerate()
            .find_map(|(instance_index, instance)| {
                instance
                    .component
                    .widgets
                    .iter()
                    .position(|widget| widget.kind == Kind::Table)
                    .map(|widget_index| (instance_index, widget_index))
            })
            .expect("fixture has a table");
        let fields = app.instances[instance_index].component.widgets[widget_index]
            .columns
            .iter()
            .map(|(_, field)| field.clone())
            .collect::<Vec<_>>();
        app.instances[instance_index].views[widget_index].rows = (0..10_000)
            .map(|row_index| {
                Json::Object(
                    fields
                        .iter()
                        .enumerate()
                        .map(|(column_index, field)| {
                            (
                                field.clone(),
                                Json::String(format!("{row_index}-{column_index}")),
                            )
                        })
                        .collect(),
                )
            })
            .collect();

        let mut samples_us = Vec::new();
        const WARMUP_BATCHES: usize = 3;
        const BATCHES: usize = 23;
        const OPERATIONS_PER_BATCH: usize = 20;
        for batch in 0..BATCHES {
            let start = Instant::now();
            for _ in 0..OPERATIONS_PER_BATCH {
                black_box(app.snapshot());
            }
            if batch >= WARMUP_BATCHES {
                samples_us.push(start.elapsed().as_secs_f64() * 1e6 / OPERATIONS_PER_BATCH as f64);
            }
        }
        samples_us.sort_by(f64::total_cmp);
        let percentile = |rank: usize| {
            let index = (samples_us.len() * rank).div_ceil(100).saturating_sub(1);
            samples_us[index]
        };
        let report = json!({
            "operation": "snapshot_100x30_10000_table_rows",
            "p50_us": percentile(50),
            "p95_us": percentile(95),
            "p99_us": percentile(99),
            "batches": BATCHES,
            "measured_batches": BATCHES - WARMUP_BATCHES,
            "operations_per_batch": OPERATIONS_PER_BATCH,
            "warmup_batches": WARMUP_BATCHES,
            "rows": 10000,
            "columns": fields.len(),
            "fixture": "examples/top-cpu.program; snapshot API includes the 100x30 in-memory frame",
        });
        let path = std::env::var("UI_SNAPSHOT_BENCH_REPORT").expect("set UI_SNAPSHOT_BENCH_REPORT");
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }

    #[test]
    #[ignore = "explicit release benchmark, not a regression assertion"]
    fn benchmark_large_table_viewport() {
        use std::hint::black_box;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let registry = Registry::load(&root.join("manifests")).unwrap();
        let source = include_str!("../../examples/top-cpu.program");
        let screen = super::super::syntax::parse(source).unwrap();
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: root.to_owned(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024 * 1024,
            command_timeout: None,
        };
        let mut app = App::new(&screen, &registry, &options);
        for instance in &mut app.instances {
            instance.queue.clear();
        }
        let (instance_index, widget_index) = app
            .instances
            .iter()
            .enumerate()
            .find_map(|(instance_index, instance)| {
                instance
                    .component
                    .widgets
                    .iter()
                    .position(|widget| widget.kind == Kind::Table)
                    .map(|widget_index| (instance_index, widget_index))
            })
            .expect("fixture has a table");
        let fields = app.instances[instance_index].component.widgets[widget_index]
            .columns
            .iter()
            .map(|(_, field)| field.clone())
            .collect::<Vec<_>>();
        app.instances[instance_index].views[widget_index].rows = (0..10_000)
            .map(|row_index| {
                Json::Object(
                    fields
                        .iter()
                        .enumerate()
                        .map(|(column_index, field)| {
                            (
                                field.clone(),
                                Json::String(format!("{row_index}-{column_index}")),
                            )
                        })
                        .collect(),
                )
            })
            .collect();

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut samples_us = Vec::new();
        const WARMUP_BATCHES: usize = 3;
        const BATCHES: usize = 23;
        const OPERATIONS_PER_BATCH: usize = 20;
        for batch in 0..BATCHES {
            let start = Instant::now();
            for _ in 0..OPERATIONS_PER_BATCH {
                terminal.draw(|frame| app.render(frame)).unwrap();
                black_box(terminal.backend().buffer());
            }
            if batch >= WARMUP_BATCHES {
                samples_us.push(start.elapsed().as_secs_f64() * 1e6 / OPERATIONS_PER_BATCH as f64);
            }
        }
        samples_us.sort_by(f64::total_cmp);
        let percentile = |rank: usize| {
            let index = (samples_us.len() * rank).div_ceil(100).saturating_sub(1);
            samples_us[index]
        };
        let report = json!({
            "operation": "render_100x30_10000_table_rows",
            "p50_us": percentile(50),
            "p95_us": percentile(95),
            "p99_us": percentile(99),
            "batches": BATCHES,
            "measured_batches": BATCHES - WARMUP_BATCHES,
            "operations_per_batch": OPERATIONS_PER_BATCH,
            "warmup_batches": WARMUP_BATCHES,
            "rows": 10000,
            "columns": fields.len(),
            "terminal_cells": [100, 30],
            "fixture": "examples/top-cpu.program; UI rendering only, applets not advanced",
        });
        let path = std::env::var("UI_TABLE_BENCH_REPORT").expect("set UI_TABLE_BENCH_REPORT");
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
