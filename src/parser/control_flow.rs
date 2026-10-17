//! Source syntax for lexical transfers and structured error/cleanup blocks.
use super::{
    ensure_end, parse_binding_identifier, parse_statement_in_scope, parse_value, word_is,
    BlockLine, StatementScope, Token, MAX_CONTROL_DEPTH,
};
use crate::ast::{Step, StepKind, Value};
use anyhow::{anyhow, bail, Context, Result};

pub(super) fn parse_transfer(
    tokens: &[Token<'_>],
    scope: StatementScope,
    line: usize,
) -> Result<Option<StepKind>> {
    let kind = match tokens.first() {
        Some(Token::Word("break")) => {
            ensure_end(tokens, 1).map_err(|error| anyhow!("line {line}: {error}"))?;
            if scope.loop_depth == 0 {
                bail!("line {line}: break is only valid inside a loop");
            }
            Some(StepKind::BreakLoop)
        }
        Some(Token::Word("continue")) => {
            ensure_end(tokens, 1).map_err(|error| anyhow!("line {line}: {error}"))?;
            if scope.loop_depth == 0 {
                bail!("line {line}: continue is only valid inside a loop");
            }
            Some(StepKind::ContinueLoop)
        }
        Some(Token::Word("return")) => {
            ensure_end(tokens, 1).map_err(|error| anyhow!("line {line}: {error}"))?;
            if !scope.reusable_program {
                bail!("line {line}: return is only valid inside a reusable program");
            }
            Some(StepKind::Return)
        }
        Some(Token::Word("exit"))
            if word_is(tokens.get(1), "with") && word_is(tokens.get(2), "status") =>
        {
            Some(parse_exit(tokens).map_err(|error| anyhow!("line {line}: {error:#}"))?)
        }
        Some(Token::Word("fail"))
            if word_is(tokens.get(1), "with") && word_is(tokens.get(2), "message") =>
        {
            Some(parse_fail(tokens).map_err(|error| anyhow!("line {line}: {error:#}"))?)
        }
        _ => None,
    };
    Ok(kind)
}

/// `exit with status <integer literal or integer binding>`.
fn parse_exit(tokens: &[Token<'_>]) -> Result<StepKind> {
    let mut cursor = 3;
    let status = parse_value(tokens, &mut cursor).context("exit status")?;
    ensure_end(tokens, cursor)?;
    check_status_operand(&status, 0, "exit status")?;
    Ok(StepKind::Exit { status })
}

/// `fail with message <text> [and status <integer>]`; the status defaults to 1.
fn parse_fail(tokens: &[Token<'_>]) -> Result<StepKind> {
    let mut cursor = 3;
    let message = parse_value(tokens, &mut cursor).context("fail message")?;
    if !matches!(message, Value::String(_) | Value::Reference(_)) {
        bail!("fail message must be a quoted text literal or text binding");
    }
    let status = if word_is(tokens.get(cursor), "and") && word_is(tokens.get(cursor + 1), "status")
    {
        cursor += 2;
        let status = parse_value(tokens, &mut cursor).context("fail status")?;
        check_status_operand(&status, 1, "fail status")?;
        Some(status)
    } else {
        None
    };
    ensure_end(tokens, cursor)?;
    Ok(StepKind::Fail { message, status })
}

/// Literal statuses are range-checked before any effect; references are
/// type-checked against the binding table during validation.
fn check_status_operand(value: &Value, minimum: i64, what: &str) -> Result<()> {
    match value {
        Value::Integer(status) if (minimum..=255).contains(status) => Ok(()),
        Value::Integer(status) => {
            bail!("{what} must be between {minimum} and 255, received {status}")
        }
        Value::Reference(_) => Ok(()),
        _ => bail!("{what} must be an integer literal or integer binding"),
    }
}

pub(super) fn parse_try<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
    depth: usize,
    scope: StatementScope,
) -> Result<()> {
    let header = line_at(*cursor).expect("try header cursor");
    ensure_end(header.tokens, 1).map_err(|error| anyhow!("line {}: {error}", header.number))?;
    if depth >= MAX_CONTROL_DEPTH {
        bail!(
            "line {}: control nesting exceeds {MAX_CONTROL_DEPTH} levels",
            header.number
        );
    }
    let start = steps.len();
    steps.push(Step {
        line: header.number,
        kind: StepKind::Try {
            catch: None,
            finally: None,
            end: 0,
        },
    });
    *cursor += 1;
    parse_body(
        line_at,
        cursor,
        indent,
        steps,
        depth,
        scope,
        header.number,
        "try",
    )?;

    let mut catch_index = None;
    let mut finally_index = None;
    if let Some(clause) = line_at(*cursor).filter(|line| {
        line.indent == indent && matches!(line.tokens.first(), Some(Token::Word("catch")))
    }) {
        if finally_index.is_some() {
            bail!("line {}: catch must precede finally", clause.number);
        }
        let binding = parse_binding_identifier(clause.tokens.get(1), "catch binding")
            .map_err(|error| anyhow!("line {}: {error}", clause.number))?;
        ensure_end(clause.tokens, 2).map_err(|error| anyhow!("line {}: {error}", clause.number))?;
        catch_index = Some(steps.len());
        steps.push(Step {
            line: clause.number,
            kind: StepKind::Catch { binding, end: 0 },
        });
        *cursor += 1;
        parse_body(
            line_at,
            cursor,
            indent,
            steps,
            depth,
            scope,
            clause.number,
            "catch",
        )?;
    }
    if let Some(clause) = line_at(*cursor).filter(|line| {
        line.indent == indent && matches!(line.tokens.first(), Some(Token::Word("finally")))
    }) {
        ensure_end(clause.tokens, 1).map_err(|error| anyhow!("line {}: {error}", clause.number))?;
        finally_index = Some(steps.len());
        steps.push(Step {
            line: clause.number,
            kind: StepKind::Finally,
        });
        *cursor += 1;
        parse_body(
            line_at,
            cursor,
            indent,
            steps,
            depth,
            scope,
            clause.number,
            "finally",
        )?;
    }
    if catch_index.is_none() && finally_index.is_none() {
        bail!(
            "line {}: try requires a catch or finally clause",
            header.number
        );
    }
    if line_at(*cursor).is_some_and(|line| {
        line.indent == indent
            && matches!(line.tokens.first(), Some(Token::Word("catch" | "finally")))
    }) {
        let line = line_at(*cursor).expect("clause line");
        bail!("line {}: duplicate or out-of-order try clause", line.number);
    }
    let end_index = steps.len();
    steps.push(Step {
        line: header.number,
        kind: StepKind::EndTry,
    });
    let finish = steps.len();
    if let StepKind::Try {
        catch,
        finally,
        end,
    } = &mut steps[start].kind
    {
        *catch = catch_index.map(|index| index - start);
        *finally = finally_index.map(|index| index - start);
        *end = finish - start;
    }
    if let Some(index) = catch_index {
        let target = finally_index.unwrap_or(end_index);
        if let StepKind::Catch { end, .. } = &mut steps[index].kind {
            *end = target - index;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // These values are the parser's per-block context.
fn parse_body<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
    depth: usize,
    scope: StatementScope,
    line: usize,
    name: &str,
) -> Result<()> {
    let body_indent = line_at(*cursor)
        .filter(|child| child.indent > indent)
        .map(|child| child.indent)
        .ok_or_else(|| anyhow!("line {line}: {name} body must contain an indented statement"))?;
    let before = steps.len();
    while line_at(*cursor).is_some_and(|child| child.indent > indent) {
        parse_statement_in_scope(line_at, cursor, body_indent, steps, depth + 1, scope)?;
    }
    if steps.len() == before {
        bail!("line {line}: {name} body cannot be empty");
    }
    Ok(())
}
