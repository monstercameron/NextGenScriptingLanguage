//! Parser for bounded `parallel` and `race` task groups.

use super::{BlockLine, Token};
use crate::ast::{Connection, Invocation, Step, StepKind, TaskEach, TaskGroupMode, Value};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;

const MAX_TASKS: usize = 256;
const MAX_PARALLEL: usize = 64;

pub(super) fn parse_group<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
) -> Result<()> {
    let header = line_at(*cursor).expect("task group cursor");
    if super::word_is(header.tokens.first(), "parallel")
        && super::word_is(header.tokens.get(1), "for")
    {
        return parse_each(line_at, cursor, indent, steps);
    }
    let mode = match header.tokens.first() {
        Some(Token::Word("parallel")) => TaskGroupMode::Join,
        Some(Token::Word("race")) => TaskGroupMode::Race,
        _ => bail!("expected `parallel` or `race` task group"),
    };
    let parse_header = || -> Result<(usize, String)> {
        if !super::word_is(header.tokens.get(1), "at")
            || !super::word_is(header.tokens.get(2), "most")
        {
            bail!("expected `parallel at most <positive integer> as <result>` or `race at most <positive integer> as <result>`");
        }
        let mut token = 3;
        let limit = match super::parse_value(header.tokens, &mut token)? {
            Value::Integer(value) => usize::try_from(value)
                .ok()
                .filter(|value| (1..=MAX_PARALLEL).contains(value))
                .context("task limit must be between 1 and 64")?,
            _ => bail!("task limit must be a positive integer literal"),
        };
        if !super::word_is(header.tokens.get(token), "as") {
            bail!("task group requires `as <result>`")
        }
        let binding =
            super::parse_binding_identifier(header.tokens.get(token + 1), "task group result")?;
        super::ensure_end(header.tokens, token + 2)?;
        Ok((limit, binding))
    };
    let (limit, binding) =
        parse_header().map_err(|error| anyhow!("line {}: {error:#}", header.number))?;

    let body_indent = line_at(*cursor + 1)
        .filter(|child| child.indent > indent)
        .map(|child| child.indent)
        .ok_or_else(|| {
            anyhow!(
                "line {}: task group body must contain an indented `run` statement",
                header.number
            )
        })?;
    let mut tasks: Vec<Invocation> = Vec::new();
    let mut names = BTreeSet::new();
    *cursor += 1;
    while let Some(child) = line_at(*cursor).filter(|child| child.indent > indent) {
        if let Some(label) = child.label {
            bail!(
                "line {}: task group entries cannot have step labels ({label:?})",
                child.number
            );
        }
        if child.indent != body_indent {
            bail!(
                "line {}: task group tasks must use one consistent indentation level",
                child.number
            );
        }
        if tasks.len() >= MAX_TASKS {
            bail!(
                "line {}: task group cannot contain more than {MAX_TASKS} tasks",
                child.number
            );
        }
        if !super::word_is(child.tokens.first(), "run") {
            bail!("line {}: task group bodies accept only independent `run ... as <task>` applet calls", child.number);
        }
        let kind = super::parse_line(child.tokens)
            .map_err(|error| anyhow!("line {}: {error:#}", child.number))?;
        let StepKind::Invoke {
            call,
            connection: Connection::Start,
        } = kind
        else {
            bail!(
                "line {}: task group entries must be independent applet calls",
                child.number
            );
        };
        super::forbid_retry(&call, "a task group entry")?;
        if call.direct {
            bail!("line {}: task groups require named applets; direct quoted commands are unsupported", child.number);
        }
        if call.cache || call.status_binding.is_some() || call.result_binding.is_some() {
            bail!(
                "line {}: task group entries cannot use `and cache`, `and keep status`, or `and keep result`",
                child.number
            );
        }
        let task_name = call
            .binding
            .as_deref()
            .context("task group entries require `as <task>`")?;
        if task_name == binding {
            bail!(
                "line {}: task identifier cannot match the group result binding",
                child.number
            );
        }
        if !names.insert(task_name.to_owned()) {
            bail!(
                "line {}: duplicate task identifier {task_name:?}",
                child.number
            );
        }
        tasks.push(call);
        *cursor += 1;
    }
    if tasks.is_empty() {
        bail!(
            "line {}: task group requires at least one task",
            header.number
        );
    }
    steps.push(Step {
        line: header.number,
        label: None,
        kind: StepKind::TaskGroup {
            mode,
            limit,
            binding,
            tasks,
            each: None,
        },
    });
    Ok(())
}

