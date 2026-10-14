//! A deliberately small, validated single-file component language.
use crate::{
    ast::{Program, Step},
    manifest::Registry,
    parser::{self, Token},
    runtime,
};
use anyhow::{bail, Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Heading,
    Input,
    List,
    Table,
    Text,
    Badge,
    Button,
    Chart,
    Bars,
}
#[derive(Clone, Debug)]
pub struct Widget {
    pub id: String,
    pub kind: Kind,
    pub initial: String,
    pub items: Vec<String>,
    pub columns: Vec<(String, String)>,
    pub rows: Option<u16>,
    pub key: Option<String>,
    pub horizontal_label: String,
    pub vertical_label: String,
    pub bar_label: Option<String>,
    pub bar_value: Option<String>,
    pub bar_suffix: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Shortcut {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub label: String,
}
impl Shortcut {
    fn parse(value: &str) -> Result<Self> {
        let (modifiers, prefix, key) = if let Some(key) = value.strip_prefix("Ctrl-") {
            (KeyModifiers::CONTROL, "Ctrl-", key)
        } else if let Some(key) = value.strip_prefix("Alt-") {
            (KeyModifiers::ALT, "Alt-", key)
        } else {
            (KeyModifiers::NONE, "", value)
        };
        let (code, label) = if key.len() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric()) {
            let c = key.chars().next().unwrap().to_ascii_lowercase();
            (
                KeyCode::Char(c),
                format!(
                    "{prefix}{}",
                    if prefix.is_empty() {
                        c
                    } else {
                        c.to_ascii_uppercase()
                    }
                ),
            )
        } else if modifiers == KeyModifiers::NONE && key == "Escape" {
            (KeyCode::Esc, "Escape".into())
        } else if modifiers == KeyModifiers::NONE && key.starts_with('F') {
            let number: u8 = key[1..].parse().context("expected F1 through F12")?;
            if !(1..=12).contains(&number) {
                bail!("expected F1 through F12");
            }
            (KeyCode::F(number), format!("F{number}"))
        } else {
            bail!("shortcut must be an ASCII letter/digit, Ctrl-/Alt- letter/digit, Escape, or F1 through F12");
        };
        if ["Ctrl-C", "Ctrl-U"].contains(&label.as_str()) {
            bail!("{label} is reserved for terminal interruption or text editing");
        }
        Ok(Self {
            code,
            modifiers,
            label,
        })
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Open,
    Input(String),
    Submit(String),
    Press(String),
    Key(Shortcut),
    Choose(String),
    Every(u64),
    Error,
}
#[derive(Clone, Debug)]
pub enum Value {
    Literal(String),
    Widget(String),
    Item,
    Result,
    Error,
}
#[derive(Clone, Debug)]
pub enum Action {
    Send(Value, String),
    Show(Value, String),
    Rows(String),
    Bars(String),
    Field(String, String),
    Point(String, String),
    Press(String),
    CloseScreen,
    Native(Step),
}
#[derive(Clone, Debug)]
pub struct Handler {
    pub event: Event,
    pub actions: Vec<Action>,
}
#[derive(Clone, Debug)]
pub struct Component {
    pub inputs: BTreeSet<String>,
    pub outputs: BTreeSet<String>,
    pub widgets: Vec<Widget>,
    pub handlers: Vec<Handler>,
}
#[derive(Clone, Debug)]
pub struct Link {
    pub source: String,
    pub output: String,
    pub target: String,
    pub input: String,
}
#[derive(Clone, Debug)]
pub struct Screen {
    pub title: String,
    pub placements: Vec<String>,
    pub first_width: u16,
    pub components: BTreeMap<String, Component>,
    pub links: Vec<Link>,
}
struct Line<'source> {
    number: usize,
    indent: usize,
    tokens: Vec<Token<'source>>,
}
fn words<'source>(tokens: &[Token<'source>]) -> Vec<&'source str> {
    tokens
        .iter()
        .map(|t| match t {
            Token::Word(s) => *s,
            _ => "",
        })
        .collect()
}
fn text(tokens: &[Token<'_>], n: usize) -> Result<String> {
    match tokens.get(n) {
        Some(Token::String(s)) => Ok(s.clone().into_owned()),
        _ => bail!("expected quoted text"),
    }
}
fn name(tokens: &[Token<'_>], n: usize) -> Result<String> {
    match tokens.get(n) {
        Some(Token::Word(s))
            if !s.is_empty()
                && !["true", "false", "error"].contains(s)
                && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && s.chars().next().unwrap().is_ascii_alphabetic() =>
        {
            Ok((*s).to_owned())
        }
        _ => bail!("expected a name"),
    }
}
pub fn is_screen(source: &str) -> bool {
    source
        .lines()
        .map(parser::strip_comment)
        .map(str::trim)
        .find(|s| !s.is_empty())
        .is_some_and(|s| s.starts_with("open a screen "))
}

pub fn parse(source: &str) -> Result<Screen> {
    let mut lines = Vec::new();
    for (index, raw) in source.lines().enumerate() {
        let raw = parser::strip_comment(raw);
        if raw.trim().is_empty() {
            continue;
        }
        let prefix = raw
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect::<String>();
        if prefix.contains('\t') {
            bail!("line {}: tabs are not allowed", index + 1);
        }
        let tokens = parser::tokenize(raw.trim()).with_context(|| format!("line {}", index + 1))?;
        lines.push(Line {
            number: index + 1,
            indent: prefix.len(),
            tokens,
        });
    }
    let mut screen = Screen {
        title: String::new(),
        placements: Vec::new(),
        first_width: 50,
        components: BTreeMap::new(),
        links: Vec::new(),
    };
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        let w = words(&line.tokens);
        let result = (|| -> Result<()> {
            if line.indent != 0 {
                bail!("unexpected top-level indentation");
            }
            if w.len() == 5 && w[..4] == ["open", "a", "screen", "titled"] {
                if !screen.title.is_empty() {
                    bail!("only one screen is allowed");
                }
                screen.title = text(&line.tokens, 4)?;
            } else if w == ["divide", "the", "screen", "into", "two", "columns"] {
            } else if w.len() == 5
                && w[0] == "place"
                && w[2..4] == ["on", "the"]
                && ["left", "right"].contains(&w[4])
            {
                let id = name(&line.tokens, 1)?;
                if screen.placements.contains(&id) {
                    bail!("component is placed twice");
                }
                if (w[4] == "left" && !screen.placements.is_empty())
                    || (w[4] == "right" && screen.placements.len() != 1)
                {
                    bail!("place the left component before the right component");
                }
                screen.placements.push(id);
            } else if w.len() == 7 && w[0] == "give" && w[3..] == ["percent", "of", "the", "width"]
            {
                if screen.placements.first() != Some(&name(&line.tokens, 1)?) {
                    bail!("width must name the left component");
                }
                screen.first_width = w[2].parse()?;
                if !(10..=90).contains(&screen.first_width) {
                    bail!("column width must be between 10 and 90 percent");
                }
            } else if w.len() == 8
                && w[0] == "connect"
                && w[2] == "from"
                && w[4] == "to"
                && w[6] == "in"
            {
                screen.links.push(Link {
                    output: name(&line.tokens, 1)?,
                    source: name(&line.tokens, 3)?,
                    input: name(&line.tokens, 5)?,
                    target: name(&line.tokens, 7)?,
                });
            } else if w.len() == 5 && w[..4] == ["define", "a", "program", "named"] {
                let id = name(&line.tokens, 4)?;
                i += 1;
                let component = parse_component(&lines, &mut i)?;
                if screen.components.insert(id, component).is_some() {
                    bail!("duplicate component definition");
                }
                return Ok(());
            } else {
                bail!("unknown screen declaration");
            }
            i += 1;
            Ok(())
        })();
        result.with_context(|| format!("line {}", line.number))?;
    }
    if screen.title.is_empty() || screen.placements.is_empty() || screen.placements.len() > 2 {
        bail!("a screen needs a title and one or two placed components");
    }
    if screen.components.len() > 16 || screen.links.len() > 32 {
        bail!("too many components or connections");
    }
    Ok(screen)
}
fn parse_component(lines: &[Line<'_>], i: &mut usize) -> Result<Component> {
    let mut component = Component {
        inputs: BTreeSet::new(),
        outputs: BTreeSet::new(),
        widgets: Vec::new(),
        handlers: Vec::new(),
    };
    while let Some(line) = lines.get(*i).filter(|l| l.indent > 0) {
        if line.indent != 2 {
            bail!(
                "line {}: component declarations need two spaces",
                line.number
            );
        }
        let w = words(&line.tokens);
        let result = (|| -> Result<()> {
            if w.len() == 4 && ["accept", "offer"].contains(&w[0]) && w[2..] == ["as", "text"] {
                let set = if w[0] == "accept" {
                    &mut component.inputs
                } else {
                    &mut component.outputs
                };
                if !set.insert(name(&line.tokens, 1)?) {
                    bail!("duplicate interface port");
                }
                *i += 1;
            } else if w.first() == Some(&"show") {
                let named = w
                    .iter()
                    .position(|s| *s == "named")
                    .context("widgets must have a name")?;
                let kind = match &w[1..named] {
                    ["a", "heading"] => Kind::Heading,
                    ["a", "text", "field"] => Kind::Input,
                    ["a", "list"] => Kind::List,
                    ["a", "table"] => Kind::Table,
                    ["a", "text", "panel"] => Kind::Text,
                    ["a", "badge"] => Kind::Badge,
                    ["a", "button"] => Kind::Button,
                    ["a", "line", "chart"] => Kind::Chart,
                    ["a", "bar", "chart"] => Kind::Bars,
                    _ => bail!("unsupported widget kind"),
                };
                let mut widget = Widget {
                    id: name(&line.tokens, named + 1)?,
                    kind,
                    initial: String::new(),
                    items: Vec::new(),
                    columns: Vec::new(),
                    rows: None,
                    key: None,
                    horizontal_label: "elapsed seconds".into(),
                    vertical_label: "value".into(),
                    bar_label: None,
                    bar_value: None,
                    bar_suffix: String::new(),
                };
                let rest = &w[named + 2..];
                match rest {
                    [] => {}
                    ["with", "text", ""] | ["with", "value", ""] => {
                        widget.initial = text(&line.tokens, named + 4)?
                    }
                    ["labelled", ""] => widget.initial = text(&line.tokens, named + 3)?,
                    ["with", "key", ""] => widget.key = Some(text(&line.tokens, named + 4)?),
                    _ => bail!("unknown widget property"),
                }
                if component.widgets.iter().any(|v| v.id == widget.id) {
                    bail!("duplicate widget name");
                }
                *i += 1;
                while let Some(child) = lines.get(*i).filter(|l| l.indent > 2) {
                    let cw = words(&child.tokens);
                    if child.indent != 4 {
                        bail!("line {}: widget items need four spaces", child.number);
                    }
                    if widget.kind == Kind::List && cw == ["include", ""] {
                        widget.items.push(text(&child.tokens, 1)?);
                    } else if widget.kind == Kind::Table && cw == ["column", "", "from", ""] {
                        widget
                            .columns
                            .push((text(&child.tokens, 1)?, text(&child.tokens, 3)?));
                    } else if widget.kind == Kind::Bars
                        && cw == ["label", "each", "bar", "from", ""]
                    {
                        if widget.bar_label.replace(text(&child.tokens, 4)?).is_some() {
                            bail!("duplicate bar label field");
                        }
                    } else if widget.kind == Kind::Bars && cw == ["size", "each", "bar", "from", ""]
                    {
                        if widget.bar_value.replace(text(&child.tokens, 4)?).is_some() {
                            bail!("duplicate bar value field");
                        }
                    } else if widget.kind == Kind::Bars
                        && cw == ["label", "bar", "values", "with", ""]
                    {
                        widget.bar_suffix = text(&child.tokens, 4)?;
                    } else {
                        bail!("line {}: invalid widget item", child.number);
                    }
                    *i += 1;
                }
                if widget.items.len() > 256 || widget.columns.len() > 16 {
                    bail!("a widget supports at most 256 list items and 16 table columns");
                }
                if widget.kind == Kind::Input && widget.initial.len() > 512 {
                    bail!("text field initial value exceeds 512 bytes");
                }
                if widget.kind == Kind::Table && widget.columns.is_empty() {
                    bail!("a table needs columns");
                }
                if widget.kind == Kind::Bars
                    && (widget.bar_label.is_none() || widget.bar_value.is_none())
                {
                    bail!("a bar chart needs label and size fields");
                }
                component.widgets.push(widget);
            } else if w.len() == 4 && w[0] == "give" && w[3] == "rows" {
                let id = name(&line.tokens, 1)?;
                let rows: u16 = w[2].parse()?;
                if !(1..=40).contains(&rows) {
                    bail!("widget height must be between 1 and 40 rows");
                }
                component
                    .widgets
                    .iter_mut()
                    .find(|v| v.id == id)
                    .context("unknown widget height target")?
                    .rows = Some(rows);
                *i += 1;
            } else if w.len() == 7 && w[..2] == ["label", "the"] && w[3..5] == ["axis", "of"] {
                let id = name(&line.tokens, 5)?;
                let widget = component
                    .widgets
                    .iter_mut()
                    .find(|v| v.id == id)
                    .context("unknown chart label target")?;
                if widget.kind != Kind::Chart {
                    bail!("axis labels require a line chart");
                }
                match w[2] {
                    "horizontal" => widget.horizontal_label = text(&line.tokens, 6)?,
                    "vertical" => widget.vertical_label = text(&line.tokens, 6)?,
                    _ => bail!("expected horizontal or vertical axis"),
                }
                *i += 1;
            } else if w.first() == Some(&"whenever") || w.first() == Some(&"every") {
                let event = match w.as_slice() {
                    ["whenever", "the", "program", "opens"] => Event::Open,
                    ["whenever", "key", "", "is", "pressed"] => {
                        Event::Key(Shortcut::parse(&text(&line.tokens, 2)?)?)
                    }
                    ["whenever", port, "arrives"] => Event::Input((*port).into()),
                    ["whenever", widget, "is", "submitted"] => Event::Submit((*widget).into()),
                    ["whenever", widget, "is", "pressed"] => Event::Press((*widget).into()),
                    ["whenever", "an", "item", "is", "chosen", "in", widget] => {
                        Event::Choose((*widget).into())
                    }
                    ["whenever", "a", "command", "fails"] => Event::Error,
                    ["every", _, _] => {
                        let trigger = parser::parse(&format!("{}\n  run pwd", w.join(" ")))?;
                        match trigger.trigger {
                            Some(crate::ast::Trigger::Every { interval_ms, .. }) => {
                                Event::Every(interval_ms)
                            }
                            _ => bail!("expected interval"),
                        }
                    }
                    _ => bail!("unsupported component event"),
                };
                *i += 1;
                let mut actions = Vec::new();
                while let Some(child) = lines.get(*i).filter(|l| l.indent > 2) {
                    if child.indent != 4 {
                        bail!("line {}: actions need four spaces", child.number);
                    }
                    if matches!(
                        words(&child.tokens).first(),
                        Some(&"if" | &"repeat" | &"for" | &"while")
                    ) {
                        let view = |index| {
                            lines.get(index).map(|line: &Line<'_>| parser::BlockLine {
                                number: line.number,
                                indent: line.indent,
                                tokens: &line.tokens,
                            })
                        };
                        let mut steps = Vec::new();
                        parser::parse_statement(&view, i, 4, &mut steps, 0)?;
                        actions.extend(steps.into_iter().map(Action::Native));
                    } else {
                        actions.push(
                            parse_action(child)
                                .with_context(|| format!("line {}", child.number))?,
                        );
                        *i += 1;
                    }
                }
                if actions.is_empty() {
                    bail!("event handler needs actions");
                }
                component.handlers.push(Handler { event, actions });
            } else {
                bail!("unknown component declaration");
            }
            Ok(())
        })();
        result.with_context(|| format!("line {}", line.number))?;
    }
    if component.widgets.is_empty() || component.widgets.len() > 32 || component.handlers.len() > 32
    {
        bail!("a component needs between 1 and 32 widgets, and at most 32 handlers");
    }
    Ok(component)
}
fn parse_action(line: &Line<'_>) -> Result<Action> {
    let mut tokens = line.tokens.as_slice();
    if words(tokens).first() == Some(&"then") {
        tokens = &tokens[1..];
    }
    let w = words(tokens);
    if w == ["close", "the", "screen"] {
        return Ok(Action::CloseScreen);
    }
    if w.len() == 2 && w[0] == "press" {
        return Ok(Action::Press(name(tokens, 1)?));
    }
    if w.len() == 6 && w[..3] == ["show", "JSON", "as"] && w[4] == "in" {
        return match w[3] {
            "rows" => Ok(Action::Rows(name(tokens, 5)?)),
            "bars" => Ok(Action::Bars(name(tokens, 5)?)),
            _ => bail!("expected rows or bars"),
        };
    }
    if w.len() == 7 && w[..4] == ["show", "the", "result", "as"] && w[5] == "in" {
        return match w[4] {
            "rows" => Ok(Action::Rows(name(tokens, 6)?)),
            "bars" => Ok(Action::Bars(name(tokens, 6)?)),
            _ => bail!("expected rows or bars"),
        };
    }
    if w.len() == 5 && w[..2] == ["show", "field"] && w[3] == "in" {
        return Ok(Action::Field(text(tokens, 2)?, name(tokens, 4)?));
    }
    if w.len() == 5 && w[..2] == ["add", "field"] && w[3] == "to" {
        return Ok(Action::Point(text(tokens, 2)?, name(tokens, 4)?));
    }
    if [Some(&"send"), Some(&"show")].contains(&w.first()) {
        let separator = if w[0] == "send" { "as" } else { "in" };
        let split = w
            .iter()
            .position(|s| *s == separator)
            .context("missing action destination")?;
        if split + 2 != w.len() {
            bail!("invalid action destination");
        }
        let value = match &w[1..split] {
            [""] => Value::Literal(text(tokens, 1)?),
            ["the", "value", "of", _] => Value::Widget(name(tokens, 4)?),
            ["that", "item"] => Value::Item,
            ["the", "result"] => Value::Result,
            ["the", "error"] | ["its", "error"] => Value::Error,
            _ => bail!("unsupported action value"),
        };
        return Ok(if w[0] == "send" {
            Action::Send(value, name(tokens, split + 1)?)
        } else {
            Action::Show(value, name(tokens, split + 1)?)
        });
    }
    Ok(Action::Native(Step {
        line: line.number,
        kind: parser::parse_line(&line.tokens)?,
    }))
}
pub(crate) const MAX_TEXT_FIELD_BYTES: usize = 512;
pub(crate) fn validate_text_field(text: &str) -> Result<()> {
    if text.len() > MAX_TEXT_FIELD_BYTES {
        bail!("text field value exceeds 512 bytes");
    }
    if text.contains('\0') {
        bail!("text field value contains NUL");
    }
    Ok(())
}
pub(crate) fn validate_message(text: &str) -> Result<()> {
    if text.len() > 64 * 1024 {
        bail!("component message exceeds 64 KiB");
    }
    if text.contains('\0') {
        bail!("component text message contains NUL");
    }
    Ok(())
}
pub fn validate(screen: &Screen, registry: &Registry) -> Result<()> {
    // Public Rust callers must satisfy the same bounds as parsed .programs.
    if screen.title.is_empty() || screen.placements.is_empty() || screen.placements.len() > 2 {
        bail!("a screen requires a title and one or two placed programs");
    }
    if screen.components.len() > 16 || screen.links.len() > 32 {
        bail!("too many components or connections");
    }
    if screen.placements.iter().collect::<BTreeSet<_>>().len() != screen.placements.len() {
        bail!("duplicate placed program");
    }
    if !(10..=90).contains(&screen.first_width) {
        bail!("left column width must be between 10 and 90 percent");
    }
    for id in &screen.placements {
        if !screen.components.contains_key(id) {
            bail!("unknown placed program {id}");
        }
    }
    let mut shortcuts = BTreeSet::new();
    for id in &screen.placements {
        for handler in &screen.components[id].handlers {
            if let Event::Key(key) = &handler.event {
                if !shortcuts.insert(&key.label) {
                    bail!("duplicate screen shortcut {}", key.label);
                }
            }
        }
    }
    for link in &screen.links {
        if !screen.placements.contains(&link.source) || !screen.placements.contains(&link.target) {
            bail!("connection must use placed programs");
        }
        if !screen.components[&link.source]
            .outputs
            .contains(&link.output)
            || !screen.components[&link.target].inputs.contains(&link.input)
        {
            bail!("connection uses an undeclared port");
        }
        if link.source == link.target {
            bail!("self connections are not supported");
        }
    }
    // Prevent cycles that could flood the UI message loop.
    for root in &screen.placements {
        let mut pending = vec![root.as_str()];
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            for link in screen.links.iter().filter(|l| l.source == id) {
                if link.target == *root {
                    bail!("cyclic connections are not supported");
                }
                if seen.insert(link.target.as_str()) {
                    pending.push(&link.target);
                }
            }
        }
    }
    for component in screen.components.values() {
        if component.widgets.len() > 32 || component.handlers.len() > 32 {
            bail!("a program supports at most 32 widgets and 32 handlers");
        }
        if component
            .widgets
            .iter()
            .map(|widget| &widget.id)
            .collect::<BTreeSet<_>>()
            .len()
            != component.widgets.len()
        {
            bail!("duplicate widget name");
        }
        let widget = |id: &str| {
            component
                .widgets
                .iter()
                .find(|v| v.id == id)
                .with_context(|| format!("unknown widget {id}"))
        };
        for w in component.widgets.iter().filter(|w| w.kind == Kind::Input) {
            validate_text_field(&w.initial)?;
            if component.inputs.contains(&w.id) {
                bail!("text field {} shadows an input port", w.id);
            }
        }
        let text_widget = |id: &str| -> Result<()> {
            if matches!(
                widget(id)?.kind,
                Kind::Table | Kind::Chart | Kind::Bars | Kind::List
            ) {
                bail!("text display requires a heading, field, panel, badge, or button");
            }
            Ok(())
        };
        let mut names = component.inputs.clone();
        names.insert("error".into());
        names.extend(
            component
                .widgets
                .iter()
                .filter(|w| w.kind == Kind::Input)
                .map(|w| w.id.clone()),
        );
        // Button actions can route to other button handlers, but cannot form a loop.
        for root in component.widgets.iter().filter(|w| w.kind == Kind::Button) {
            let mut pending = vec![root.id.as_str()];
            let mut seen = BTreeSet::new();
            while let Some(id) = pending.pop() {
                for handler in component
                    .handlers
                    .iter()
                    .filter(|h| h.event == Event::Press(id.into()))
                {
                    for action in &handler.actions {
                        if let Action::Press(target) = action {
                            if target == &root.id {
                                bail!("cyclic button actions are not supported");
                            }
                            if seen.insert(target.as_str()) {
                                pending.push(target);
                            }
                        }
                    }
                }
            }
        }
        for handler in &component.handlers {
            match &handler.event {
                Event::Input(id) if !component.inputs.contains(id) => {
                    bail!("unknown input port {id}")
                }
                Event::Submit(id) if widget(id)?.kind != Kind::Input => {
                    bail!("submit requires a text field")
                }
                Event::Press(id) if widget(id)?.kind != Kind::Button => {
                    bail!("press requires a button")
                }
                Event::Choose(id) if widget(id)?.kind != Kind::List => {
                    bail!("choose requires a list")
                }
                _ => {}
            }
            let mut steps = Vec::new();
            let mut ended = false;
            for (index, action) in handler.actions.iter().enumerate() {
                match action {
                    Action::Native(step) => {
                        if matches!(step.kind, crate::ast::StepKind::ReadStdin { .. }) {
                            bail!("line {}: TUI handlers use component inputs and widget state, not read from stdin", step.line);
                        }
                        if ended {
                            bail!("put applet steps in one contiguous pipeline before display actions");
                        }
                        steps.push(step.clone());
                    }
                    other => {
                        ended |= !steps.is_empty();
                        match other {
                            Action::Press(id) => {
                                if widget(id)?.kind != Kind::Button {
                                    bail!("press requires a button");
                                }
                                if !component
                                    .handlers
                                    .iter()
                                    .any(|h| h.event == Event::Press(id.clone()))
                                {
                                    bail!("button {id} has no press handler");
                                }
                            }
                            Action::CloseScreen => {
                                if index + 1 != handler.actions.len() {
                                    bail!("close the screen must be the final action");
                                }
                            }
                            Action::Send(value, port) => {
                                if !component.outputs.contains(port) {
                                    bail!("undeclared output {port}");
                                }
                                validate_value(value, &widget, &handler.event, !steps.is_empty())?;
                                if let Value::Literal(text) = value {
                                    validate_message(text)?;
                                }
                            }
                            Action::Show(value, id) => {
                                text_widget(id)?;
                                validate_value(value, &widget, &handler.event, !steps.is_empty())?;
                                if widget(id)?.kind == Kind::Input {
                                    if let Value::Literal(text) = value {
                                        validate_text_field(text)?;
                                    }
                                }
                            }
                            Action::Bars(id) => {
                                if widget(id)?.kind != Kind::Bars {
                                    bail!("bars require a bar chart");
                                }
                                if steps.is_empty() && !matches!(handler.event, Event::Input(_)) {
                                    bail!("bars need input or pipeline output");
                                }
                            }
                            Action::Rows(id) => {
                                if widget(id)?.kind != Kind::Table {
                                    bail!("rows require a table");
                                }
                                if steps.is_empty() && !matches!(handler.event, Event::Input(_)) {
                                    bail!("rows need pipeline output");
                                }
                            }
                            Action::Field(_, id) => {
                                text_widget(id)?;
                                if steps.is_empty() && !matches!(handler.event, Event::Input(_)) {
                                    bail!("field display needs pipeline output");
                                }
                            }
                            Action::Point(_, id) => {
                                if widget(id)?.kind != Kind::Chart {
                                    bail!("points require a line chart");
                                }
                                if steps.is_empty() && !matches!(handler.event, Event::Input(_)) {
                                    bail!("chart points need input or pipeline output");
                                }
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
            if !steps.is_empty() {
                runtime::validate_with_inputs(
                    &Program {
                        definitions: Default::default(),
                        inputs: Vec::new(),
                        steps,
                        trigger: None,
                    },
                    registry,
                    names.clone(),
                    matches!(handler.event, Event::Input(_)),
                )?;
            }
        }
    }
    Ok(())
}
fn validate_value<'a>(
    value: &Value,
    widget: &impl Fn(&str) -> Result<&'a Widget>,
    event: &Event,
    has_output: bool,
) -> Result<()> {
    match value {
        Value::Widget(id) => {
            if matches!(
                widget(id)?.kind,
                Kind::List | Kind::Table | Kind::Chart | Kind::Bars
            ) {
                bail!("the value of requires a text widget");
            }
        }
        Value::Item if !matches!(event, Event::Choose(_)) => {
            bail!("that item requires a choose handler")
        }
        Value::Result if !has_output && !matches!(event, Event::Input(_)) => {
            bail!("the result needs input or pipeline output")
        }
        Value::Error if !matches!(event, Event::Error) => {
            bail!("the error requires a failure handler")
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod borrowed_token_tests {
    use super::*;

    #[test]
    fn parsed_screen_owns_values_after_source_is_dropped() {
        let screen = {
            let source = String::from(
                "open a screen titled \"Résumé\"\nplace app on the left\ndefine a program named app\n  show a text panel named status with text \"ready\"\n",
            );
            parse(&source).unwrap()
        };

        assert_eq!(screen.title, "Résumé");
        assert_eq!(screen.placements, ["app"]);
        assert_eq!(screen.components["app"].widgets[0].initial, "ready");
    }

    #[test]
    fn tui_native_handlers_share_loop_block_lowering() {
        let screen = parse("open a screen titled \"Loop\"\nplace app on the left\ndefine a program named app\n  show a text panel named status\n  whenever the program opens\n    repeat 2 times\n      for each item in [\"a\", \"b\"]\n        while false is true\n          run \"printf\" with arguments [item]\n    then show the result in status\n").unwrap();
        let actions = &screen.components["app"].handlers[0].actions;
        assert_eq!(actions.len(), 8);
        assert!(matches!(
            &actions[0],
            Action::Native(Step {
                kind: crate::ast::StepKind::Loop { end: 7, .. },
                line: 6
            })
        ));
        assert!(matches!(
            &actions[6],
            Action::Native(Step {
                kind: crate::ast::StepKind::EndLoop { back: 6 },
                line: 6
            })
        ));
        assert!(matches!(&actions[7], Action::Show(Value::Result, target) if target == "status"));
    }
}
