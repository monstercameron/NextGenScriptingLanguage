use crate::ast::{
    Condition, ConditionOperator, Connection, InputDeclaration, InputSource, InputType, Invocation,
    ListenSource, LoopKind, Program, RedirectTarget, Step, StepKind, Trigger, Value,
};
use anyhow::{anyhow, bail, Context, Result};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Token<'source> {
    Word(&'source str),
    String(Cow<'source, str>),
    LeftBracket,
    RightBracket,
    Comma,
    LeftBrace,
    RightBrace,
    Colon,
}

struct SourceLine {
    number: usize,
    indent: usize,
    token_range: Range<usize>,
}

impl SourceLine {
    fn tokens<'arena, 'source>(
        &self,
        token_arena: &'arena [Token<'source>],
    ) -> &'arena [Token<'source>] {
        &token_arena[self.token_range.clone()]
    }
}

/// Shared block parser input, also used for native command blocks in TUI handlers.
pub(crate) struct BlockLine<'arena, 'source> {
    pub number: usize,
    pub indent: usize,
    pub tokens: &'arena [Token<'source>],
}

const MAX_CONTROL_DEPTH: usize = 64;
#[derive(Debug, Clone, Copy, Default)]
struct StatementScope {
    loop_depth: usize,
    reusable_program: bool,
}

mod control_flow;
mod jobs;
mod reusable;
mod tasks;

/// Lower one indented statement into relative control instructions. Offsets remain
/// valid when a complete block is copied into a managed execution node.
pub(crate) fn parse_statement<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
    depth: usize,
) -> Result<()> {
    parse_statement_in_scope(
        line_at,
        cursor,
        indent,
        steps,
        depth,
        StatementScope::default(),
    )
}

pub(super) fn parse_reusable_statement<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
    depth: usize,
) -> Result<()> {
    parse_statement_in_scope(
        line_at,
        cursor,
        indent,
        steps,
        depth,
        StatementScope {
            loop_depth: 0,
            reusable_program: true,
        },
    )
}

fn parse_statement_in_scope<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
    depth: usize,
    scope: StatementScope,
) -> Result<()> {
    let line = line_at(*cursor).expect("statement cursor");
    if line.indent != indent {
        bail!("line {}: inconsistent block indentation", line.number);
    }
    if is_trigger_header(line.tokens) || is_listener_event_header(line.tokens) {
        bail!(
            "line {}: nested trigger blocks are unsupported",
            line.number
        );
    }
    if word_is(line.tokens.first(), "define") {
        bail!(
            "line {}: reusable program definitions must be top-level",
            line.number
        );
    }
    if word_is(line.tokens.first(), "otherwise") {
        bail!(
            "line {}: otherwise must follow a matching if body",
            line.number
        );
    }
    if let Some(kind) = control_flow::parse_transfer(line.tokens, scope, line.number)? {
        steps.push(Step {
            line: line.number,
            kind,
        });
        *cursor += 1;
        return Ok(());
    }
    if word_is(line.tokens.first(), "try") {
        return control_flow::parse_try(line_at, cursor, indent, steps, depth, scope);
    }
    if matches!(line.tokens.first(), Some(Token::Word("catch" | "finally"))) {
        bail!(
            "line {}: catch and finally must belong to a try block",
            line.number
        );
    }
    if matches!(
        line.tokens.first(),
        Some(Token::Word("repeat" | "for" | "while"))
    ) {
        return parse_loop(line_at, cursor, indent, steps, depth, scope);
    }
    if word_is(line.tokens.first(), "spawn") {
        return jobs::parse_spawn(line_at, cursor, indent, steps);
    }
    if matches!(
        line.tokens.first(),
        Some(Token::Word("inspect" | "await" | "cancel"))
    ) {
        let kind = jobs::parse_action(line.tokens)
            .map_err(|error| anyhow!("line {}: {error:#}", line.number))?;
        steps.push(Step {
            line: line.number,
            kind,
        });
        *cursor += 1;
        return Ok(());
    }
    if matches!(line.tokens.first(), Some(Token::Word("parallel" | "race"))) {
        if depth >= MAX_CONTROL_DEPTH {
            bail!(
                "line {}: control nesting exceeds {MAX_CONTROL_DEPTH} levels",
                line.number
            );
        }
        return tasks::parse_group(line_at, cursor, indent, steps);
    }
    if !word_is(line.tokens.first(), "if") {
        for kind in parse_simple_statement(line.tokens)
            .map_err(|error| anyhow!("line {}: {error}", line.number))?
        {
            steps.push(Step {
                line: line.number,
                kind,
            });
        }
        *cursor += 1;
        return Ok(());
    }
    parse_if_block(line_at, cursor, indent, steps, depth, scope, &line)
}

/// Parse one `if` block whose header is `line`, starting at `*cursor`. An
/// `otherwise if` clause calls this again for the rest of its chain.
fn parse_if_block<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
    depth: usize,
    scope: StatementScope,
    line: &BlockLine<'arena, 'source>,
) -> Result<()> {
    if depth >= MAX_CONTROL_DEPTH {
        bail!(
            "line {}: conditional nesting exceeds {MAX_CONTROL_DEPTH} levels",
            line.number
        );
    }
    let head = parse_condition_head(line.tokens, "if")
        .map_err(|error| anyhow!("line {}: {error:#}", line.number))?;
    let start = steps.len();
    let kind = match head {
        ConditionHead::Value(condition) => StepKind::If {
            condition,
            otherwise: 0,
            end: 0,
        },
        ConditionHead::Command {
            call,
            expect_success,
        } => StepKind::IfCommand {
            call,
            expect_success,
            otherwise: 0,
            end: 0,
        },
    };
    steps.push(Step {
        line: line.number,
        kind,
    });
    *cursor += 1;
    let body_indent = line_at(*cursor)
        .filter(|child| child.indent > indent)
        .map(|child| child.indent)
        .ok_or_else(|| {
            anyhow!(
                "line {}: if body must contain an indented statement",
                line.number
            )
        })?;
    while line_at(*cursor).is_some_and(|child| child.indent > indent) {
        parse_statement_in_scope(line_at, cursor, body_indent, steps, depth + 1, scope)?;
    }
    let mut alternative = None;
    if let Some(otherwise) = line_at(*cursor)
        .filter(|child| child.indent == indent && word_is(child.tokens.first(), "otherwise"))
    {
        alternative = Some(steps.len());
        if word_is(otherwise.tokens.get(1), "if") {
            // `otherwise if` puts the rest of the chain in the else branch. The
            // nested block consumes this line and its own EndIf.
            steps.push(Step {
                line: otherwise.number,
                kind: StepKind::Otherwise { end: 0 },
            });
            let nested = BlockLine {
                number: otherwise.number,
                indent: otherwise.indent,
                tokens: &otherwise.tokens[1..],
            };
            parse_if_block(line_at, cursor, indent, steps, depth + 1, scope, &nested)?;
        } else {
            ensure_end(otherwise.tokens, 1)
                .map_err(|error| anyhow!("line {}: {error}", otherwise.number))?;
            steps.push(Step {
                line: otherwise.number,
                kind: StepKind::Otherwise { end: 0 },
            });
            *cursor += 1;
            if line_at(*cursor).is_none_or(|child| child.indent <= indent) {
                bail!(
                    "line {}: otherwise body must contain an indented statement",
                    otherwise.number
                );
            }
            while line_at(*cursor).is_some_and(|child| child.indent > indent) {
                parse_statement_in_scope(line_at, cursor, body_indent, steps, depth + 1, scope)?;
            }
        }
    }
    steps.push(Step {
        line: line.number,
        kind: StepKind::EndIf,
    });
    let finish = steps.len();
    match &mut steps[start].kind {
        StepKind::If { otherwise, end, .. } | StepKind::IfCommand { otherwise, end, .. } => {
            *otherwise = alternative.map_or(finish, |index| index + 1) - start;
            *end = finish - start;
        }
        _ => {}
    }
    if let Some(index) = alternative {
        steps[index].kind = StepKind::Otherwise {
            end: finish - index,
        };
    }
    Ok(())
}

