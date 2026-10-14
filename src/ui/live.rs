//! The live API projects and edits the renderer's actual view/port state.
use super::*;
use crate::live::state::{Shared, MAX_QUEUE};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::MutexGuard;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WidgetState {
    text: String,
    selected: usize,
    items: Vec<String>,
    rows: Vec<Json>,
    bars: Vec<BarState>,
    series: BTreeMap<String, Vec<(f64, u64)>>,
    active_series: String,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BarState {
    label: String,
    value: f64,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentState {
    inputs: BTreeMap<String, String>,
    widgets: BTreeMap<String, WidgetState>,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Focus {
    program: String,
    widget: String,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum LiveEvent {
    Press {
        program: String,
        widget: String,
    },
    Submit {
        program: String,
        widget: String,
    },
    Choose {
        program: String,
        widget: String,
        index: usize,
    },
    Open {
        program: String,
    },
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UiState {
    programs: BTreeMap<String, ComponentState>,
    focus: Option<Focus>,
    events: Vec<LiveEvent>,
}
fn defaults(screen: &Screen) -> UiState {
    let programs = screen
        .placements
        .iter()
        .map(|name| {
            let widgets = screen.components[name]
                .widgets
                .iter()
                .map(|widget| {
                    (
                        widget.id.clone(),
                        WidgetState {
                            text: widget.initial.clone(),
                            selected: 0,
                            items: widget.items.clone(),
                            rows: Vec::new(),
                            bars: Vec::new(),
                            series: BTreeMap::new(),
                            active_series: String::new(),
                        },
                    )
                })
                .collect();
            (
                name.clone(),
                ComponentState {
                    inputs: BTreeMap::new(),
                    widgets,
                },
            )
        })
        .collect();
    let focus = screen.placements.iter().find_map(|program| {
        screen.components[program]
            .widgets
            .iter()
            .find(|w| matches!(w.kind, Kind::Input | Kind::List | Kind::Button))
            .map(|widget| Focus {
                program: program.clone(),
                widget: widget.id.clone(),
            })
    });
    UiState {
        programs,
        focus,
        events: Vec::new(),
    }
}
pub(crate) fn initial_state(screen: &Screen) -> Result<Json> {
    Ok(serde_json::to_value(defaults(screen))?)
}
pub(crate) fn normalize_state(
    screen: &Screen,
    old: &Json,
    previous: Option<&Screen>,
) -> Result<Json> {
    let mut fresh = initial_state(screen)?;
    let original = previous
        .map(initial_state)
        .transpose()?
        .unwrap_or(Json::Null);
    for (name, component) in fresh["programs"].as_object_mut().unwrap() {
        if let Some(previous) = old.get("programs").and_then(|p| p.get(name)) {
            for port in &screen.components[name].inputs {
                if let Some(value) = previous.get("inputs").and_then(|p| p.get(port)) {
                    component["inputs"][port] = value.clone();
                }
            }
            for declaration in &screen.components[name].widgets {
                let widget = &mut component["widgets"][&declaration.id];
                if let Some(fields) = previous
                    .get("widgets")
                    .and_then(|w| w.get(&declaration.id))
                    .and_then(Json::as_object)
                {
                    let before = original
                        .get("programs")
                        .and_then(|p| p.get(name))
                        .and_then(|p| p.get("widgets"))
                        .and_then(|w| w.get(&declaration.id));
                    for (field, value) in fields {
                        if before.and_then(|w| w.get(field)) != Some(value) {
                            widget[field] = value.clone();
                        }
                    }
                }
                if declaration.kind != Kind::List {
                    widget["items"] = json!([]);
                    widget["selected"] = json!(0);
                } else if widget["selected"].as_u64().unwrap_or(0)
                    >= widget["items"].as_array().map_or(1, |a| a.len().max(1)) as u64
                {
                    widget["selected"] = json!(0);
                }
                if declaration.kind != Kind::Table
                    || widget["rows"].as_array().is_some_and(|rows| {
                        rows.iter().any(|row| {
                            declaration
                                .columns
                                .iter()
                                .any(|(_, field)| row.get(field).is_none())
                        })
                    })
                {
                    widget["rows"] = json!([]);
                }
                if declaration.kind != Kind::Bars {
                    widget["bars"] = json!([]);
                }
                if declaration.kind != Kind::Chart {
                    widget["series"] = json!({});
                    widget["active_series"] = json!("");
                }
                if declaration.kind == Kind::Input
                    && widget["text"]
                        .as_str()
                        .is_none_or(|text| validate_text_field(text).is_err())
                {
                    widget["text"] = json!(declaration.initial);
                }
            }
        }
    }
    if let Some(focus) = old.get("focus") {
        fresh["focus"] = focus.clone();
    }
    let focus = serde_json::from_value::<UiState>(fresh.clone())?.focus;
    if focus
        .as_ref()
        .is_some_and(|focus| !focus_valid(screen, focus))
        || (focus.is_none() && defaults(screen).focus.is_some())
    {
        fresh["focus"] = serde_json::to_value(defaults(screen).focus)?;
    }
    validate_state(screen, &fresh)?;
    Ok(fresh)
}
fn focus_valid(screen: &Screen, focus: &Focus) -> bool {
    screen.placements.contains(&focus.program)
        && screen.components[&focus.program].widgets.iter().any(|w| {
            w.id == focus.widget && matches!(w.kind, Kind::Input | Kind::List | Kind::Button)
        })
}
fn event_widget<'a>(screen: &'a Screen, program: &str, id: &str, kind: Kind) -> Result<&'a Widget> {
    if !screen.placements.iter().any(|p| p == program) {
        bail!("event program is not placed on the screen");
    }
    screen.components[program]
        .widgets
        .iter()
        .find(|w| w.id == id && w.kind == kind)
        .context("event widget/type does not match the program")
}
fn validate_event(screen: &Screen, ui: &UiState, event: &LiveEvent) -> Result<()> {
    match event {
        LiveEvent::Press { program, widget } => {
            event_widget(screen, program, widget, Kind::Button)?;
        }
        LiveEvent::Submit { program, widget } => {
            event_widget(screen, program, widget, Kind::Input)?;
        }
        LiveEvent::Choose {
            program,
            widget,
            index,
        } => {
            event_widget(screen, program, widget, Kind::List)?;
            if *index >= ui.programs[program].widgets[widget].items.len() {
                bail!("chosen list index is out of range");
            }
        }
        LiveEvent::Open { program } => {
            if !screen.placements.contains(program) {
                bail!("event program is not on the screen");
            }
        }
    }
    Ok(())
}
pub(crate) fn validate_state(screen: &Screen, value: &Json) -> Result<()> {
    let ui: UiState = serde_json::from_value(value.clone()).context("invalid live UI state")?;
    if ui.programs.keys().cloned().collect::<BTreeSet<_>>()
        != screen.placements.iter().cloned().collect()
    {
        bail!("UI component identities come from source; edit source to add/remove components");
    }
    if ui.events.len() > MAX_QUEUE {
        bail!("UI event queue exceeds capacity");
    }
    if ui.focus.is_none() && defaults(screen).focus.is_some() {
        bail!("an interactive screen requires a focused widget");
    }
    if let Some(focus) = &ui.focus {
        if !focus_valid(screen, focus) {
            bail!("focus must name an interactive widget on the screen");
        }
    }
    for (name, component) in &ui.programs {
        let declaration = &screen.components[name];
        if component.widgets.keys().cloned().collect::<BTreeSet<_>>()
            != declaration.widgets.iter().map(|w| w.id.clone()).collect()
        {
            bail!("widget identities come from source; edit source to add/remove widgets");
        }
        for (port, text) in &component.inputs {
            if !declaration.inputs.contains(port) {
                bail!("unknown component input {name}.{port}");
            }
            validate_message(text)?;
        }
        for widget in &declaration.widgets {
            let state = &component.widgets[&widget.id];
            if widget.kind == Kind::Input {
                validate_text_field(&state.text)?;
            }
            if state.items.len() > 256
                || state.rows.len() > 10000
                || state.bars.len() > 256
                || state.series.len() > 16
            {
                bail!("widget collection limit exceeded");
            }
            if widget.kind == Kind::List && state.selected >= state.items.len().max(1) {
                bail!("selected list index is out of range");
            }
            if widget.kind != Kind::List && state.selected != 0 {
                bail!("selected is supported only for list widgets");
            }
            if widget.kind != Kind::List && !state.items.is_empty() {
                bail!("items are supported only by list widgets");
            }
            if widget.kind != Kind::Table && !state.rows.is_empty() {
                bail!("rows are supported only by table widgets");
            }
            if widget.kind != Kind::Bars && !state.bars.is_empty() {
                bail!("bars are supported only by bar widgets");
            }
            if widget.kind != Kind::Chart
                && (!state.series.is_empty() || !state.active_series.is_empty())
            {
                bail!("series are supported only by chart widgets");
            }
            for item in &state.items {
                validate_message(item)?;
            }
            for row in &state.rows {
                if !row.is_object()
                    || widget
                        .columns
                        .iter()
                        .any(|(_, field)| row.get(field).is_none())
                {
                    bail!("table rows must be objects containing the declared fields");
                }
            }
            if state
                .bars
                .iter()
                .any(|bar| !bar.value.is_finite() || bar.value < 0.0)
            {
                bail!("bar values must be finite and nonnegative");
            }
            if state
                .series
                .values()
                .any(|points| points.len() > 120 || points.iter().any(|(x, _)| !x.is_finite()))
            {
                bail!("chart series need finite x coordinates and at most 120 points");
            }
            if !state.active_series.is_empty() && !state.series.contains_key(&state.active_series) {
                bail!("active_series must name an existing series");
            }
        }
    }
    for event in &ui.events {
        validate_event(screen, &ui, event)?;
    }
    Ok(())
}
impl App {
    fn live_state(&self) -> UiState {
        let programs = self
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
                            WidgetState {
                                text: view.text.clone(),
                                selected: view.selected,
                                items: widget.items.clone(),
                                rows: view.rows.clone(),
                                bars: view
                                    .bars
                                    .iter()
                                    .map(|(label, value)| BarState {
                                        label: label.clone(),
                                        value: *value,
                                    })
                                    .collect(),
                                series: view.series.clone(),
                                active_series: view.active.clone(),
                            },
                        )
                    })
                    .collect();
                (
                    instance.name.clone(),
                    ComponentState {
                        inputs: instance.inputs.clone(),
                        widgets,
                    },
                )
            })
            .collect();
        let focus = self.focus.get(self.focused).map(|&(i, j)| Focus {
            program: self.instances[i].name.clone(),
            widget: self.instances[i].component.widgets[j].id.clone(),
        });
        UiState {
            programs,
            focus,
            events: Vec::new(),
        }
    }
    fn apply_live_state(&mut self, ui: UiState) -> Result<()> {
        for i in 0..self.instances.len() {
            let desired = &ui.programs[&self.instances[i].name];
            let old = self.instances[i].inputs.clone();
            self.instances[i].inputs = desired.inputs.clone();
            for (port, value) in &desired.inputs {
                if old.get(port) != Some(value) {
                    self.instances[i].generation = self.instances[i].generation.wrapping_add(1);
                    self.enqueue(i, &Event::Input(port.clone()), value.clone(), String::new());
                }
            }
            for j in 0..self.instances[i].views.len() {
                let instance = &mut self.instances[i];
                let widget = &mut instance.component.widgets[j];
                let view = &mut instance.views[j];
                let desired = &desired.widgets[&widget.id];
                widget.items = desired.items.clone();
                instance.list_items[j] = desired
                    .items
                    .iter()
                    .map(|item| safe(item).into_owned())
                    .collect();
                view.text = desired.text.clone();
                view.selected = desired.selected;
                view.rows = desired.rows.clone();
                view.bars = desired
                    .bars
                    .iter()
                    .map(|b| (b.label.clone(), b.value))
                    .collect();
                view.series = desired.series.clone();
                view.active = desired.active_series.clone();
            }
        }
        if let Some(focus) = ui.focus {
            self.focused = self
                .focus
                .iter()
                .position(|&(i, j)| {
                    self.instances[i].name == focus.program
                        && self.instances[i].component.widgets[j].id == focus.widget
                })
                .context("invalid focus")?;
        }
        for event in ui.events {
            let program = match &event {
                LiveEvent::Press { program, .. }
                | LiveEvent::Submit { program, .. }
                | LiveEvent::Choose { program, .. }
                | LiveEvent::Open { program } => program,
            };
            let i = self
                .instances
                .iter()
                .position(|instance| &instance.name == program)
                .context("unknown component")?;
            match event {
                LiveEvent::Press { widget, .. } => {
                    self.enqueue(i, &Event::Press(widget), String::new(), String::new())
                }
                LiveEvent::Submit { widget, .. } => {
                    self.enqueue(i, &Event::Submit(widget), String::new(), String::new())
                }
                LiveEvent::Choose { widget, index, .. } => {
                    let j = self.widget(i, &widget)?;
                    self.instances[i].views[j].selected = index;
                    let item = self.instances[i].component.widgets[j].items[index].clone();
                    self.enqueue(i, &Event::Choose(widget), item, String::new());
                }
                LiveEvent::Open { .. } => {
                    self.enqueue(i, &Event::Open, String::new(), String::new())
                }
            }
        }
        self.dirty = true;
        Ok(())
    }
}

