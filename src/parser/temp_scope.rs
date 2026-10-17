//! `with temporary file|directory as NAME`: an indented body that sees a fresh private
//! path, removed when the body ends.
use super::*;
use crate::ast::TempKind;

pub(super) fn parse_temp_scope<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
    depth: usize,
    scope: StatementScope,
) -> Result<()> {
    let line = line_at(*cursor).expect("temporary scope cursor");
    if depth >= MAX_CONTROL_DEPTH {
        bail!(
            "line {}: control nesting exceeds {MAX_CONTROL_DEPTH} levels",
            line.number
        );
    }
    let (kind, binding) = (|| -> Result<(TempKind, String)> {
        if !word_is(line.tokens.get(1), "temporary") || !word_is(line.tokens.get(3), "as") {
            bail!(
                "expected `with temporary file as <name>` or `with temporary directory as <name>`"
            );
        }
        let kind = if word_is(line.tokens.get(2), "file") {
            TempKind::File
        } else if word_is(line.tokens.get(2), "directory") {
            TempKind::Directory
        } else {
            bail!("expected `file` or `directory` after `with temporary`");
        };
        let binding = parse_binding_identifier(line.tokens.get(4), "temporary binding")?;
        ensure_end(line.tokens, 5)?;
        Ok((kind, binding))
    })()
    .map_err(|error| anyhow!("line {}: {error:#}", line.number))?;
    let start = steps.len();
    steps.push(Step {
        line: line.number,
        label: None,
        kind: StepKind::TempScope {
            kind,
            binding,
            end: 0,
        },
    });
    *cursor += 1;
    let body_indent = line_at(*cursor)
        .filter(|child| child.indent > indent)
        .map(|child| child.indent)
        .ok_or_else(|| {
            anyhow!(
                "line {}: temporary scope body must contain an indented statement",
                line.number
            )
        })?;
    while line_at(*cursor).is_some_and(|child| child.indent > indent) {
        parse_statement_in_scope(line_at, cursor, body_indent, steps, depth + 1, scope)?;
    }
    let back = steps.len() - start;
    steps.push(Step {
        line: line.number,
        label: None,
        kind: StepKind::EndTempScope { back },
    });
    if let StepKind::TempScope { end, .. } = &mut steps[start].kind {
        *end = back + 1;
    }
    Ok(())
}