fn parse_condition(tokens: &[Token<'_>], keyword: &str) -> Result<Condition> {
    let mut token = 1;
    let left = parse_value(tokens, &mut token).with_context(|| format!("{keyword} left value"))?;
    let (operator, negated) = if word_is(tokens.get(token), "is") {
        token += 1;
        let negated = word_is(tokens.get(token), "not");
        token += usize::from(negated);
        (ConditionOperator::Equals, negated)
    } else if word_is(tokens.get(token), "contains") {
        token += 1;
        (ConditionOperator::Contains, false)
    } else if word_is(tokens.get(token), "does")
        && word_is(tokens.get(token + 1), "not")
        && word_is(tokens.get(token + 2), "contain")
    {
        token += 3;
        (ConditionOperator::Contains, true)
    } else {
        bail!(
            "expected `{keyword} <value> is [not] <value>` or `{keyword} <value> contains <value>`"
        );
    };
    let right =
        parse_value(tokens, &mut token).with_context(|| format!("{keyword} right value"))?;
    ensure_end(tokens, token)?;
    Ok(Condition {
        left,
        right,
        negated,
        operator,
    })
}

/// A conditional head is either a typed value comparison or one command whose
/// outcome decides the branch. The `run` keyword selects the command form.
enum ConditionHead {
    Value(Condition),
    Command {
        call: Invocation,
        expect_success: bool,
    },
}

fn parse_condition_head(tokens: &[Token<'_>], keyword: &str) -> Result<ConditionHead> {
    if !word_is(tokens.get(1), "run") {
        return Ok(ConditionHead::Value(parse_condition(tokens, keyword)?));
    }
    let expect_success = match tokens.last() {
        Some(Token::Word("succeeds")) if tokens.len() > 3 => true,
        Some(Token::Word("fails")) if tokens.len() > 3 => false,
        _ => {
            bail!("expected `{keyword} run <command> succeeds` or `{keyword} run <command> fails`")
        }
    };
    let StepKind::Invoke { call, connection } = parse_invocation(&tokens[1..tokens.len() - 1])?
    else {
        bail!("expected a command after `{keyword} run`");
    };
    if connection != Connection::Start
        || call.binding.is_some()
        || call.status_binding.is_some()
        || call.result_binding.is_some()
        || call.cache
        || call.discard_stderr
    {
        bail!(
            "a command condition takes only an applet or quoted executable with arguments; it cannot bind, keep status or result, cache, or discard stderr"
        );
    }
    Ok(ConditionHead::Command {
        call,
        expect_success,
    })
}

fn parse_loop<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    cursor: &mut usize,
    indent: usize,
    steps: &mut Vec<Step>,
    depth: usize,
    scope: StatementScope,
) -> Result<()> {
    let line = line_at(*cursor).expect("loop cursor");
    if depth >= MAX_CONTROL_DEPTH {
        bail!(
            "line {}: control nesting exceeds {MAX_CONTROL_DEPTH} levels",
            line.number
        );
    }
    let kind = (|| -> Result<LoopKind> {
        if word_is(line.tokens.first(), "repeat") {
            let mut token = 1;
            let count = parse_value(line.tokens, &mut token)?;
            if !word_is(line.tokens.get(token), "times") {
                bail!("expected `repeat <integer value> times`");
            }
            ensure_end(line.tokens, token + 1)?;
            Ok(LoopKind::Repeat { count })
        } else if word_is(line.tokens.first(), "for") {
            if !word_is(line.tokens.get(1), "each") || !word_is(line.tokens.get(3), "in") {
                bail!("expected `for each <name> in <text-list value>`");
            }
            let binding = parse_binding_identifier(line.tokens.get(2), "iteration binding")?;
            let mut token = 4;
            let values = parse_value(line.tokens, &mut token)?;
            ensure_end(line.tokens, token)?;
            Ok(LoopKind::ForEach { binding, values })
        } else {
            match parse_condition_head(line.tokens, "while")? {
                ConditionHead::Value(condition) => Ok(LoopKind::While { condition }),
                ConditionHead::Command {
                    call,
                    expect_success,
                } => Ok(LoopKind::WhileCommand {
                    call,
                    expect_success,
                }),
            }
        }
    })()
    .map_err(|error| anyhow!("line {}: {error:#}", line.number))?;
    let start = steps.len();
    steps.push(Step {
        line: line.number,
        kind: StepKind::Loop { kind, end: 0 },
    });
    *cursor += 1;
    let body_indent = line_at(*cursor)
        .filter(|child| child.indent > indent)
        .map(|child| child.indent)
        .ok_or_else(|| {
            anyhow!(
                "line {}: loop body must contain an indented statement",
                line.number
            )
        })?;
    while line_at(*cursor).is_some_and(|child| child.indent > indent) {
        parse_statement_in_scope(
            line_at,
            cursor,
            body_indent,
            steps,
            depth + 1,
            StatementScope {
                loop_depth: scope.loop_depth + 1,
                ..scope
            },
        )?;
    }
    let back = steps.len() - start;
    steps.push(Step {
        line: line.number,
        kind: StepKind::EndLoop { back },
    });
    if let StepKind::Loop { end, .. } = &mut steps[start].kind {
        *end = back + 1;
    }
    Ok(())
}

/// One statement after joining its continuation lines and raw block strings.
struct LogicalLine {
    number: usize,
    indent: usize,
    text: String,
}

/// Physical lines form statements. An open `[` or `{` continues the statement onto
/// following lines until the brackets balance. A piece ending in ` """` takes a raw
/// block up to a line holding only `"""`; the block becomes one quoted string with
/// no escape processing, so embedded programs need no backslash escaping.
fn logical_lines(source: &str) -> Result<Vec<LogicalLine>> {
    let physical: Vec<&str> = source.lines().collect();
    let mut statements = Vec::new();
    let mut index = 0;
    while index < physical.len() {
        let number = index + 1;
        let first = strip_comment(physical[index]);
        index += 1;
        if first.trim().is_empty() {
            continue;
        }
        let mut indent = 0;
        for byte in first.bytes() {
            match byte {
                b' ' => indent += 1,
                b'\t' => bail!("line {number}: tabs are not allowed for indentation"),
                _ => break,
            }
        }
        let mut text = take_piece(first.trim_start_matches(' '), number, &physical, &mut index)?;
        // Most statements have no bracket, so skip the second tokenize for them.
        while (text.contains('[') || text.contains('{')) && bracket_depth(&text, number)? > 0 {
            let Some(next) = physical.get(index) else {
                bail!("line {number}: statement ends inside an open bracket or brace");
            };
            index += 1;
            let next = strip_comment(next);
            if next.trim().is_empty() {
                continue;
            }
            text.push(' ');
            text.push_str(&take_piece(next.trim(), number, &physical, &mut index)?);
        }
        statements.push(LogicalLine {
            number,
            indent,
            text,
        });
    }
    Ok(statements)
}

/// Returns one piece of a statement, with a raw block replaced by its quoted text.
fn take_piece(piece: &str, number: usize, physical: &[&str], index: &mut usize) -> Result<String> {
    let trimmed = piece.trim_end();
    if !trimmed.ends_with(" \"\"\"") {
        return Ok(piece.to_owned());
    }
    let mut body = Vec::new();
    loop {
        let Some(raw) = physical.get(*index) else {
            bail!("line {number}: raw block string has no closing \"\"\" line");
        };
        *index += 1;
        if raw.trim() == "\"\"\"" {
            break;
        }
        body.push(*raw);
    }
    let quoted = serde_json::to_string(&body.join("\n")).context("encoding raw block string")?;
    Ok(format!("{}{quoted}", &trimmed[..trimmed.len() - 3]))
}

/// Net count of open brackets and braces in a statement's tokens.
fn bracket_depth(text: &str, number: usize) -> Result<i64> {
    let mut tokens = Vec::new();
    tokenize_into(text, &mut tokens).with_context(|| format!("line {number}"))?;
    Ok(tokens.iter().fold(0, |depth, token| match token {
        Token::LeftBracket | Token::LeftBrace => depth + 1,
        Token::RightBracket | Token::RightBrace => depth - 1,
        _ => depth,
    }))
}

/// The language version this build implements. Unmarked programs are this version.
pub const LANGUAGE_VERSION: u32 = 1;

/// A first statement `execute N` declares the language version it was written for.
/// Only the implemented version is accepted; other versions fail with an upgrade message.
fn take_language_version(statements: &mut Vec<LogicalLine>) -> Result<()> {
    let Some(first) = statements.first() else {
        return Ok(());
    };
    let words: Vec<&str> = first.text.split_ascii_whitespace().collect();
    let declared = match words.as_slice() {
        ["execute", version] if first.indent == 0 => version.parse::<u32>().ok(),
        _ => None,
    };
    let Some(declared) = declared else {
        return Ok(());
    };
    if declared != LANGUAGE_VERSION {
        bail!(
            "line {}: program declares language version {declared}, but this execute implements version {LANGUAGE_VERSION}; upgrade execute or remove the marker to use version {LANGUAGE_VERSION}",
            first.number
        );
    }
    statements.remove(0);
    Ok(())
}

/// Program-level cleanup blocks. `on exit` runs on every exit path; `on interrupt` runs
/// when a first interrupt starts a drain. Both wrap the statements in try/finally.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandlerKind {
    Exit,
    Interrupt,
}

#[derive(Clone, Copy)]
struct HandlerBlock {
    kind: HandlerKind,
    /// Cursor index of the `on exit` / `on interrupt` header line.
    header: usize,
    number: usize,
    /// Cursor index just after the indented body.
    body_end: usize,
}

/// Step indices of the opening `try` for each handler, pushed before the statements.
struct HandlerOpeners {
    exit: Option<usize>,
    interrupt: Option<usize>,
    count: usize,
}

impl HandlerOpeners {
    fn push(steps: &mut Vec<Step>, blocks: &[HandlerBlock]) -> Self {
        let mut openers = Self {
            exit: None,
            interrupt: None,
            count: 0,
        };
        // The exit cleanup encloses the interrupt cleanup, so it opens first.
        for kind in [HandlerKind::Exit, HandlerKind::Interrupt] {
            let Some(block) = blocks.iter().find(|block| block.kind == kind) else {
                continue;
            };
            let index = steps.len();
            steps.push(Step {
                line: block.number,
                kind: StepKind::Try {
                    catch: None,
                    finally: None,
                    end: 0,
                },
            });
            match kind {
                HandlerKind::Exit => openers.exit = Some(index),
                HandlerKind::Interrupt => openers.interrupt = Some(index),
            }
            openers.count += 1;
        }
        openers
    }
}

fn program_handlers(lines: &[SourceLine], arena: &[Token<'_>]) -> Result<Vec<HandlerBlock>> {
    let mut blocks: Vec<HandlerBlock> = Vec::new();
    let mut cursor = 0;
    while cursor < lines.len() {
        let line = &lines[cursor];
        let kind = if line.indent == 0 {
            match line.tokens(arena) {
                [Token::Word("on"), Token::Word("exit")] => Some(HandlerKind::Exit),
                [Token::Word("on"), Token::Word("interrupt")] => Some(HandlerKind::Interrupt),
                _ => None,
            }
        } else {
            None
        };
        let Some(kind) = kind else {
            cursor += 1;
            continue;
        };
        let label = match kind {
            HandlerKind::Exit => "on exit",
            HandlerKind::Interrupt => "on interrupt",
        };
        if blocks.iter().any(|block| block.kind == kind) {
            bail!("line {}: duplicate {label} block", line.number);
        }
        let mut body_end = cursor + 1;
        while body_end < lines.len() && lines[body_end].indent != 0 {
            body_end += 1;
        }
        if body_end == cursor + 1 {
            bail!(
                "line {}: {label} body must contain an indented statement",
                line.number
            );
        }
        blocks.push(HandlerBlock {
            kind,
            header: cursor,
            number: line.number,
            body_end,
        });
        cursor = body_end;
    }
    Ok(blocks)
}

/// Appends each handler after the statements: interrupt cleanup first (it sits inside the
/// exit cleanup), then the exit cleanup. Indices are relative to each opening `try`.
fn emit_handler_blocks<'arena, 'source: 'arena>(
    line_at: &impl Fn(usize) -> Option<BlockLine<'arena, 'source>>,
    blocks: &[HandlerBlock],
    openers: &HandlerOpeners,
    steps: &mut Vec<Step>,
) -> Result<()> {
    for kind in [HandlerKind::Interrupt, HandlerKind::Exit] {
        let Some(block) = blocks.iter().find(|block| block.kind == kind) else {
            continue;
        };
        let opener = match kind {
            HandlerKind::Exit => openers.exit,
            HandlerKind::Interrupt => openers.interrupt,
        };
        let Some(opener) = opener else {
            continue;
        };
        let finally = steps.len();
        steps.push(Step {
            line: block.number,
            kind: StepKind::Finally,
        });
        let guard = (kind == HandlerKind::Interrupt).then_some(steps.len());
        if guard.is_some() {
            steps.push(Step {
                line: block.number,
                kind: StepKind::OnInterrupt { end: 0 },
            });
        }
        let mut cursor = block.header + 1;
        let indent = line_at(cursor).map_or(0, |line| line.indent);
        while cursor < block.body_end {
            parse_statement(line_at, &mut cursor, indent, steps, 0)?;
        }
        if let Some(guard) = guard {
            let target = steps.len();
            if let StepKind::OnInterrupt { end } = &mut steps[guard].kind {
                *end = target - guard;
            }
        }
        steps.push(Step {
            line: block.number,
            kind: StepKind::EndTry,
        });
        let end = steps.len();
        if let StepKind::Try {
            finally: finally_slot,
            end: end_slot,
            ..
        } = &mut steps[opener].kind
        {
            *finally_slot = Some(finally - opener);
            *end_slot = end - opener;
        }
    }
    Ok(())
}

