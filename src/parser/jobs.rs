//! Parser for named, scope-owned native pipeline jobs.

use super::{ensure_end, parse_binding_identifier, parse_line, word_is, BlockLine, Token};
use crate::ast::{Connection, JobStage, Step, StepKind};
use anyhow::{anyhow, bail, Context, Result};

const MAX_JOB_STAGES: usize = 16;

pub(super) fn parse_spawn<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
) -> Result<()> {
    let header = line_at(*cursor).expect("job spawn cursor");
    let name = (|| -> Result<String> {
        if !word_is(header.tokens.get(1), "as") {
            bail!("expected `spawn as <job>`");
        }
        let name = parse_binding_identifier(header.tokens.get(2), "job name")?;
        ensure_end(header.tokens, 3)?;
        Ok(name)
    })()
    .map_err(|error| anyhow!("line {}: {error:#}", header.number))?;

    let body_indent = line_at(*cursor + 1)
        .filter(|child| child.indent > indent)
        .map(|child| child.indent)
        .ok_or_else(|| {
            anyhow!(
                "line {}: spawn body must contain an indented `run` pipeline stage",
                header.number
            )
        })?;
    let mut stages = Vec::new();
    *cursor += 1;
    while let Some(child) = line_at(*cursor).filter(|child| child.indent > indent) {
        if let Some(label) = child.label {
            bail!(
                "line {}: background pipeline stages cannot have step labels ({label:?})",
                child.number
            );
        }
        if child.indent != body_indent {
            bail!(
                "line {}: background pipeline stages must use one consistent indentation level",
                child.number
            );
        }
        if stages.len() >= MAX_JOB_STAGES {
            bail!(
                "line {}: a background pipeline cannot contain more than {MAX_JOB_STAGES} stages",
                child.number
            );
        }
        let connection = if stages.is_empty() {
            if !word_is(child.tokens.first(), "run") {
                bail!(
                    "line {}: a background pipeline must start with `run`",
                    child.number
                );
            }
            Connection::Start
        } else {
            if !word_is(child.tokens.first(), "pipe") || !word_is(child.tokens.get(1), "to") {
                bail!(
                    "line {}: background pipelines accept only `pipe to` after the first stage",
                    child.number
                );
            }
            Connection::Pipe
        };
        let kind = parse_line(child.tokens)
            .map_err(|error| anyhow!("line {}: {error:#}", child.number))?;
        let StepKind::Invoke {
            mut call,
            connection: parsed_connection,
        } = kind
        else {
            bail!(
                "line {}: a background pipeline stage must be a native command",
                child.number
            );
        };
        super::forbid_retry(&call, "a background pipeline stage")?;
        if parsed_connection != connection {
            bail!(
                "line {}: invalid background pipeline connection",
                child.number
            );
        }
        if call.binding.is_some()
            || call.cache
            || call.discard_stderr
            || call.status_binding.is_some()
            || call.result_binding.is_some()
        {
            bail!(
                "line {}: background pipeline stages cannot bind output, status, or result, use `and cache`, or discard stderr",
                child.number
            );
        }
        // The job name and stage invocations own all source-derived data after
        // parsing. No child/control statement can be smuggled into this block.
        call.binding = None;
        stages.push(JobStage {
            line: child.number,
            call,
        });
        *cursor += 1;
    }
    if stages.is_empty() {
        bail!(
            "line {}: spawn requires at least one native pipeline stage",
            header.number
        );
    }
    steps.push(Step {
        line: header.number,
        label: None,
        kind: StepKind::SpawnJob { name, stages },
    });
    Ok(())
}

pub(super) fn parse_action(tokens: &[Token<'_>]) -> Result<StepKind> {
    match tokens.first() {
        Some(Token::Word("inspect")) => {
            let name = parse_binding_identifier(tokens.get(1), "job name")?;
            if !word_is(tokens.get(2), "as") {
                bail!("expected `inspect <job> as <binding>`");
            }
            let binding = parse_binding_identifier(tokens.get(3), "job snapshot binding")?;
            ensure_end(tokens, 4)?;
            Ok(StepKind::InspectJob { name, binding })
        }
        Some(Token::Word("await")) => {
            let name = parse_binding_identifier(tokens.get(1), "job name")?;
            let mut cursor = 2;
            let mut status_binding = None;
            let mut result_binding = None;
            if is_suffix(tokens, cursor, "status") {
                status_binding = Some(parse_suffix_binding(tokens, &mut cursor, "status")?);
            }
            if is_suffix(tokens, cursor, "result") {
                result_binding = Some(parse_suffix_binding(tokens, &mut cursor, "result")?);
            }
            ensure_end(tokens, cursor)?;
            Ok(StepKind::AwaitJob {
                name,
                status_binding,
                result_binding,
            })
        }
        Some(Token::Word("cancel")) => {
            let name = parse_binding_identifier(tokens.get(1), "job name")?;
            ensure_end(tokens, 2)?;
            Ok(StepKind::CancelJob { name })
        }
        _ => bail!("expected `inspect`, `await`, or `cancel` job statement"),
    }
}

fn is_suffix(tokens: &[Token<'_>], cursor: usize, suffix: &str) -> bool {
    word_is(tokens.get(cursor), "and")
        && word_is(tokens.get(cursor + 1), "keep")
        && word_is(tokens.get(cursor + 2), suffix)
}

fn parse_suffix_binding(tokens: &[Token<'_>], cursor: &mut usize, suffix: &str) -> Result<String> {
    if !is_suffix(tokens, *cursor, suffix) || !word_is(tokens.get(*cursor + 3), "as") {
        bail!("expected `and keep {suffix} as <name>`");
    }
    *cursor += 4;
    let name = parse_binding_identifier(tokens.get(*cursor), &format!("job {suffix} binding"))
        .with_context(|| format!("invalid job {suffix} binding"))?;
    *cursor += 1;
    Ok(name)
}