fn publish(
    app: &App,
    state: &mut MutexGuard<'_, crate::live::state::State>,
    shared: &Shared,
    force: bool,
) -> Result<()> {
    let ui = serde_json::to_value(app.live_state())?;
    let changed = ui != state.doc.ui;
    if !state.doc.replace_ui(ui)? {
        bail!("UI state exceeds 8 MiB; effects may have completed");
    }
    state.in_flight = app.instances.iter().any(|instance| instance.job.is_some());
    state.nodes = app.jobs_done;
    state.runs = app.jobs_done;
    state.launches = app.completed_native_launches;
    state.cache_hits = app.cache_hits;
    state.ui_errors = json!(app
        .instances
        .iter()
        .filter_map(|instance| instance.error.as_ref().map(|error| (
            instance.name.clone(),
            error.chars().take(4096).collect::<String>()
        )))
        .collect::<BTreeMap<_, _>>());
    if app.dirty || force {
        state.ui_frame = app.rendered_frame();
    }
    if !state.uncertain {
        state.status = if state.in_flight {
            "running"
        } else if state.running {
            "waiting"
        } else {
            "paused"
        }
        .into();
    }
    if changed || app.dirty || force {
        state.changed("ui_applied");
        state.ui_applied_revision = state.revision;
        shared.wake.notify_all();
    }
    Ok(())
}