/// Parse the line-oriented workflow language into its AST.
pub fn parse(source: &str) -> Result<Program> {
    let mut statements = logical_lines(source)?;
    take_language_version(&mut statements)?;
    let mut definitions = BTreeMap::new();
    let mut inputs = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
    let mut token_arena = Vec::new();
    let mut lines = Vec::new();
    for statement in &statements {
        let token_start = token_arena.len();
        tokenize_into(&statement.text, &mut token_arena)
            .with_context(|| format!("line {}", statement.number))?;
        let token_range = token_start..token_arena.len();
        if token_range.is_empty() {
            continue;
        }
        lines.push(SourceLine {
            number: statement.number,
            indent: statement.indent,
            token_range,
        });
    }

    let block_line = |index| {
        lines.get(index).map(|line: &SourceLine| BlockLine {
            number: line.number,
            indent: line.indent,
            tokens: line.tokens(&token_arena),
        })
    };
    let handlers = program_handlers(&lines, &token_arena)?;
    if let Some(line) = lines.iter().find(|line| {
        line.indent == 0 && !handlers.is_empty() && is_trigger_header(line.tokens(&token_arena))
    }) {
        bail!(
            "line {}: on exit and on interrupt apply to one-shot programs, not triggers",
            line.number
        );
    }
    let openers = HandlerOpeners::push(&mut steps, &handlers);
    let mut cursor = 0;
    while cursor < lines.len() {
        if let Some(block) = handlers.iter().find(|block| block.header == cursor) {
            cursor = block.body_end;
            continue;
        }
        let line = &lines[cursor];
        let line_tokens = line.tokens(&token_arena);
        if line.indent != 0 {
            bail!(
                "line {}: unexpected indentation outside a trigger body",
                line.number
            );
        }
        if word_is(line_tokens.first(), "define") {
            if steps.len() > openers.count {
                bail!(
                    "line {}: program definitions must precede executable statements",
                    line.number
                );
            }
            let (name, definition) = reusable::parse_definition(&block_line, &mut cursor)?;
            if definitions.insert(name.clone(), definition).is_some() {
                bail!("line {}: duplicate program {name:?}", line.number);
            }
            continue;
        }
        if word_is(line_tokens.first(), "accept") {
            if steps.len() > openers.count {
                bail!(
                    "line {}: input declarations must precede executable statements",
                    line.number
                );
            }
            inputs.push(
                parse_input(line_tokens, line.number)
                    .map_err(|error| anyhow!("line {}: {error:#}", line.number))?,
            );
            cursor += 1;
            continue;
        }
        if is_trigger_header(line_tokens) {
            if steps.iter().any(|step| !is_trigger_setup(&step.kind)) {
                bail!(
                    "line {}: a trigger must follow only initial folder setup",
                    line.number
                );
            }
            let (trigger, body_start) = parse_trigger_header(&lines, &token_arena, cursor)?;
            let first_body = lines.get(body_start).ok_or_else(|| {
                anyhow!(
                    "line {}: trigger body must contain at least one indented statement",
                    line.number
                )
            })?;
            if first_body.indent == 0 {
                bail!("line {}: trigger body must be indented", first_body.number);
            }
            let body_indent = first_body.indent;
            cursor = body_start;
            let mut body_steps = Vec::new();
            while let Some(body_line) = lines.get(cursor) {
                if body_line.indent == 0 {
                    break;
                }
                let body_tokens = body_line.tokens(&token_arena);
                if is_trigger_header(body_tokens) || is_listener_event_header(body_tokens) {
                    bail!(
                        "line {}: nested trigger blocks are unsupported",
                        body_line.number
                    );
                }
                parse_statement(&block_line, &mut cursor, body_indent, &mut body_steps, 0)?;
            }
            if body_steps.is_empty() {
                bail!(
                    "line {}: trigger body must contain at least one indented statement",
                    first_body.number
                );
            }
            steps.extend(body_steps);
            if let Some(extra) = lines.get(cursor) {
                bail!(
                    "line {}: statements after the trigger body are unsupported",
                    extra.number
                );
            }
            crate::inputs::validate(&inputs)?;
            return reusable::finish(inputs, steps, Some(trigger), definitions);
        }
        if is_listener_event_header(line_tokens) {
            bail!(
                "line {}: listener event header must follow `listen to ...`",
                line.number
            );
        }
        parse_statement(&block_line, &mut cursor, 0, &mut steps, 0)?;
    }
    if let Some(first) = handlers.first() {
        if steps.len() == openers.count {
            bail!(
                "line {}: on exit and on interrupt need at least one statement to wrap",
                first.number
            );
        }
        emit_handler_blocks(&block_line, &handlers, &openers, &mut steps)?;
    }

    crate::inputs::validate(&inputs)?;
    reusable::finish(inputs, steps, None, definitions)
}

fn parse_input(tokens: &[Token<'_>], line: usize) -> Result<InputDeclaration> {
    let name = parse_binding_identifier(tokens.get(1), "input name")?;
    if !word_is(tokens.get(2), "as") {
        bail!("expected `accept <name> as <text|integer|boolean|list of text|json>`");
    }
    let (kind, mut cursor) = match tokens.get(3) {
        Some(Token::Word("text")) => (InputType::Text, 4),
        Some(Token::Word("integer")) => (InputType::Integer, 4),
        Some(Token::Word("boolean")) => (InputType::Boolean, 4),
        Some(Token::Word("json")) => (InputType::Json, 4),
        Some(Token::Word("list"))
            if word_is(tokens.get(4), "of") && word_is(tokens.get(5), "text") =>
        {
            (InputType::TextList, 6)
        }
        _ => bail!("input type must be text, integer, boolean, list of text, or json"),
    };
    let choices = if word_is(tokens.get(cursor), "one") && word_is(tokens.get(cursor + 1), "of") {
        if kind != InputType::Text {
            bail!("`one of` applies only to text inputs");
        }
        cursor += 2;
        Some(parse_choices(tokens, &mut cursor)?)
    } else {
        None
    };
    let source = if word_is(tokens.get(cursor), "from") {
        cursor += 1;
        let source = match tokens.get(cursor) {
            Some(Token::Word("environment")) => {
                cursor += 1;
                let value = parse_value(tokens, &mut cursor).context("environment name")?;
                let Value::String(name) = value else {
                    bail!("environment name must be a quoted literal");
                };
                InputSource::Environment(name)
            }
            Some(Token::Word("argument")) => {
                cursor += 1;
                let value = parse_value(tokens, &mut cursor).context("argument position")?;
                let Value::Integer(position) = value else {
                    bail!("argument position must be a positive integer");
                };
                let position = usize::try_from(position)
                    .ok()
                    .filter(|position| (1..=1_000_000).contains(position))
                    .context("argument position must be between 1 and 1000000")?;
                InputSource::Argument(position)
            }
            Some(Token::Word("arguments")) => {
                cursor += 1;
                InputSource::Arguments
            }
            _ => bail!("input source must be environment, argument, or arguments"),
        };
        Some(source)
    } else {
        None
    };
    // `with default V` and `described as "TEXT"` may appear in either order, once each.
    let mut default = None;
    let mut description = None;
    while cursor < tokens.len() {
        if word_is(tokens.get(cursor), "with") && word_is(tokens.get(cursor + 1), "default") {
            if default.is_some() {
                bail!("input declares its default more than once");
            }
            cursor += 2;
            default = Some(parse_value(tokens, &mut cursor).context("input default")?);
        } else if word_is(tokens.get(cursor), "described") && word_is(tokens.get(cursor + 1), "as")
        {
            if description.is_some() {
                bail!("input declares its description more than once");
            }
            let Some(Token::String(text)) = tokens.get(cursor + 2) else {
                bail!("`described as` must be followed by a quoted description");
            };
            description = Some(text.clone().into_owned());
            cursor += 3;
        } else {
            break;
        }
    }
    ensure_end(tokens, cursor)?;
    if let (Some(choices), Some(Value::String(value))) = (&choices, &default) {
        if !choices.contains(value) {
            bail!(
                "input {name:?} default {value:?} is not one of the allowed values {}",
                quoted_list(choices)
            );
        }
    }
    Ok(InputDeclaration {
        name,
        kind,
        default,
        source,
        line,
        description,
        choices,
    })
}

/// Parse the literal list after `one of`: `["a", "b"]`. Values are non-empty, unique,
/// NUL-free text, and at most 256 of them.
fn parse_choices(tokens: &[Token<'_>], cursor: &mut usize) -> Result<Vec<String>> {
    let Value::List(items) = parse_value(tokens, cursor).context("`one of` choices")? else {
        bail!("`one of` must be followed by a list of quoted text, such as [\"light\", \"dark\"]");
    };
    if items.is_empty() {
        bail!("`one of` needs at least one choice");
    }
    if items.len() > 256 {
        bail!("`one of` allows at most 256 choices");
    }
    let mut choices: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(text) = item else {
            bail!("`one of` choices must be quoted text");
        };
        if text.is_empty() || text.contains('\0') {
            bail!("`one of` choices must be nonempty and contain no NUL bytes");
        }
        if choices.contains(&text) {
            bail!("`one of` lists the choice {text:?} more than once");
        }
        choices.push(text);
    }
    Ok(choices)
}

/// The allowed values, quoted and comma-separated, for an error message.
pub(crate) fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_trigger_setup(kind: &StepKind) -> bool {
    matches!(kind, StepKind::UseFolder { .. })
        || matches!(kind, StepKind::Invoke { call, connection: Connection::Start | Connection::Then } if call.operation == "cd")
}

fn is_trigger_header(tokens: &[Token<'_>]) -> bool {
    word_is(tokens.first(), "every")
        || (word_is(tokens.first(), "on") && word_is(tokens.get(1), "schedule"))
        || (word_is(tokens.first(), "listen") && word_is(tokens.get(1), "to"))
}

fn is_listener_event_header(tokens: &[Token<'_>]) -> bool {
    word_is(tokens.first(), "whenever")
}

fn parse_trigger_header<'source>(
    lines: &[SourceLine],
    token_arena: &[Token<'source>],
    index: usize,
) -> Result<(Trigger, usize)> {
    let header = &lines[index];
    let tokens = header.tokens(token_arena);
    if word_is(tokens.first(), "every") {
        if tokens.len() != 3 {
            bail!(
                "line {}: expected `every <positive integer> <milliseconds|seconds|minutes|hours>`",
                header.number
            );
        }
        let amount = match &tokens[1] {
            Token::Word(number) => number.parse::<u64>().map_err(|_| {
                anyhow!(
                    "line {}: interval must be a positive integer in range",
                    header.number
                )
            })?,
            _ => bail!(
                "line {}: interval must be a positive integer",
                header.number
            ),
        };
        if amount == 0 {
            bail!("line {}: interval must be greater than zero", header.number);
        }
        let unit = match &tokens[2] {
            Token::Word(unit) => *unit,
            _ => bail!("line {}: expected an interval unit", header.number),
        };
        let factor = match unit {
            "millisecond" | "milliseconds" => 1,
            "second" | "seconds" => 1_000,
            "minute" | "minutes" => 60_000,
            "hour" | "hours" => 3_600_000,
            _ => bail!("line {}: unsupported interval unit `{unit}`", header.number),
        };
        let interval_ms = amount
            .checked_mul(factor)
            .ok_or_else(|| anyhow!("line {}: interval is too large", header.number))?;
        return Ok((
            Trigger::Every {
                interval_ms,
                line: header.number,
            },
            index + 1,
        ));
    }

    if word_is(tokens.first(), "on") {
        if tokens.len() != 3 || !word_is(tokens.get(1), "schedule") {
            bail!(
                "line {}: expected `on schedule <quoted cron expression>`",
                header.number
            );
        }
        let Token::String(expression) = &tokens[2] else {
            bail!("line {}: cron expression must be quoted", header.number);
        };
        if expression.trim().is_empty() {
            bail!("line {}: cron expression cannot be empty", header.number);
        }
        return Ok((
            Trigger::Cron {
                expression: expression.clone().into_owned(),
                line: header.number,
            },
            index + 1,
        ));
    }

    let source = if tokens.len() == 3 && word_is(tokens.get(2), "stdin") {
        ListenSource::Stdin
    } else {
        if tokens.len() < 3 {
            bail!(
                "line {}: expected `listen to stdin` or `listen to <applet phrase>`",
                header.number
            );
        }
        let source_tokens = &tokens[2..];
        if word_is(source_tokens.first(), "run")
            || word_is(source_tokens.first(), "then")
            || (word_is(source_tokens.first(), "pipe") && word_is(source_tokens.get(1), "to"))
        {
            bail!(
                "line {}: listener source must be an applet phrase without a connection prefix",
                header.number
            );
        }
        let kind = parse_invocation(source_tokens)
            .map_err(|error| anyhow!("line {}: invalid listener source: {error}", header.number))?;
        let StepKind::Invoke {
            call,
            connection: Connection::Start,
        } = kind
        else {
            bail!("line {}: listener source must be an applet", header.number);
        };
        if call.binding.is_some()
            || call.status_binding.is_some()
            || call.result_binding.is_some()
            || call.cache
        {
            bail!(
                "line {}: listener source cannot bind output/status/result or use `and cache`",
                header.number
            );
        }
        ListenSource::Applet(call)
    };

    let event_index = index + 1;
    let event = lines.get(event_index).ok_or_else(|| {
        anyhow!(
            "line {}: listener requires `whenever a line arrives as <binding>`",
            header.number
        )
    })?;
    let event_tokens = event.tokens(token_arena);
    if event.indent != 0 || !word_is(event_tokens.first(), "whenever") {
        bail!("line {}: expected unindented listener event header `whenever a line arrives as <binding>`", event.number);
    }
    if event_tokens.len() != 6
        || !word_is(event_tokens.get(1), "a")
        || !word_is(event_tokens.get(2), "line")
        || !word_is(event_tokens.get(3), "arrives")
        || !word_is(event_tokens.get(4), "as")
    {
        bail!(
            "line {}: expected `whenever a line arrives as <binding>`",
            event.number
        );
    }
    let binding = parse_binding_identifier(event_tokens.get(5), "listener binding")
        .map_err(|error| anyhow!("line {}: {error}", event.number))?;
    Ok((
        Trigger::Listen {
            source,
            binding,
            line: header.number,
        },
        event_index + 1,
    ))
}

pub(crate) fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quoted = false;
    let mut escaped = false;
    for (i, byte) in bytes.iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
        } else if *byte == b'"' {
            quoted = true;
        } else if *byte == b'#' {
            return &line[..i];
        }
    }
    line
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn tokenize<'source>(line: &'source str) -> Result<Vec<Token<'source>>> {
    let mut tokens = Vec::new();
    tokenize_into(line, &mut tokens)?;
    Ok(tokens)
}

