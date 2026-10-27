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
    let focus = UiState::deserialize(&fresh)?.focus;
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
    let ui: UiState = UiState::deserialize(value).context("invalid live UI state")?;
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
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    fn live_state_json(&self) -> Result<Json> {
        // The publication owns one final JSON tree. Rows already are JSON;
        // copy them once at this boundary rather than through an owned UiState.
        let programs = self
            .instances
            .iter()
            .map(|instance| -> Result<_> {
                let widgets = instance
                    .component
                    .widgets
                    .iter()
                    .zip(&instance.views)
                    .map(|(widget, view)| -> Result<_> {
                        Ok((widget.id.clone(), Json::Object(serde_json::Map::from_iter([
                            ("text".into(), Json::String(view.text.clone())),
                            ("selected".into(), Json::from(view.selected)),
                            ("items".into(), Json::Array(widget.items.iter().cloned().map(Json::String).collect())),
                            ("rows".into(), Json::Array(view.rows.clone())),
                            ("bars".into(), Json::Array(view.bars.iter().map(|(label, value)| json!({"label":label,"value":value})).collect())),
                            ("series".into(), serde_json::to_value(&view.series)?),
                            ("active_series".into(), Json::String(view.active.clone())),
                        ]))))
                    })
                    .collect::<Result<serde_json::Map<String, Json>>>()?;
                Ok((instance.name.clone(), Json::Object(serde_json::Map::from_iter([
                    ("inputs".into(), serde_json::to_value(&instance.inputs)?),
                    ("widgets".into(), Json::Object(widgets)),
                ]))))
            })
            .collect::<Result<serde_json::Map<String, Json>>>()?;
        let focus = self.focus.get(self.focused).map_or(Json::Null, |&(i, j)| {
            json!({"program":self.instances[i].name,"widget":self.instances[i].component.widgets[j].id})
        });
        Ok(Json::Object(serde_json::Map::from_iter([
            ("programs".into(), Json::Object(programs)),
            ("focus".into(), focus),
            ("events".into(), Json::Array(Vec::new())),
        ])))
    }
    #[cfg(test)]
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
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    fn apply_live_state(&mut self, mut ui: UiState) -> Result<()> {
        // Source/state validation gives each placed component and widget one
        // owner. Transfer incoming payloads into the renderer without a copy.
        for i in 0..self.instances.len() {
            let mut desired = ui
                .programs
                .remove(&self.instances[i].name)
                .expect("validated UI component");
            let old = std::mem::replace(&mut self.instances[i].inputs, desired.inputs);
            let changed: Vec<_> = self.instances[i]
                .inputs
                .iter()
                .filter(|(port, value)| old.get(*port) != Some(*value))
                .map(|(port, value)| (port.clone(), value.clone()))
                .collect();
            for (port, value) in changed {
                self.instances[i].generation = self.instances[i].generation.wrapping_add(1);
                self.enqueue(i, &Event::Input(port), value, String::new());
            }
            for j in 0..self.instances[i].views.len() {
                let instance = &mut self.instances[i];
                let widget = &mut instance.component.widgets[j];
                let view = &mut instance.views[j];
                let desired = desired
                    .widgets
                    .remove(&widget.id)
                    .expect("validated UI widget");
                widget.items = desired.items;
                instance.list_items[j] = widget
                    .items
                    .iter()
                    .map(|item| safe(item).into_owned())
                    .collect();
                view.text = desired.text;
                view.selected = desired.selected;
                view.rows = desired.rows;
                view.bars = desired
                    .bars
                    .into_iter()
                    .map(|bar| (bar.label, bar.value))
                    .collect();
                view.series = desired.series;
                view.active = desired.active_series;
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

#[cfg_attr(not(feature = "tui"), allow(dead_code))]
fn publish(
    app: &App,
    state: &mut MutexGuard<'_, crate::live::state::State>,
    shared: &Shared,
    force: bool,
) -> Result<()> {
    let ui = app.live_state_json()?;
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

#[cfg(feature = "tui")]
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
                cache_directory: options.cache_directory.clone(),
                max_output_bytes: options.max_output_bytes,
                max_loop_iterations: options.max_loop_iterations,
                loop_budget_scope: options.loop_budget_scope,
                command_timeout: options.command_timeout,
                total_timeout: options.total_timeout,
                ..RunOptions::new(state.doc.cwd.clone())
            };
            app = Some(App::new(&screen, &shared.registry, &options));
            state.ui_dirty = true;
        }
        let app = app.as_mut().unwrap();
        let apply = state.ui_dirty;
        if apply {
            let desired: UiState = UiState::deserialize(&state.doc.ui)?;
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
                        if app.key(key.into()) {
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

#[cfg(test)]
mod bench {
    use super::*;
    use crate::ui::syntax;
    use sha2::{Digest, Sha256};

    fn fixture(name: &str) -> Result<(Screen, Json)> {
        let screen = syntax::parse(include_str!("../../examples/live/widget-console.program"))?;
        let mut value = initial_state(&screen)?;
        match name {
            "list256" => {
                value["programs"]["console"]["widgets"]["choices"]["items"] =
                    json!((0..256).map(|n| format!("item {n} 雪")).collect::<Vec<_>>())
            }
            "table64k" | "table4m" => {
                let count = if name == "table64k" { 64 } else { 4096 };
                value["programs"]["dashboard"]["widgets"]["metrics"]["rows"] = json!((0..count)
                    .map(
                        |n| json!({"name":format!("row {n}"),"value":n,"padding":"x".repeat(1024)})
                    )
                    .collect::<Vec<_>>());
            }
            "chart" => {
                let series: BTreeMap<_, _> = (0..16)
                    .map(|n| {
                        (
                            format!("series{n}"),
                            (0..120)
                                .map(|x| (f64::from(x), x as u64))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                value["programs"]["dashboard"]["widgets"]["history"]["series"] =
                    serde_json::to_value(series)?;
            }
            _ => {}
        }
        validate_state(&screen, &value)?;
        Ok((screen, value))
    }

    #[test]
    #[ignore = "explicit paired release UI-state validation timings"]
    #[allow(clippy::assertions_on_constants)]
    fn ui_state_validation() -> Result<()> {
        assert!(!cfg!(debug_assertions), "use --release");
        let mut results = BTreeMap::new();
        for name in ["empty_control", "list256", "table64k", "table4m", "chart"] {
            let (screen, value) = fixture(name)?;
            let bytes = serde_json::to_vec(&value)?;
            let fingerprint = format!("{:x}", Sha256::digest(&bytes));
            let before = value.clone();
            for normalize in [false, true] {
                let mut raw = Vec::new();
                let iterations = if name == "table4m" { 5 } else { 20 };
                let expected = if normalize {
                    Some(normalize_state(&screen, &value, None)?)
                } else {
                    None
                };
                if let Some(expected) = &expected {
                    assert_eq!(expected, &value);
                }
                for batch in 0..60 {
                    let start = Instant::now();
                    for _ in 0..iterations {
                        if normalize {
                            std::hint::black_box(normalize_state(
                                std::hint::black_box(&screen),
                                std::hint::black_box(&value),
                                None,
                            )?);
                        } else {
                            validate_state(
                                std::hint::black_box(&screen),
                                std::hint::black_box(&value),
                            )?;
                        }
                    }
                    if batch >= 10 {
                        raw.push(start.elapsed().as_secs_f64() * 1e9 / f64::from(iterations));
                    }
                }
                let mut sorted = raw.clone();
                sorted.sort_by(f64::total_cmp);
                let key = format!(
                    "{name}_{}",
                    if normalize { "normalize" } else { "validate" }
                );
                results.insert(key,json!({"p50_ns":(sorted[24]+sorted[25])/2.0,"p95_ns":sorted[47],"raw_batch_average_ns":raw,"state_sha256":fingerprint,"serialized_state_bytes":bytes.len(),"immutable":true}));
            }
            assert_eq!(before, value);
        }
        std::fs::write(
            std::env::var("OPT_BENCH_REPORT")?,
            serde_json::to_vec_pretty(
                &json!({"results":results,"method":"Private UI-state validate/normalize entry with identical parsed screen and preconstructed JSON fixtures. Fixture,serialization,hash/oracle construction excluded; validation/normalization and result destruction included.10warmup/50measured batches,5or20operations. Caller JSON verified immutable and normalized state exactly equal. No renderer,IPC/native processes/network; not event latency,memory/RSS."}),
            )?,
        )?;
        Ok(())
    }

    #[test]
    fn ui_state_decode_preserves_legacy_schema_errors_and_validation() -> Result<()> {
        let (screen, initial) = fixture("empty_control")?;
        let paths = [
            "/focus",
            "/events",
            "/programs",
            "/programs/console/inputs",
            "/programs/console/widgets/message/text",
            "/programs/console/widgets/choices/selected",
            "/programs/console/widgets/choices/items",
            "/programs/dashboard/widgets/metrics/rows",
            "/programs/dashboard/widgets/load/bars",
            "/programs/dashboard/widgets/history/series",
            "/programs/dashboard/widgets/history/active_series",
        ];
        let values = [
            Json::Null,
            json!(true),
            json!(0),
            json!(-1),
            json!(u64::MAX),
            json!("text 雪"),
            json!([]),
            json!({}),
            json!(["one", "two"]),
            json!({"type":"open","program":"console"}),
            json!([{"type":"press","program":"console","widget":"send"}]),
            json!([{"name":"n","value":1}]),
        ];
        for path in paths {
            for replacement in &values {
                let mut value = initial.clone();
                *value.pointer_mut(path).unwrap() = replacement.clone();
                let before = value.clone();
                let legacy = legacy_validate_state(&screen, &value).map_err(|e| format!("{e:#}"));
                let current = validate_state(&screen, &value).map_err(|e| format!("{e:#}"));
                assert_eq!(current, legacy, "path={path},replacement={replacement}");
                assert_eq!(before, value);
            }
        }
        for name in ["empty_control", "list256", "table64k", "table4m", "chart"] {
            let (screen, value) = fixture(name)?;
            legacy_validate_state(&screen, &value)?;
        }
        for path in ["", "/programs/console", "/programs/console/widgets/message"] {
            let mut value = initial.clone();
            let object = value.pointer_mut(path).unwrap().as_object_mut().unwrap();
            object.insert("unexpected".into(), json!(1));
            assert_eq!(
                validate_state(&screen, &value).map_err(|e| format!("{e:#}")),
                legacy_validate_state(&screen, &value).map_err(|e| format!("{e:#}"))
            );
        }
        Ok(())
    }

    fn legacy_validate_state(screen: &Screen, value: &Json) -> Result<()> {
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
                if !state.active_series.is_empty()
                    && !state.series.contains_key(&state.active_series)
                {
                    bail!("active_series must name an existing series");
                }
            }
        }
        for event in &ui.events {
            validate_event(screen, &ui, event)?;
        }
        Ok(())
    }
    fn new_handoff_app(screen: &Screen) -> App {
        App::new(
            screen,
            &Registry::default(),
            &RunOptions {
                max_output_bytes: 1024 * 1024,
                command_timeout: Some(Duration::from_secs(5)),
                ..RunOptions::new(".".into())
            },
        )
    }
    fn handoff_snapshot(app: &App) -> Result<Json> {
        Ok(
            json!({"ui":serde_json::to_value(app.live_state())?,"frame":app.rendered_frame(),"dirty":app.dirty,"focused":app.focused,"instances":app.instances.iter().map(|instance|json!({"name":instance.name,"generation":instance.generation,"queue":instance.queue,"list_items":instance.list_items})).collect::<Vec<_>>()}),
        )
    }
    #[test]
    fn owned_handoff_matches_legacy_state_queues_generation_and_frames() -> Result<()> {
        for name in ["empty_control", "list256", "table64k", "table4m", "chart"] {
            let (screen, initial) = fixture(name)?;
            for input in [None, Some("old"), Some("literal $HOME; * 雪")] {
                for fill_queue in [false, true] {
                    for generation in [0, u64::MAX] {
                        let mut previous = new_handoff_app(&screen);
                        let mut candidate = new_handoff_app(&screen);
                        for app in [&mut previous, &mut candidate] {
                            app.instances[0].generation = generation;
                            app.instances[0]
                                .inputs
                                .insert("incoming".into(), "old".into());
                            if fill_queue {
                                app.instances[0].queue = (0..32)
                                    .map(|n| (n, format!("item{n}"), String::new()))
                                    .collect();
                            }
                        }
                        for step in 0..3 {
                            let mut value = initial.clone();
                            if let Some(input) = input {
                                value["programs"]["console"]["inputs"]["incoming"] = json!(input);
                            }
                            value["focus"] = json!({"program":"console","widget":if step%2==0 {"send"} else {"message"}});
                            value["events"] = json!([{"type":"press","program":"console","widget":"send"},{"type":"submit","program":"console","widget":"message"},{"type":"choose","program":"console","widget":"choices","index":0},{"type":"open","program":"console"}]);
                            validate_state(&screen, &value)?;
                            let ui = UiState::deserialize(&value)?;
                            legacy_apply(&mut previous, ui.clone())?;
                            candidate.apply_live_state(ui)?;
                            assert_eq!(
                                handoff_snapshot(&candidate)?,
                                handoff_snapshot(&previous)?,
                                "fixture={name},input={input:?},fullqueue={fill_queue},generation={generation},step={step}"
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }
    #[test]
    #[ignore = "explicit release owned UI handoff timing"]
    #[allow(clippy::assertions_on_constants)]
    fn ui_owned_handoff() -> Result<()> {
        assert!(!cfg!(debug_assertions), "use --release");
        let mut results = BTreeMap::new();
        for name in [
            "empty_control",
            "list256",
            "table64k",
            "table4m",
            "chart",
            "port64k",
        ] {
            let (screen, mut value) = fixture(name)?;
            if name == "port64k" {
                value["programs"]["console"]["inputs"]["incoming"] = json!("p".repeat(64 * 1024));
            }
            validate_state(&screen, &value)?;
            let ui = UiState::deserialize(&value)?;
            let bytes = serde_json::to_vec(&value)?;
            let fingerprint = format!("{:x}", Sha256::digest(&bytes));
            let mut app = new_handoff_app(&screen);
            app.apply_live_state(ui.clone())?;
            let expected = handoff_snapshot(&app)?;
            let count = if name == "table4m" { 5 } else { 20 };
            let mut raw = Vec::new();
            for batch in 0..60 {
                let inputs = (0..count).map(|_| ui.clone()).collect::<Vec<_>>();
                let start = Instant::now();
                for input in inputs {
                    app.apply_live_state(std::hint::black_box(input))?;
                }
                if batch >= 10 {
                    raw.push(start.elapsed().as_secs_f64() * 1e9 / f64::from(count));
                }
            }
            assert_eq!(handoff_snapshot(&app)?, expected);
            let mut sorted = raw.clone();
            sorted.sort_by(f64::total_cmp);
            results.insert(name,json!({"p50_ns":(sorted[24]+sorted[25])/2.0,"p95_ns":sorted[47],"raw_batch_average_ns":raw,"state_sha256":fingerprint,"serialized_state_bytes":bytes.len(),"full_state_frame_queue_generation_oracle":true}));
        }
        std::fs::write(
            std::env::var("OPT_BENCH_REPORT")?,
            serde_json::to_vec_pretty(
                &json!({"results":results,"method":"Private App::apply_live_state owned handoff. Parsed screen,typed incoming state,App construction,warm initial apply,per-batch incoming typed clones and full output oracle hashes excluded. Timed apply consumes each state,updates existing App,destroys replaced/temporary payloads and drops input pool.10warmup/50measured batches,5or20operations. No decode/validation/publication/rendering/IPC/native jobs/providers; full final state/frame/queue/generation unchanged. Not whole-widget latency or allocation/RSS."}),
            )?,
        )?;
        Ok(())
    }

    fn legacy_apply(app: &mut App, ui: UiState) -> Result<()> {
        for i in 0..app.instances.len() {
            let desired = &ui.programs[&app.instances[i].name];
            let old = app.instances[i].inputs.clone();
            app.instances[i].inputs = desired.inputs.clone();
            for (port, value) in &desired.inputs {
                if old.get(port) != Some(value) {
                    app.instances[i].generation = app.instances[i].generation.wrapping_add(1);
                    app.enqueue(i, &Event::Input(port.clone()), value.clone(), String::new());
                }
            }
            for j in 0..app.instances[i].views.len() {
                let instance = &mut app.instances[i];
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
            app.focused = app
                .focus
                .iter()
                .position(|&(i, j)| {
                    app.instances[i].name == focus.program
                        && app.instances[i].component.widgets[j].id == focus.widget
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
            let i = app
                .instances
                .iter()
                .position(|instance| &instance.name == program)
                .context("unknown component")?;
            match event {
                LiveEvent::Press { widget, .. } => {
                    app.enqueue(i, &Event::Press(widget), String::new(), String::new())
                }
                LiveEvent::Submit { widget, .. } => {
                    app.enqueue(i, &Event::Submit(widget), String::new(), String::new())
                }
                LiveEvent::Choose { widget, index, .. } => {
                    let j = app.widget(i, &widget)?;
                    app.instances[i].views[j].selected = index;
                    let item = app.instances[i].component.widgets[j].items[index].clone();
                    app.enqueue(i, &Event::Choose(widget), item, String::new());
                }
                LiveEvent::Open { .. } => {
                    app.enqueue(i, &Event::Open, String::new(), String::new())
                }
            }
        }
        app.dirty = true;
        Ok(())
    }
    #[test]
    fn publication_matches_legacy_json_bytes_and_state_validation() -> Result<()> {
        for name in ["empty_control", "list256", "table64k", "table4m", "chart"] {
            let (screen, value) = fixture(name)?;
            let mut app = new_handoff_app(&screen);
            app.apply_live_state(UiState::deserialize(&value)?)?;
            for case in 0..12 {
                if case > 0 {
                    for instance in &mut app.instances {
                        instance
                            .inputs
                            .insert("incoming".into(), format!("port 雪 \n \t \u{1b} {case}"));
                        for (widget, view) in instance
                            .component
                            .widgets
                            .iter_mut()
                            .zip(&mut instance.views)
                        {
                            view.text = format!("text 雪 \n\t\u{1b} {case}");
                            if widget.kind == Kind::List {
                                widget.items = vec!["first 雪".into(), "second\n\u{1b}".into()];
                                view.selected = case % 2;
                            }
                            if widget.kind == Kind::Bars {
                                view.bars = vec![(
                                    "bar 雪".into(),
                                    if case % 3 == 0 { f64::NAN } else { case as f64 },
                                )];
                            }
                            if widget.kind == Kind::Chart {
                                view.series = BTreeMap::from([(
                                    "series 雪".into(),
                                    vec![(
                                        if case % 4 == 0 { f64::INFINITY } else { -0.0 },
                                        u64::MAX,
                                    )],
                                )]);
                                view.active = "series 雪".into();
                            }
                            if widget.kind == Kind::Table {
                                view.rows = vec![
                                    json!({"name":"nested 雪","value":case,"record":{"min":i64::MIN,"max":u64::MAX,"negative_zero":-0.0,"values":[null,true,"escaped\n\t\u{1b}",9007199254740993_u64]}}),
                                ];
                            }
                        }
                    }
                    app.focused = if case % 2 == 0 { usize::MAX } else { 0 };
                }
                let before = handoff_snapshot(&app)?;
                let legacy = serde_json::to_value(app.live_state())?;
                let current = app.live_state_json()?;
                assert_eq!(current, legacy, "fixture={name},case={case}");
                assert_eq!(
                    serde_json::to_vec(&current)?,
                    serde_json::to_vec(&legacy)?,
                    "encoded bytes differ"
                );
                assert_eq!(
                    validate_state(&screen, &current).map_err(|e| format!("{e:#}")),
                    validate_state(&screen, &legacy).map_err(|e| format!("{e:#}"))
                );
                assert_eq!(
                    before,
                    handoff_snapshot(&app)?,
                    "publication mutated renderer"
                );
            }
        }
        Ok(())
    }
    #[test]
    #[ignore = "explicit paired release UI publication timing"]
    #[allow(clippy::assertions_on_constants)]
    fn ui_publication_json() -> Result<()> {
        assert!(!cfg!(debug_assertions), "use --release");
        let mut results = BTreeMap::new();
        for name in [
            "empty_control",
            "list256",
            "table64k",
            "table4m",
            "chart",
            "port64k",
        ] {
            let (screen, mut value) = fixture(name)?;
            if name == "port64k" {
                value["programs"]["console"]["inputs"]["incoming"] = json!("p".repeat(64 * 1024));
            }
            validate_state(&screen, &value)?;
            let mut app = new_handoff_app(&screen);
            app.apply_live_state(UiState::deserialize(&value)?)?;
            let expected = serde_json::to_vec(&serde_json::to_value(app.live_state())?)?;
            let actual = serde_json::to_vec(&app.live_state_json()?)?;
            assert_eq!(actual, expected);
            let before = handoff_snapshot(&app)?;
            let count = if name == "table4m" { 5 } else { 20 };
            let mut raw = Vec::new();
            for batch in 0..60 {
                let start = Instant::now();
                for _ in 0..count {
                    std::hint::black_box(app.live_state_json()?);
                }
                if batch >= 10 {
                    raw.push(start.elapsed().as_secs_f64() * 1e9 / f64::from(count));
                }
            }
            assert_eq!(before, handoff_snapshot(&app)?);
            let mut sorted = raw.clone();
            sorted.sort_by(f64::total_cmp);
            results.insert(name,json!({"p50_ns":(sorted[24]+sorted[25])/2.0,"p95_ns":sorted[47],"raw_batch_average_ns":raw,"state_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(&value)?)),"output_sha256":format!("{:x}",Sha256::digest(expected)),"output_bytes":actual.len(),"renderer_immutable":true}));
        }
        std::fs::write(
            std::env::var("OPT_BENCH_REPORT")?,
            serde_json::to_vec_pretty(
                &json!({"results":results,"method":"Private renderer-to-final-JSON publication builder and result destruction. Parsed screen,input/renderer construction,validation,JSON-byte hashing and full state/frame/queue oracles excluded.10warmup/50measured batches,5or20operations. Exact legacy JSON and encoded-byte oracle,renderer unchanged. Excludes document mutex/equality/budget/rollback,rendering,IPC/native jobs/providers; not whole-widget latency or allocation/RSS."}),
            )?,
        )?;
        Ok(())
    }
}