/// `parallel for each <item> in <list> at most <N> as <result>` followed by one
/// indented `run` template. Unlike fixed groups, the template may be a quoted
/// direct executable, and every element produces a record instead of failing
/// the group on the first non-success.
fn parse_each<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
) -> Result<()> {
    let header = line_at(*cursor).expect("task group cursor");
    let parse_header = || -> Result<(TaskEach, usize, String)> {
        let usage =
            "expected `parallel for each <name> in <list> at most <positive integer> as <result>`";
        if !super::word_is(header.tokens.get(2), "each")
            || !super::word_is(header.tokens.get(4), "in")
        {
            bail!(usage);
        }
        let item = super::parse_binding_identifier(header.tokens.get(3), "iteration binding")?;
        let mut token = 5;
        let values = super::parse_value(header.tokens, &mut token)?;
        if !super::word_is(header.tokens.get(token), "at")
            || !super::word_is(header.tokens.get(token + 1), "most")
        {
            bail!(usage);
        }
        token += 2;
        let limit = match super::parse_value(header.tokens, &mut token)? {
            Value::Integer(value) => usize::try_from(value)
                .ok()
                .filter(|value| (1..=MAX_PARALLEL).contains(value))
                .context("task limit must be between 1 and 64")?,
            _ => bail!("task limit must be a positive integer literal"),
        };
        if !super::word_is(header.tokens.get(token), "as") {
            bail!("parallel for each requires `as <result>`")
        }
        let binding =
            super::parse_binding_identifier(header.tokens.get(token + 1), "task group result")?;
        super::ensure_end(header.tokens, token + 2)?;
        if binding == item {
            bail!("iteration binding cannot match the group result binding");
        }
        Ok((
            TaskEach {
                binding: item,
                values,
            },
            limit,
            binding,
        ))
    };
    let (each, limit, binding) =
        parse_header().map_err(|error| anyhow!("line {}: {error:#}", header.number))?;

    *cursor += 1;
    let child = line_at(*cursor)
        .filter(|child| child.indent > indent)
        .ok_or_else(|| {
            anyhow!(
                "line {}: parallel for each requires one indented `run` statement",
                header.number
            )
        })?;
    if !super::word_is(child.tokens.first(), "run") {
        bail!(
            "line {}: parallel for each bodies accept one `run` command",
            child.number
        );
    }
    let kind = super::parse_line(child.tokens)
        .map_err(|error| anyhow!("line {}: {error:#}", child.number))?;
    let StepKind::Invoke {
        call,
        connection: Connection::Start,
    } = kind
    else {
        bail!(
            "line {}: parallel for each runs one independent command",
            child.number
        );
    };
    super::forbid_retry(&call, "a parallel for each command")?;
    if call.binding.is_some()
        || call.cache
        || call.discard_stderr
        || call.status_binding.is_some()
        || call.result_binding.is_some()
    {
        bail!(
            "line {}: the parallel for each command cannot use `as`, `and cache`, `and discard stderr`, `and keep status`, or `and keep result`; every element's status and output are in the result records",
            child.number
        );
    }
    *cursor += 1;
    if let Some(extra) = line_at(*cursor).filter(|next| next.indent > indent) {
        bail!(
            "line {}: parallel for each accepts exactly one `run` command",
            extra.number
        );
    }
    steps.push(Step {
        line: header.number,
        label: None,
        kind: StepKind::TaskGroup {
            mode: TaskGroupMode::Join,
            limit,
            binding,
            tasks: vec![call],
            each: Some(each),
        },
    });
    Ok(())
}