fn tokenize_into<'source>(line: &'source str, tokens: &mut Vec<Token<'source>>) -> Result<()> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut object_depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' => i += 1,
            b'[' => {
                tokens.push(Token::LeftBracket);
                i += 1;
            }
            b']' => {
                tokens.push(Token::RightBracket);
                i += 1;
            }
            b',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            b'{' if object_depth > 0
                || i == 0
                || matches!(bytes[i - 1], b' ' | b'\t' | b'\r' | b'[' | b',') =>
            {
                object_depth += 1;
                tokens.push(Token::LeftBrace);
                i += 1;
            }
            b'}' if object_depth > 0 => {
                object_depth -= 1;
                tokens.push(Token::RightBrace);
                i += 1;
            }
            b':' if object_depth > 0 => {
                tokens.push(Token::Colon);
                i += 1;
            }
            b'"' => {
                let start = i;
                i += 1;
                let mut escaped = false;
                let mut closed = false;
                let mut plain = true;
                while i < bytes.len() {
                    let byte = bytes[i];
                    i += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                        plain = false;
                    } else if byte == b'"' {
                        closed = true;
                        break;
                    } else if byte < 0x20 {
                        plain = false;
                    }
                }
                if !closed {
                    bail!("unterminated quoted string");
                }
                if plain {
                    tokens.push(Token::String(Cow::Borrowed(&line[start + 1..i - 1])));
                } else {
                    let literal = &line[start..i];
                    let value = serde_json::from_str::<String>(literal)
                        .map_err(|_| anyhow!("malformed quoted string {literal}"))?;
                    tokens.push(Token::String(Cow::Owned(value)));
                }
            }
            _ => {
                let start = i;
                while i < bytes.len()
                    && !matches!(bytes[i], b' ' | b'\t' | b'\r' | b'[' | b']' | b',' | b'"')
                    && !(object_depth > 0 && matches!(bytes[i], b'{' | b'}' | b':'))
                {
                    // Every delimiter is ASCII. Advancing byte-by-byte is safe
                    // for UTF-8 here because a delimiter cannot occur inside a
                    // multibyte character, and we only slice at the delimiter.
                    i += 1;
                }
                tokens.push(Token::Word(&line[start..i]));
            }
        }
    }
    Ok(())
}

pub(crate) fn parse_line(tokens: &[Token<'_>]) -> Result<StepKind> {
    if matches!(tokens.first(), Some(Token::Word("run" | "then")))
        && word_is(tokens.get(1), "program")
    {
        let StepKind::Invoke {
            mut call,
            connection,
        } = parse_invocation(tokens)?
        else {
            unreachable!()
        };
        let name = call
            .operation
            .strip_prefix("program ")
            .context("expected `run program <name>`")?;
        if !is_identifier(name) {
            bail!("program name must be an identifier");
        }
        call.operation = name.to_owned();
        if call.cache || call.status_binding.is_some() || call.result_binding.is_some() {
            bail!("program calls cannot cache or capture native status or execution results");
        }
        return Ok(StepKind::CallProgram {
            call,
            connection,
            inputs: Vec::new(),
            end: 0,
        });
    }
    if word_is(tokens.first(), "take") {
        return parse_take_json(tokens);
    }
    if word_is(tokens.first(), "then") && word_is(tokens.get(1), "parse") {
        if tokens.len() != 5 || !word_is(tokens.get(2), "json") || !word_is(tokens.get(3), "as") {
            bail!("expected `then parse json as <name>`");
        }
        return Ok(StepKind::ParseJson {
            binding: parse_binding_identifier(tokens.get(4), "JSON binding")?,
        });
    }
    if word_is(tokens.first(), "then") && word_is(tokens.get(1), "encode") {
        let mut cursor = 2;
        let source = parse_value(tokens, &mut cursor)?;
        if !word_is(tokens.get(cursor), "as") || !word_is(tokens.get(cursor + 1), "json") {
            bail!("expected `then encode <value> as json`");
        }
        ensure_end(tokens, cursor + 2)?;
        return Ok(StepKind::EncodeJson { source });
    }
    if word_is(tokens.first(), "accept") {
        bail!("input declarations must be unindented and precede executable statements");
    }
    if word_is(tokens.first(), "use") {
        return parse_use(tokens);
    }
    if word_is(tokens.first(), "read") {
        return parse_read(tokens);
    }
    if word_is(tokens.first(), "then") && word_is(tokens.get(1), "save") {
        return parse_save(tokens);
    }
    if word_is(tokens.first(), "then") && word_is(tokens.get(1), "append") {
        return parse_append(tokens);
    }
    if tokens.first().is_some_and(|token| {
        matches!(token, Token::Word(word) if matches!(*word, "repeat" | "for" | "while" | "if" | "otherwise" | "else" | "loop"))
    }) {
        bail!("unsupported control block");
    }
    parse_invocation(tokens)
}

/// Parse one typed literal/reference using the same grammar as `.program` values.
/// Graph and other source surfaces reuse this to avoid divergent JSON coercions.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn parse_value_source(source: &str) -> Result<Value> {
    let mut tokens = Vec::new();
    tokenize_into(source, &mut tokens)?;
    let mut cursor = 0;
    let value = parse_value(&tokens, &mut cursor)?;
    ensure_end(&tokens, cursor)?;
    Ok(value)
}