pub(crate) fn run(shared: Arc<Shared>, options: &RunOptions) -> Result<()> {
    let mut source_epoch = u64::MAX;
    let mut app: Option<App> = None;
    let mut terminal = if shared.headless {
        None
    } else {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("live TUI needs a terminal or serve --headless");
        }
        Some(Terminal::new(CrosstermBackend::new(io::stdout()))?)
    };
    let _terminal_guard = if terminal.is_some() {
        Some(TerminalGuard::enter()?)
    } else {
        None
    };
    let mut input = if terminal.is_some() {
        let (sender, receiver) = mpsc::sync_channel(64);
        let (wake_sender, wake_receiver) = mpsc::sync_channel(1);
        Some((
            InputReader::start(sender, wake_sender)?,
            receiver,
            wake_receiver,
        ))
    } else {
        None
    };
    loop {
        let mut state = shared.state.lock().unwrap();
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(screen) = state.plan.screen.clone() else {
            return Ok(());
        };
        let rebuilding = source_epoch != state.ui_epoch;
        if rebuilding {
            source_epoch = state.ui_epoch;
            let options = RunOptions {
                inputs: Default::default(),
                working_directory: state.doc.cwd.clone(),
                cache_directory: options.cache_directory.clone(),
                trace: false,
                max_output_bytes: options.max_output_bytes,
                command_timeout: options.command_timeout,
            };
            app = Some(App::new(&screen, &shared.registry, &options));
            state.ui_dirty = true;
        }
        let app = app.as_mut().unwrap();
        let apply = state.ui_dirty;
        if apply {
            let desired: UiState = serde_json::from_value(state.doc.ui.clone())?;
            app.apply_live_state(desired)?;
            state.ui_dirty = false;
        }
        if state.active_cancel.swap(false, Ordering::Relaxed) {
            let mut any = false;
            for instance in &mut app.instances {
                if let Some(job) = &instance.job {
                    any = true;
                    job.cancel.store(true, Ordering::Relaxed);
                    instance.generation = instance.generation.wrapping_add(1);
                    let execution = &job.execution;
                    if !instance
                        .queue
                        .iter()
                        .any(|queued| queued.0 == execution.handler)
                    {
                        instance.queue.push_front((
                            execution.handler,
                            execution.item.clone(),
                            execution.error.clone(),
                        ));
                    }
                }
            }
            if any {
                state.uncertain = true;
                state.status = "failed".into();
                state.error = Some("UI worker cancelled; retry may repeat external effects".into());
                state.running = false;
                state.changed("ui_cancelled");
            }
        }
        if let Some((reader, receiver, wake)) = &mut input {
            let _ = wake.try_recv();
            for _ in 0..64 {
                let Ok(message) = receiver.try_recv() else {
                    break;
                };
                reader.unpark();
                match message? {
                    TerminalEvent::Key(key) => {
                        if app.key(key) {
                            shared.stop.store(true, Ordering::Relaxed);
                        }
                    }
                    TerminalEvent::Resize(_, _) => app.dirty = true,
                    TerminalEvent::Paste(text) => {
                        if let Some(&(i, j)) = app.focus.get(app.focused) {
                            if app.instances[i].component.widgets[j].kind == Kind::Input {
                                for c in safe(&text).chars().filter(|c| *c != '\n' && *c != '\t') {
                                    if app.instances[i].views[j].text.len() + c.len_utf8() > 512 {
                                        break;
                                    }
                                    app.instances[i].views[j].text.push(c);
                                }
                                app.dirty = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let before_jobs = app.jobs_done;
        let budget = if !state.running {
            0
        } else {
            state.step_budget.unwrap_or(usize::MAX)
        };
        let started = app.advance_controlled(state.running && state.step_budget.is_none(), budget);
        if let Some(remaining) = &mut state.step_budget {
            *remaining = remaining.saturating_sub(started);
            if *remaining == 0 {
                state.running = false;
            }
        }
        if app.closing {
            shared.stop.store(true, Ordering::Relaxed);
        }
        let active = app.instances.iter().any(|instance| instance.job.is_some());
        let refresh = apply
            || app.dirty
            || before_jobs != app.jobs_done
            || active != state.in_flight
            || started > 0;
        if refresh {
            publish(app, &mut state, &shared, apply)?;
        } else if !state.uncertain {
            state.status = if active {
                "running"
            } else if state.running {
                "waiting"
            } else {
                "paused"
            }
            .into();
        }
        let draw = app.dirty;
        app.dirty = false;
        let revision = state.revision;
        drop(state);
        if draw {
            if let Some(terminal) = &mut terminal {
                terminal.draw(|frame| app.render(frame))?;
            }
        }
        let state = shared.state.lock().unwrap();
        if state.revision == revision && !state.ui_dirty && !shared.stop.load(Ordering::Relaxed) {
            drop(
                shared
                    .wake
                    .wait_timeout(state, Duration::from_millis(20))
                    .unwrap(),
            );
        }
    }
}