fn parse_take_json(tokens: &[Token<'_>]) -> Result<StepKind> {
    let (kind, mut cursor) = match tokens.get(1) {
        Some(Token::Word("text")) => (InputType::Text, 2),
        Some(Token::Word("integer")) => (InputType::Integer, 2),
        Some(Token::Word("boolean")) => (InputType::Boolean, 2),
        Some(Token::Word("json")) => (InputType::Json, 2),
        Some(Token::Word("list")) if word_is(tokens.get(2), "of") && word_is(tokens.get(3), "text") => (InputType::TextList, 4),
        _ => bail!("expected `take <text|integer|boolean|list of text|json> at <quoted pointer> from <value> as <name>`"),
    };
    if !word_is(tokens.get(cursor), "at") {
        bail!("expected at before JSON pointer");
    }
    let (pointer, dynamic) = match tokens.get(cursor + 1) {
        Some(Token::String(pointer)) => {
            crate::structured::validate_pointer(pointer)?;
            (Value::String(pointer.clone().into_owned()), false)
        }
        Some(Token::Word(pointer)) if is_identifier(pointer) => {
            (Value::Reference((*pointer).to_owned()), true)
        }
        _ => bail!("JSON pointer must be a quoted literal or a text binding name"),
    };
    cursor += 2;
    if !word_is(tokens.get(cursor), "from") {
        bail!("expected from before JSON source");
    }
    cursor += 1;
    let source = parse_value(tokens, &mut cursor)?;
    if !word_is(tokens.get(cursor), "as") {
        bail!("expected as before JSON binding");
    }
    let binding = parse_binding_identifier(tokens.get(cursor + 1), "JSON binding")?;
    ensure_end(tokens, cursor + 2)?;
    if dynamic {
        Ok(StepKind::TakeJsonDynamic {
            pointer,
            source,
            kind,
            binding,
        })
    } else {
        let Value::String(pointer) = pointer else {
            unreachable!("literal pointer is text")
        };
        Ok(StepKind::TakeJson {
            pointer,
            source,
            kind,
            binding,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn is_input_header(line: &str) -> bool {
    line.split_ascii_whitespace().next() == Some("accept")
}

fn parse_use(tokens: &[Token<'_>]) -> Result<StepKind> {
    if word_is(tokens.get(1), "value") {
        if tokens.len() == 2 {
            bail!("expected `use value <value>`");
        }
        let mut cursor = 2;
        let source = parse_value(tokens, &mut cursor)?;
        ensure_end(tokens, cursor)?;
        return Ok(StepKind::UseValue { source });
    }
    if !word_is(tokens.get(1), "folder") {
        bail!("expected `use folder <value>` or `use value <value>`");
    }
    let mut cursor = 2;
    let path = parse_value(tokens, &mut cursor)?;
    ensure_end(tokens, cursor)?;
    Ok(StepKind::UseFolder { path })
}

fn parse_read(tokens: &[Token<'_>]) -> Result<StepKind> {
    let from_stdin = word_is(tokens.get(1), "from") && word_is(tokens.get(2), "stdin");
    let mut cursor = if from_stdin { 3 } else { 1 };
    if !from_stdin && word_is(tokens.get(cursor), "file") {
        cursor += 1;
    }
    let path = if from_stdin {
        None
    } else {
        Some(parse_value(tokens, &mut cursor)?)
    };
    let binding = if word_is(tokens.get(cursor), "as") {
        cursor += 1;
        Some(parse_binding_identifier(
            tokens.get(cursor),
            "binding name",
        )?)
    } else {
        None
    };
    if binding.is_some() {
        cursor += 1;
    }
    ensure_end(tokens, cursor)?;
    Ok(match path {
        Some(path) => StepKind::Read { path, binding },
        None => StepKind::ReadStdin { binding },
    })
}

fn parse_append(tokens: &[Token<'_>]) -> Result<StepKind> {
    let mut cursor = 2;
    if !word_is(tokens.get(cursor), "to") {
        bail!("expected `then append to <value>`");
    }
    cursor += 1;
    let path = parse_value(tokens, &mut cursor)?;
    ensure_end(tokens, cursor)?;
    Ok(StepKind::Append { path })
}

/// One trailing redirection suffix on a native command.
enum RedirectSuffix<'source> {
    Stdin(Token<'source>),
    Output {
        target: RedirectTarget,
        path: Option<Token<'source>>,
    },
}

/// Recognize `and stdin from V`, `and stderr to V`, `and stderr appended to V`,
/// `and stderr merged`, or `and stdout to stderr` at `index`. Returns the suffix
/// and how many tokens it spans.
fn redirect_suffix<'source>(
    tokens: &[Token<'source>],
    index: usize,
) -> Option<(RedirectSuffix<'source>, usize)> {
    if !word_is(tokens.get(index), "and") {
        return None;
    }
    let at = |offset: usize| tokens.get(index + offset);
    if word_is(at(1), "stdin") && word_is(at(2), "from") {
        let path = at(3)?.clone();
        return Some((RedirectSuffix::Stdin(path), 4));
    }
    if word_is(at(1), "stderr") && word_is(at(2), "to") {
        let path = at(3)?.clone();
        return Some((
            RedirectSuffix::Output {
                target: RedirectTarget::StderrFile,
                path: Some(path),
            },
            4,
        ));
    }
    if word_is(at(1), "stderr") && word_is(at(2), "appended") && word_is(at(3), "to") {
        let path = at(4)?.clone();
        return Some((
            RedirectSuffix::Output {
                target: RedirectTarget::StderrAppend,
                path: Some(path),
            },
            5,
        ));
    }
    if word_is(at(1), "stderr") && word_is(at(2), "merged") {
        return Some((
            RedirectSuffix::Output {
                target: RedirectTarget::StderrMerged,
                path: None,
            },
            3,
        ));
    }
    if word_is(at(1), "stdout") && word_is(at(2), "to") && word_is(at(3), "stderr") {
        return Some((
            RedirectSuffix::Output {
                target: RedirectTarget::StdoutToStderr,
                path: None,
            },
            4,
        ));
    }
    None
}

/// A redirection path is exactly one literal or binding token.
fn single_value(token: &Token<'_>) -> Result<Value> {
    let tokens = std::slice::from_ref(token);
    let mut cursor = 0;
    let value = parse_value(tokens, &mut cursor)?;
    ensure_end(tokens, cursor)?;
    Ok(value)
}

/// Parse one ordinary statement into its steps. Redirection suffixes on a native
/// command become, in order, a file read for stdin, a `Redirect` step, and the
/// command. The command then stays the only `Invoke` for its line.
/// A per-command environment suffix: `and environment {...}` or
/// `and without environment [...]`.
enum EnvironmentSuffix {
    Set(BTreeMap<String, Value>),
    Unset(Vec<String>),
}

/// Environment variable names cannot be empty or contain `=` or NUL.
fn valid_environment_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('=') && !name.contains('\0')
}

/// Index of the token that closes the bracket opened at `open`.
fn matching_close(tokens: &[Token<'_>], open: usize) -> Option<usize> {
    let (opener, closer) = match tokens.get(open)? {
        Token::LeftBrace => (Token::LeftBrace, Token::RightBrace),
        Token::LeftBracket => (Token::LeftBracket, Token::RightBracket),
        _ => return None,
    };
    let mut depth = 0usize;
    for (offset, token) in tokens[open..].iter().enumerate() {
        if *token == opener {
            depth += 1;
        } else if *token == closer {
            depth -= 1;
            if depth == 0 {
                return Some(open + offset);
            }
        }
    }
    None
}

/// Recognize an environment suffix at `index` and how many tokens it spans.
fn environment_suffix(
    tokens: &[Token<'_>],
    index: usize,
) -> Result<Option<(EnvironmentSuffix, usize)>> {
    if !word_is(tokens.get(index), "and") {
        return Ok(None);
    }
    let at = |offset: usize| tokens.get(index + offset);
    if word_is(at(1), "environment") && matches!(at(2), Some(Token::LeftBrace)) {
        let close = matching_close(tokens, index + 2)
            .context("environment record is missing its closing brace")?;
        let mut cursor = 0;
        let slice = &tokens[index + 2..=close];
        let Value::Record(fields) = parse_value(slice, &mut cursor)? else {
            bail!("`and environment` takes a record of names and values");
        };
        ensure_end(slice, cursor)?;
        let mut set = BTreeMap::new();
        for (name, value) in fields {
            if !valid_environment_name(&name) {
                bail!("invalid environment name {name:?}");
            }
            match &value {
                Value::String(_) | Value::Reference(_) => {}
                _ => bail!("environment value for {name:?} must be a text literal or text binding"),
            }
            set.insert(name, value);
        }
        return Ok(Some((EnvironmentSuffix::Set(set), close - index + 1)));
    }
    if word_is(at(1), "without")
        && word_is(at(2), "environment")
        && matches!(at(3), Some(Token::LeftBracket))
    {
        let close = matching_close(tokens, index + 3)
            .context("environment name list is missing its closing bracket")?;
        let mut cursor = 0;
        let slice = &tokens[index + 3..=close];
        let Value::List(items) = parse_value(slice, &mut cursor)? else {
            bail!("`and without environment` takes a list of names");
        };
        ensure_end(slice, cursor)?;
        let mut names = Vec::with_capacity(items.len());
        for item in items {
            let Value::String(name) = item else {
                bail!("`and without environment` names must be quoted literals");
            };
            if !valid_environment_name(&name) {
                bail!("invalid environment name {name:?}");
            }
            names.push(name);
        }
        return Ok(Some((EnvironmentSuffix::Unset(names), close - index + 1)));
    }
    Ok(None)
}

/// `run <command> ... interactively`: one fresh native command with a terminal
/// handoff. Redirection, stdin and environment suffixes are rejected here, so the
/// marker always sits directly before its command.
fn parse_interactive_run(tokens: &[Token<'_>]) -> Result<Vec<StepKind>> {
    if !word_is(tokens.first(), "run") || word_is(tokens.get(1), "program") {
        bail!("`interactively` applies to a single native `run` command");
    }
    for index in 0..tokens.len() {
        if redirect_suffix(tokens, index).is_some() || environment_suffix(tokens, index)?.is_some()
        {
            bail!(
                "`interactively` cannot be combined with redirection, stdin from, or environment"
            );
        }
    }
    let StepKind::Invoke { call, connection } = parse_invocation(tokens)? else {
        bail!("`interactively` applies to a single native `run` command");
    };
    if connection != Connection::Start {
        bail!("`interactively` applies to a fresh command, not `then` or `pipe to`");
    }
    Ok(vec![
        StepKind::Interactive,
        StepKind::Invoke { call, connection },
    ])
}

fn parse_simple_statement(tokens: &[Token<'_>]) -> Result<Vec<StepKind>> {
    // `interactively` is a trailing word on one top-level native `run`.
    if let Some(Token::Word("interactively")) = tokens.last() {
        return parse_interactive_run(&tokens[..tokens.len() - 1]);
    }
    let mut kept = Vec::with_capacity(tokens.len());
    let mut stdin = None;
    let mut output = None;
    let mut env_set: BTreeMap<String, Value> = BTreeMap::new();
    let mut env_unset: Vec<String> = Vec::new();
    let mut has_env = false;
    let mut index = 0;
    while index < tokens.len() {
        if let Some((suffix, span)) = environment_suffix(tokens, index)? {
            has_env = true;
            match suffix {
                EnvironmentSuffix::Set(fields) => {
                    for (name, value) in fields {
                        if env_set.insert(name.clone(), value).is_some() {
                            bail!("environment name {name:?} is set more than once");
                        }
                    }
                }
                EnvironmentSuffix::Unset(names) => env_unset.extend(names),
            }
            index += span;
            continue;
        }
        match redirect_suffix(tokens, index) {
            Some((RedirectSuffix::Stdin(path), span)) => {
                if stdin.replace(path).is_some() {
                    bail!("only one `and stdin from` redirection is allowed");
                }
                index += span;
            }
            Some((RedirectSuffix::Output { target, path }, span)) => {
                if output.replace((target, path)).is_some() {
                    bail!("only one stderr or stdout redirection is allowed per command");
                }
                index += span;
            }
            None => {
                kept.push(tokens[index].clone());
                index += 1;
            }
        }
    }
    if stdin.is_none() && output.is_none() && !has_env {
        return Ok(vec![parse_line(tokens)?]);
    }
    if let Some(name) = env_unset.iter().find(|name| env_set.contains_key(*name)) {
        bail!("environment name {name:?} is both set and removed");
    }
    let StepKind::Invoke { call, connection } = parse_invocation(&kept)? else {
        bail!("redirection and environment apply only to a native command");
    };
    let mut steps = Vec::with_capacity(4);
    let stdin_present = stdin.is_some();
    if let Some(path) = stdin {
        if connection != Connection::Start {
            bail!("`and stdin from` applies only to a command that starts a pipeline statement");
        }
        steps.push(StepKind::Read {
            path: single_value(&path)?,
            binding: None,
        });
    }
    if has_env {
        if connection == Connection::Pipe {
            bail!("`and environment` applies to a single command, not a pipe stage");
        }
        steps.push(StepKind::Environment {
            set: env_set,
            unset: env_unset,
        });
    }
    let consumer = if stdin_present {
        Connection::Then
    } else {
        connection
    };
    if let Some((target, path)) = output {
        if connection == Connection::Pipe {
            bail!("stderr and stdout redirection apply to a single command, not a pipe stage");
        }
        let path = path.as_ref().map(single_value).transpose()?;
        steps.push(StepKind::Redirect { target, path });
    }
    steps.push(StepKind::Invoke {
        call,
        connection: consumer,
    });
    Ok(steps)
}

fn parse_save(tokens: &[Token<'_>]) -> Result<StepKind> {
    let mut cursor = 2;
    if !word_is(tokens.get(cursor), "to") {
        bail!("expected `then save to <value>`");
    }
    cursor += 1;
    if word_is(tokens.get(cursor), "file") {
        cursor += 1;
    }
    let path = parse_value(tokens, &mut cursor)?;
    ensure_end(tokens, cursor)?;
    Ok(StepKind::Save {
        path,
        connection: Connection::Then,
    })
}

fn is_status_suffix(tokens: &[Token<'_>], cursor: usize) -> bool {
    word_is(tokens.get(cursor), "and")
        && word_is(tokens.get(cursor + 1), "keep")
        && word_is(tokens.get(cursor + 2), "status")
}

fn is_discard_stderr_suffix(tokens: &[Token<'_>], cursor: usize) -> bool {
    word_is(tokens.get(cursor), "and")
        && word_is(tokens.get(cursor + 1), "discard")
        && word_is(tokens.get(cursor + 2), "stderr")
}

fn is_result_suffix(tokens: &[Token<'_>], cursor: usize) -> bool {
    word_is(tokens.get(cursor), "and")
        && word_is(tokens.get(cursor + 1), "keep")
        && word_is(tokens.get(cursor + 2), "result")
}

fn parse_invocation(tokens: &[Token<'_>]) -> Result<StepKind> {
    let (mut cursor, connection) = if word_is(tokens.first(), "run") {
        (1, Connection::Start)
    } else if word_is(tokens.first(), "then") {
        (1, Connection::Then)
    } else if word_is(tokens.first(), "pipe") && word_is(tokens.get(1), "to") {
        (2, Connection::Pipe)
    } else {
        (0, Connection::Start)
    };

    let direct = matches!(tokens.get(cursor), Some(Token::String(_)));
    let operation = if let Some(Token::String(executable)) = tokens.get(cursor) {
        cursor += 1;
        if executable.is_empty() || executable.contains('\0') {
            bail!("executable name must be nonempty and cannot contain a NUL byte");
        }
        executable.clone().into_owned()
    } else {
        let operation_start = cursor;
        while cursor < tokens.len() {
            if cursor > operation_start
                && (word_is(tokens.get(cursor), "with") || word_is(tokens.get(cursor), "as"))
            {
                break;
            }
            if cursor > operation_start
                && word_is(tokens.get(cursor), "and")
                && ((word_is(tokens.get(cursor + 1), "cache") && cursor + 2 == tokens.len())
                    || (direct && is_discard_stderr_suffix(tokens, cursor))
                    || is_status_suffix(tokens, cursor)
                    || is_result_suffix(tokens, cursor))
            {
                break;
            }
            match tokens.get(cursor) {
                Some(Token::Word(_)) => cursor += 1,
                Some(Token::String(_)) => {
                    bail!("operation name must contain words, not quoted values")
                }
                _ => bail!("unexpected punctuation in operation name"),
            }
        }
        if cursor == operation_start {
            bail!("missing operation name");
        }
        let operation_tokens = &tokens[operation_start..cursor];
        let operation_capacity = operation_tokens
            .iter()
            .map(|token| match token {
                Token::Word(word) => word.len(),
                _ => unreachable!(),
            })
            .sum::<usize>()
            + operation_tokens.len()
            - 1;
        let mut operation = String::with_capacity(operation_capacity);
        for token in operation_tokens {
            if !operation.is_empty() {
                operation.push(' ');
            }
            let Token::Word(word) = token else {
                unreachable!();
            };
            operation.push_str(word);
        }
        operation
    };

    let mut parameters = BTreeMap::new();
    if word_is(tokens.get(cursor), "with") {
        cursor += 1;
        loop {
            let name = parse_identifier(tokens.get(cursor), "parameter name")?;
            cursor += 1;
            let value = parse_value(tokens, &mut cursor)
                .with_context(|| format!("missing or invalid value for parameter `{name}`"))?;
            match parameters.entry(name) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(value);
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    bail!("duplicate parameter `{}`", entry.key());
                }
            }
            let blocked = (word_is(tokens.get(cursor + 1), "cache") && cursor + 2 == tokens.len())
                || (direct && is_discard_stderr_suffix(tokens, cursor))
                || is_status_suffix(tokens, cursor)
                || is_result_suffix(tokens, cursor);
            if word_is(tokens.get(cursor), "and") && !blocked {
                cursor += 1;
                continue;
            }
            break;
        }
    }

    let binding = if word_is(tokens.get(cursor), "as") {
        cursor += 1;
        let name = parse_binding_identifier(tokens.get(cursor), "binding name")?;
        cursor += 1;
        Some(name)
    } else {
        None
    };
    let discard_stderr = if direct && is_discard_stderr_suffix(tokens, cursor) {
        cursor += 3;
        true
    } else {
        false
    };
    let status_binding = if is_status_suffix(tokens, cursor) {
        if !word_is(tokens.get(cursor + 2), "status") || !word_is(tokens.get(cursor + 3), "as") {
            bail!("expected `and keep status as <name>`");
        }
        cursor += 4;
        let name = parse_binding_identifier(tokens.get(cursor), "status binding name")?;
        cursor += 1;
        Some(name)
    } else {
        None
    };
    let result_binding = if is_result_suffix(tokens, cursor) {
        if !word_is(tokens.get(cursor + 2), "result") || !word_is(tokens.get(cursor + 3), "as") {
            bail!("expected `and keep result as <name>`");
        }
        cursor += 4;
        let name = parse_binding_identifier(tokens.get(cursor), "execution result binding name")?;
        cursor += 1;
        Some(name)
    } else {
        None
    };
    let cache = if word_is(tokens.get(cursor), "and")
        && word_is(tokens.get(cursor + 1), "cache")
        && cursor + 2 == tokens.len()
    {
        cursor += 2;
        true
    } else {
        false
    };
    if discard_stderr && cache {
        bail!("`and discard stderr` cannot be combined with `and cache`");
    }
    ensure_end(tokens, cursor)?;

    Ok(StepKind::Invoke {
        call: Invocation {
            operation,
            direct,
            discard_stderr,
            parameters,
            binding,
            cache,
            status_binding,
            result_binding,
        },
        connection,
    })
}

const MAX_VALUE_NESTING: usize = 64;

fn parse_value(tokens: &[Token<'_>], cursor: &mut usize) -> Result<Value> {
    parse_value_depth(tokens, cursor, 0)
}

fn parse_value_depth(tokens: &[Token<'_>], cursor: &mut usize, depth: usize) -> Result<Value> {
    let token = tokens
        .get(*cursor)
        .ok_or_else(|| anyhow!("missing value"))?;
    match token {
        Token::String(value) => {
            *cursor += 1;
            Ok(Value::String(value.clone().into_owned()))
        }
        Token::Word(word) if *word == "true" || *word == "false" => {
            *cursor += 1;
            Ok(Value::Bool(*word == "true"))
        }
        Token::Word("null") => {
            *cursor += 1;
            Ok(Value::Null)
        }
        Token::Word(word) => {
            *cursor += 1;
            if let Ok(integer) = word.parse::<i64>() {
                Ok(Value::Integer(integer))
            } else if word
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'-')
            {
                word.parse::<serde_json::Number>()
                    .map(Value::Number)
                    .map_err(|_| {
                        anyhow!(
                            "invalid JSON number `{word}`; quote strings containing punctuation"
                        )
                    })
            } else if is_identifier(word) {
                Ok(Value::Reference((*word).to_owned()))
            } else {
                bail!("invalid value `{word}`; quote strings containing spaces or punctuation")
            }
        }
        Token::LeftBracket => {
            if depth >= MAX_VALUE_NESTING {
                bail!("value nesting exceeds the maximum depth of {MAX_VALUE_NESTING}");
            }
            *cursor += 1;
            let mut values = Vec::new();
            if matches!(tokens.get(*cursor), Some(Token::RightBracket)) {
                *cursor += 1;
                return Ok(Value::List(values));
            }
            loop {
                values.push(parse_value_depth(tokens, cursor, depth + 1)?);
                match tokens.get(*cursor) {
                    Some(Token::Comma) => {
                        *cursor += 1;
                        if matches!(tokens.get(*cursor), Some(Token::RightBracket)) {
                            bail!("trailing comma in list");
                        }
                    }
                    Some(Token::RightBracket) => {
                        *cursor += 1;
                        break;
                    }
                    _ => bail!("expected `,` or `]` in list"),
                }
            }
            Ok(Value::List(values))
        }
        Token::LeftBrace => {
            if depth >= MAX_VALUE_NESTING {
                bail!("value nesting exceeds the maximum depth of {MAX_VALUE_NESTING}");
            }
            *cursor += 1;
            let mut fields = BTreeMap::new();
            if matches!(tokens.get(*cursor), Some(Token::RightBrace)) {
                *cursor += 1;
                return Ok(Value::Record(fields));
            }
            loop {
                let Some(Token::String(key)) = tokens.get(*cursor) else {
                    bail!("JSON object keys must be quoted strings");
                };
                let key = key.clone().into_owned();
                *cursor += 1;
                if !matches!(tokens.get(*cursor), Some(Token::Colon)) {
                    bail!("expected `:` after JSON object key");
                }
                *cursor += 1;
                let value = parse_value_depth(tokens, cursor, depth + 1)?;
                if fields.insert(key.clone(), value).is_some() {
                    bail!("duplicate JSON object key {key:?}");
                }
                match tokens.get(*cursor) {
                    Some(Token::Comma) => {
                        *cursor += 1;
                        if matches!(tokens.get(*cursor), Some(Token::RightBrace)) {
                            bail!("trailing comma in JSON object");
                        }
                    }
                    Some(Token::RightBrace) => {
                        *cursor += 1;
                        break;
                    }
                    _ => bail!("expected `,` or `}}` in JSON object"),
                }
            }
            Ok(Value::Record(fields))
        }
        Token::RightBracket => bail!("unexpected `]` while reading value"),
        Token::RightBrace => bail!("unexpected `}}` while reading value"),
        Token::Comma => bail!("unexpected `,` while reading value"),
        Token::Colon => bail!("unexpected `:` while reading value"),
    }
}

fn parse_identifier(token: Option<&Token<'_>>, description: &str) -> Result<String> {
    match token {
        Some(Token::Word(word)) if is_identifier(word) => Ok((*word).to_owned()),
        Some(_) => bail!("invalid {description}; expected an identifier"),
        None => bail!("missing {description}"),
    }
}

fn parse_binding_identifier(token: Option<&Token<'_>>, description: &str) -> Result<String> {
    let name = parse_identifier(token, description)?;
    if matches!(name.as_str(), "true" | "false") {
        bail!("reserved boolean `{name}` cannot be used as a {description}");
    }
    Ok(name)
}

pub(crate) fn is_identifier(word: &str) -> bool {
    let mut bytes = word.bytes();
    matches!(bytes.next(), Some(b'_' | b'A'..=b'Z' | b'a'..=b'z'))
        && bytes.all(|byte| matches!(byte, b'_' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'))
}

fn word_is(token: Option<&Token<'_>>, expected: &str) -> bool {
    matches!(token, Some(Token::Word(word)) if *word == expected)
}

fn ensure_end(tokens: &[Token<'_>], cursor: usize) -> Result<()> {
    if cursor == tokens.len() {
        Ok(())
    } else {
        bail!("stray token `{}`", token_text(&tokens[cursor]))
    }
}

fn token_text(token: &Token<'_>) -> String {
    match token {
        Token::Word(word) => (*word).to_owned(),
        Token::String(value) => format!("\"{value}\""),
        Token::LeftBracket => "[".into(),
        Token::RightBracket => "]".into(),
        Token::Comma => ",".into(),
        Token::LeftBrace => "{".into(),
        Token::RightBrace => "}".into(),
        Token::Colon => ":".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_readable_sample_and_keeps_source_lines() {
        let program = parse(
            "# input\nuse folder \"work\"\nread file \"notes.txt\" as notes\ncount lines with source notes and limit 10 as total and cache\nthen save to file \"summary.txt\" # output\n",
        )
        .unwrap();
        assert_eq!(program.steps.len(), 4);
        assert_eq!(program.steps[0].line, 2);
        assert_eq!(program.steps[1].line, 3);
        assert_eq!(program.steps[2].line, 4);
        assert_eq!(program.steps[3].line, 5);
        assert_eq!(program.trigger, None);
        assert!(matches!(&program.steps[2].kind,
            StepKind::Invoke { call, connection: Connection::Start }
                if call.operation == "count lines"
                    && call.binding.as_deref() == Some("total")
                    && call.cache
                    && call.parameters.get("limit") == Some(&Value::Integer(10))));
    }

    #[test]
    fn strings_keep_keywords_comments_and_escapes_as_literals() {
        let program = parse(
            r##"run printf with text "and discard stderr # as then \"quoted\"" as output # comment"##,
        )
        .unwrap();
        let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
            panic!()
        };
        assert_eq!(
            call.parameters.get("text"),
            Some(&Value::String(
                "and discard stderr # as then \"quoted\"".into()
            ))
        );
        assert_eq!(call.binding.as_deref(), Some("output"));
    }

    #[test]
    fn direct_stderr_discard_precedes_status_and_result_and_keeps_legacy_serde() {
        let program = parse(
            "run \"printf\" with arguments [\"x\"] as output and discard stderr and keep status as code and keep result as execution\n",
        )
        .unwrap();
        let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
            panic!();
        };
        assert!(call.direct);
        assert!(call.discard_stderr);
        assert_eq!(call.binding.as_deref(), Some("output"));
        assert_eq!(call.status_binding.as_deref(), Some("code"));
        assert_eq!(call.result_binding.as_deref(), Some("execution"));
        assert!(serde_json::to_string(&program)
            .unwrap()
            .contains("\"discard_stderr\":true"));

        let legacy = parse("run printf with arguments [\"x\"]\n").unwrap();
        let serialized = serde_json::to_value(&legacy).unwrap();
        assert!(!serialized.to_string().contains("discard_stderr"));
        let decoded: Program = serde_json::from_value(serialized).unwrap();
        assert_eq!(legacy, decoded);

        let comment =
            parse("run \"printf\" with arguments [\"x\"] # and discard stderr\n").unwrap();
        let StepKind::Invoke { call, .. } = &comment.steps[0].kind else {
            panic!();
        };
        assert!(!call.discard_stderr);
    }

    #[test]
    fn stderr_discard_is_restricted_to_supported_direct_calls() {
        for source in [
            "run \"printf\" with arguments [\"x\"] and discard stderr and cache\n",
            "spawn as child\n  run \"printf\" with arguments [\"x\"] and discard stderr\n",
        ] {
            let error = parse(source).unwrap_err();
            assert!(
                format!("{error:#}").contains("discard stderr")
                    || format!("{error:#}").contains("discard stdout"),
                "unexpected error for {source:?}: {error:#}"
            );
        }
        assert!(parse("run program child and discard stderr\n").is_err());
    }

    #[test]
    fn named_app_parameters_and_operations_keep_discard_words_as_literals() {
        let program = parse("run renderer with text label and discard \"stderr\"\n").unwrap();
        let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
            panic!();
        };
        assert!(!call.direct);
        assert!(!call.discard_stderr);
        assert_eq!(call.operation, "renderer");
        assert_eq!(
            call.parameters.get("discard"),
            Some(&Value::String("stderr".into()))
        );

        let program = parse("run renderer and discard stderr\n").unwrap();
        let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
            panic!();
        };
        assert!(!call.discard_stderr);
        assert_eq!(call.operation, "renderer and discard stderr");
    }

    #[test]
    fn tokenization_preserves_multibyte_words_and_quoted_values() {
        let program = parse("run café printer with label \"naïve 🐈\"").unwrap();
        let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
            panic!()
        };
        assert_eq!(call.operation, "café printer");
        assert_eq!(
            call.parameters.get("label"),
            Some(&Value::String("naïve 🐈".into()))
        );
    }

    #[test]
    fn tokenizer_words_borrow_the_original_utf8_source() {
        let source = String::from("café printer");
        let tokens = tokenize(&source).unwrap();
        let Token::Word(word) = &tokens[0] else {
            panic!()
        };
        assert_eq!(*word, "café");
        assert!(std::ptr::eq::<str>(*word, &source[.."café".len()]));
    }

    #[test]
    fn plain_unicode_json_string_tokens_borrow_and_keep_debug_format() {
        let source = String::from("\"snow 雪\"");
        let tokens = tokenize(&source).unwrap();
        let Token::String(Cow::Borrowed(value)) = &tokens[0] else {
            panic!("unescaped string should borrow from the input")
        };
        assert_eq!(*value, "snow 雪");
        assert!(std::ptr::eq::<str>(*value, &source[1..source.len() - 1]));
        assert_eq!(format!("{:?}", tokens[0]), "String(\"snow 雪\")");
        assert_eq!(
            format!("{:?}", Token::String(Cow::Owned(String::from("plain")))),
            "String(\"plain\")"
        );
    }

    #[test]
    fn escaped_json_strings_keep_serde_decoding_and_owned_values() {
        let literal = r#""line\nquote: \" slash: \\ solidus: \/ nul: \u0000 astral: \uD83D\uDE00""#;
        let tokens = tokenize(literal).unwrap();
        let Token::String(Cow::Owned(value)) = &tokens[0] else {
            panic!("escaped string should use the JSON decoder")
        };
        assert_eq!(
            value,
            "line\nquote: \" slash: \\ solidus: / nul: \0 astral: 😀"
        );
    }

    #[test]
    fn raw_json_controls_fall_back_to_the_same_malformed_string_error() {
        for control in 0..=0x1f {
            let control = char::from_u32(control).unwrap();
            let literal = format!("\"before{control}after\"");
            let error = tokenize(&literal).unwrap_err().to_string();
            assert_eq!(error, format!("malformed quoted string {literal}"));
        }
    }

    #[test]
    fn malformed_escape_keeps_later_lexical_error_precedence_and_line_number() {
        let error = parse("run\nrun printf with message \"bad\\q\"").unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "line 2: malformed quoted string \"bad\\q\""
        );
    }

    #[test]
    fn parsed_plain_string_values_are_owned_after_source_is_dropped() {
        let program = {
            let source = String::from("run printf with message \"plain 雪\"");
            parse(&source).unwrap()
        };
        let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
            panic!()
        };
        assert_eq!(
            call.parameters.get("message"),
            Some(&Value::String("plain 雪".into()))
        );
    }

    #[test]
    fn borrowed_word_errors_keep_the_same_token_text() {
        let error = parse("read file \"notes.txt\" naïve")
            .unwrap_err()
            .to_string();
        assert_eq!(error, "line 1: stray token `naïve`");
    }

    #[test]
    fn parses_lists_booleans_references_and_connections() {
        let program = parse(
            "run printf with values [1, true, name, \"two words\"] and enabled false\nthen count lines\npipe to filter with pattern \"x\"\n",
        )
        .unwrap();
        assert!(matches!(&program.steps[0].kind,
        StepKind::Invoke { call, connection: Connection::Start }
            if call.operation == "printf"
                && call.parameters.get("values") == Some(&Value::List(vec![
                    Value::Integer(1), Value::Bool(true), Value::Reference("name".into()), Value::String("two words".into())
                ]))));
        assert!(matches!(
            &program.steps[1].kind,
            StepKind::Invoke {
                connection: Connection::Then,
                ..
            }
        ));
        assert!(matches!(
            &program.steps[2].kind,
            StepKind::Invoke {
                connection: Connection::Pipe,
                ..
            }
        ));
    }

    #[test]
    fn allows_keyword_command_names_and_parameter_named_cache() {
        let as_command = parse("run as with path \"input.s\" as result").unwrap();
        assert!(matches!(
            &as_command.steps[0].kind,
            StepKind::Invoke { call, .. }
                if call.operation == "as"
                    && call.parameters.get("path") == Some(&Value::String("input.s".into()))
                    && call.binding.as_deref() == Some("result")
        ));

        for (value, expected) in [
            ("false", Value::Bool(false)),
            ("\"off\"", Value::String("off".into())),
            ("cached", Value::Reference("cached".into())),
        ] {
            let source = format!("run tool with mode \"x\" and cache {value}");
            let program = parse(&source).unwrap();
            let StepKind::Invoke { call, .. } = &program.steps[0].kind else {
                panic!()
            };
            assert_eq!(call.parameters.get("cache"), Some(&expected), "{source}");
            assert!(!call.cache, "{source}");
        }

        let modifier = parse("run tool with mode \"x\" and cache").unwrap();
        assert!(matches!(
            &modifier.steps[0].kind,
            StepKind::Invoke { call, .. }
                if call.cache && !call.parameters.contains_key("cache")
        ));
    }

    #[test]
    fn rejects_bad_syntax_with_line_number() {
        for source in [
            "run printf with value",
            "run printf with x [1,]",
            "read file \"unterminated",
            "run printf trailing [",
            "read file path as 3bad",
        ] {
            let error = parse(source).unwrap_err().to_string();
            assert!(error.contains("line 1"), "{source:?}: {error}");
        }
    }

    #[test]
    fn rejects_duplicate_parameters_and_empty_control_blocks() {
        let duplicate = parse("run printf with x 1 and x 2")
            .unwrap_err()
            .to_string();
        assert!(duplicate.contains("line 1") && duplicate.contains("duplicate parameter `x`"));
        let loop_error = parse("for each file in inputs").unwrap_err().to_string();
        assert_eq!(
            loop_error,
            "line 1: loop body must contain an indented statement"
        );
    }

    #[test]
    fn parses_every_and_cron_triggers_with_body_line_numbers() {
        let every =
            parse("use folder \"work\"\nevery 2 minutes\n  count lines\n  then summarize\n")
                .unwrap();
        assert_eq!(
            every.trigger,
            Some(Trigger::Every {
                interval_ms: 120_000,
                line: 2
            })
        );
        assert_eq!(
            every.steps.iter().map(|step| step.line).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );

        let cron = parse("on schedule \"*/5 * * * *\"\n    refresh\n").unwrap();
        assert_eq!(
            cron.trigger,
            Some(Trigger::Cron {
                expression: "*/5 * * * *".into(),
                line: 1
            })
        );
        assert_eq!(cron.steps[0].line, 2);
    }

    #[test]
    fn shared_token_arena_preserves_blank_and_comment_line_positions() {
        let program = parse(
            "# heading\n\nuse folder \"work\"\n\n# interval\nevery 2 minutes\n\n  count lines\n\n  then summarize\n\n# trailing comment\n",
        )
        .unwrap();
        assert_eq!(
            program.trigger,
            Some(Trigger::Every {
                interval_ms: 120_000,
                line: 6,
            })
        );
        assert_eq!(
            program
                .steps
                .iter()
                .map(|step| step.line)
                .collect::<Vec<_>>(),
            vec![3, 8, 10]
        );
    }

    #[test]
    fn parses_stdin_and_applet_listeners() {
        let stdin = parse("listen to stdin\nwhenever a line arrives as message\n  count words with text message\n").unwrap();
        assert_eq!(
            stdin.trigger,
            Some(Trigger::Listen {
                source: ListenSource::Stdin,
                binding: "message".into(),
                line: 1,
            })
        );
        assert_eq!(stdin.steps[0].line, 3);

        let applet = parse("listen to watch queue with topic \"jobs\" and limit 5\nwhenever a line arrives as item\n  process with input item\n").unwrap();
        let Some(Trigger::Listen {
            source: ListenSource::Applet(call),
            binding,
            line,
        }) = applet.trigger
        else {
            panic!()
        };
        assert_eq!(line, 1);
        assert_eq!(binding, "item");
        assert_eq!(call.operation, "watch queue");
        assert_eq!(call.parameters.get("limit"), Some(&Value::Integer(5)));

        let no_parameters = parse(
            "listen to kernel messages\nwhenever a line arrives as event\n  process with input event\n",
        )
        .unwrap();
        let Some(Trigger::Listen {
            source: ListenSource::Applet(call),
            binding,
            ..
        }) = no_parameters.trigger
        else {
            panic!()
        };
        assert_eq!(call.operation, "kernel messages");
        assert!(call.parameters.is_empty());
        assert_eq!(binding, "event");

        let one_word =
            parse("listen to pwd\nwhenever a line arrives as record\n  count lines\n").unwrap();
        assert!(matches!(
            one_word.trigger,
            Some(Trigger::Listen {
                source: ListenSource::Applet(Invocation { ref operation, ref parameters, .. }),
                ..
            }) if operation == "pwd" && parameters.is_empty()
        ));
    }

    #[test]
    fn keeps_builtin_cd_as_setup_before_listener_trigger() {
        let program = parse(
            "run cd with path \"./spool\"\nlisten to stdin\nwhenever a line arrives as message\n  process with input message\n",
        )
        .unwrap();
        assert_eq!(program.steps[0].line, 1);
        assert!(matches!(
            &program.steps[0].kind,
            StepKind::Invoke {
                call,
                connection: Connection::Start
            } if call.operation == "cd"
        ));
        assert_eq!(program.steps[1].line, 4);
        assert!(matches!(
            program.trigger,
            Some(Trigger::Listen { line: 2, .. })
        ));
    }

    #[test]
    fn rejects_invalid_trigger_blocks_and_durations() {
        for source in [
            "every 0 seconds\n  task",
            "every 18446744073709551615 hours\n  task",
            "every 1.5 seconds\n  task",
            "on schedule \"\"\n  task",
            "every 1 seconds\n",
            "every 1 seconds\n  task\n   nested",
            "every 1 seconds\n  task\nnext",
            "run first\nevery 1 seconds\n  task",
            "every 1 seconds\n  task\n  on schedule \"* * * * *\"\n    nested",
            "listen to watch queue with topic \"jobs\" and cache\nwhenever a line arrives as item\n  task",
            "listen to stdin\nwhenever a line arrives as 3bad\n  task",
        ] {
            let error = parse(source).unwrap_err().to_string();
            assert!(error.contains("line "), "{source:?}: {error}");
        }
    }

    #[test]
    fn rejects_boolean_words_as_result_bindings() {
        for source in [
            "run printf with message \"hello\" as true",
            "read file \"input.txt\" as false",
            "listen to stdin\nwhenever a line arrives as true\n  run printf with message \"ok\"",
        ] {
            let error = parse(source).unwrap_err().to_string();
            assert!(error.contains("reserved boolean"), "{source:?}: {error}");
        }
    }

    #[test]
    fn rejects_excessively_nested_lists_without_recursing_unboundedly() {
        let depth = MAX_VALUE_NESTING + 1;
        let source = format!(
            "run printf with message {}1{}",
            "[".repeat(depth),
            "]".repeat(depth)
        );
        let list = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
        let tokens = tokenize(&list).unwrap();
        let mut cursor = 0;
        let error = parse_value(&tokens, &mut cursor).unwrap_err().to_string();
        assert!(error.contains("maximum depth of 64"), "{error}");
        assert!(parse(&source).is_err());
    }
    #[test]
    fn chained_cd_setup_is_allowed_before_a_trigger() {
        let program = parse("run cd with path \"sub\"\nthen cd with path \"..\"\nlisten to stdin\nwhenever a line arrives as record\n  then cat\n").unwrap();
        assert!(matches!(
            program.steps[1].kind,
            StepKind::Invoke {
                connection: Connection::Then,
                ..
            }
        ));
        assert!(matches!(
            program.trigger,
            Some(Trigger::Listen { line: 3, .. })
        ));
    }

    #[test]
    fn loops_lower_to_relative_markers_without_unrolling() {
        let program = parse("repeat 3 times\n  for each path in [\"a.txt\", \"b.txt\"]\n    while ready is not false\n      run \"cat\" with arguments [path]\nthen save to \"report.txt\"\n").unwrap();
        assert_eq!(program.steps.len(), 8);
        assert!(matches!(
            &program.steps[0].kind,
            StepKind::Loop {
                kind: LoopKind::Repeat {
                    count: Value::Integer(3)
                },
                end: 7
            }
        ));
        assert!(matches!(&program.steps[1].kind, StepKind::Loop {
            kind: LoopKind::ForEach { binding, values: Value::List(values) }, end: 5
        } if binding == "path" && values.len() == 2));
        assert!(matches!(&program.steps[2].kind, StepKind::Loop {
            kind: LoopKind::While { condition: Condition {
                left: Value::Reference(left), right: Value::Bool(false), negated: true,
                operator: ConditionOperator::Equals,
            } }, end: 3
        } if left == "ready"));
        for (index, back) in [(4, 2), (5, 4), (6, 6)] {
            assert!(
                matches!(program.steps[index].kind, StepKind::EndLoop { back: actual } if actual == back)
            );
        }
        assert_eq!(
            program
                .steps
                .iter()
                .map(|step| step.line)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 3, 2, 1, 5]
        );
    }

    #[test]
    fn loop_headers_accept_input_references_and_empty_lists() {
        for (source, expected) in [
            (
                "repeat count times\n  run pwd",
                LoopKind::Repeat {
                    count: Value::Reference("count".into()),
                },
            ),
            (
                "for each item in files\n  run pwd",
                LoopKind::ForEach {
                    binding: "item".into(),
                    values: Value::Reference("files".into()),
                },
            ),
            (
                "for each item in []\n  run pwd",
                LoopKind::ForEach {
                    binding: "item".into(),
                    values: Value::List(vec![]),
                },
            ),
        ] {
            let program = parse(source).unwrap();
            assert!(
                matches!(&program.steps[0].kind, StepKind::Loop { kind, end: 3 } if kind == &expected)
            );
        }
    }

    #[test]
    fn loop_and_conditional_offsets_survive_reusable_expansion() {
        let program = parse("define a program named emit\n  run pwd\n  then cat\nrepeat 2 times\n  if true is true\n    run program emit\n  otherwise\n    for each item in []\n      run program emit\nthen cat\n").unwrap();
        let steps = &program.steps;
        assert!(matches!(steps[0].kind, StepKind::Loop { end: 15, .. }));
        assert!(matches!(
            steps[1].kind,
            StepKind::If {
                otherwise: 6,
                end: 13,
                ..
            }
        ));
        assert!(matches!(
            steps[2].kind,
            StepKind::CallProgram { end: 4, .. }
        ));
        assert!(matches!(steps[6].kind, StepKind::Otherwise { end: 8 }));
        assert!(matches!(steps[7].kind, StepKind::Loop { end: 6, .. }));
        assert!(matches!(steps[12].kind, StepKind::EndLoop { back: 5 }));
        assert!(matches!(steps[14].kind, StepKind::EndLoop { back: 14 }));
    }

    #[test]
    fn loops_inside_reusable_programs_rebase_relative_to_their_header() {
        let program = parse("define a program named emit\n  run pwd\n  then cat\ndefine a program named twice\n  repeat 2 times\n    run program emit\nrun program twice\n").unwrap();
        assert!(matches!(
            program.steps[0].kind,
            StepKind::CallProgram { end: 8, .. }
        ));
        assert!(matches!(
            program.steps[1].kind,
            StepKind::Loop { end: 6, .. }
        ));
        assert!(matches!(
            program.steps[6].kind,
            StepKind::EndLoop { back: 5 }
        ));
        assert!(matches!(program.steps[7].kind, StepKind::EndCall));
    }

    #[test]
    fn rejects_malformed_loop_headers_and_bodies_with_source_lines() {
        for source in [
            "repeat 3\n  run pwd",
            "repeat 3 times extra\n  run pwd",
            "repeat times\n  run pwd",
            "repeat 1 times",
            "repeat 1 times\nrun pwd",
            "for item in []\n  run pwd",
            "for each item []\n  run pwd",
            "for each true in []\n  run pwd",
            "for each item in [\"a\",]\n  run pwd",
            "for each item in [] extra\n  run pwd",
            "while ready\n  run pwd",
            "while ready is\n  run pwd",
            "while ready is true extra\n  run pwd",
            "repeat 1 times\n  run pwd\n   run pwd",
            "repeat 1 times\n  otherwise\n    run pwd",
            "repeat 1 times\n  every 1 second\n    run pwd",
        ] {
            let error = parse(source).unwrap_err().to_string();
            assert!(error.contains("line "), "{source:?}: {error}");
        }
    }

    #[test]
    fn mixed_loop_and_conditional_nesting_is_bounded() {
        let mut source = String::new();
        for depth in 0..=MAX_CONTROL_DEPTH {
            source.push_str(&"  ".repeat(depth));
            source.push_str(if depth % 2 == 0 {
                "repeat 1 times\n"
            } else {
                "if true is true\n"
            });
        }
        source.push_str(&"  ".repeat(MAX_CONTROL_DEPTH + 1));
        source.push_str("run pwd\n");
        let error = parse(&source).unwrap_err().to_string();
        assert!(error.contains("nesting exceeds 64 levels"), "{error}");

        // Exactly 64 nested controls remain accepted; the guard is not off by one.
        let mut accepted = String::new();
        for depth in 0..MAX_CONTROL_DEPTH {
            accepted.push_str(&"  ".repeat(depth));
            accepted.push_str(if depth % 2 == 0 {
                "repeat 1 times\n"
            } else {
                "if true is true\n"
            });
        }
        accepted.push_str(&"  ".repeat(MAX_CONTROL_DEPTH));
        accepted.push_str("run pwd\n");
        assert!(parse(&accepted).is_ok());
    }

    #[test]
    fn loops_parse_inside_scheduled_and_listener_handlers() {
        for source in [
            "every 1 second\n  repeat 2 times\n    run pwd",
            "listen to stdin\nwhenever a line arrives as message\n  for each item in [message]\n    while false is true\n      run pwd",
        ] {
            let program = parse(source).unwrap();
            assert!(program.trigger.is_some());
            assert!(matches!(program.steps[0].kind, StepKind::Loop { .. }));
            assert!(matches!(program.steps.last().unwrap().kind, StepKind::EndLoop { .. }));
        }
    }
}
