//! Strict YAML subset converted to and from JSON.
//!
//! Supported: block mappings and block sequences indented with spaces; plain,
//! single-quoted and double-quoted scalars; flow sequences and mappings written
//! on one line; literal (`|`) and folded (`>`) block scalars with clip, strip and
//! keep chomping; comments; and one leading `---`. A plain scalar resolves to
//! null (`null`, `~`, empty), a boolean (`true`/`false` in the common
//! capitalizations), an integer or a decimal float; everything else is a string.
//!
//! Anchors, aliases, tags, directives, multiple documents, complex keys, merge
//! keys, tabs in indentation, multi-line plain or quoted scalars and multi-line
//! flow collections fail explicitly. Duplicate mapping keys fail. Infinite and
//! NaN values fail because JSON cannot represent them. Output keys are sorted.
//!
//! Stringify writes two-space indentation. It quotes any string that would not
//! read back as the same plain text, using JSON string escapes, which the parser
//! accepts.

use super::config::{check_depth, count_value, insert_unique, integer_in_range, syntax};
use super::text::{check_text_size, text_input};
use super::{checkpoint_work, invalid, AppletError, ExecutionContext};
use serde_json::{Map, Value};

const FORMAT: &str = "yaml";

pub(super) fn parse(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    check_text_size(text.len())?;
    let text = std::str::from_utf8(crate::structured::strip_utf8_bom(text.as_bytes()))
        .map_err(|error| invalid(format!("YAML input is not UTF-8: {error}")))?;
    let lines = split_lines(text)?;
    let mut parser = Parser {
        lines,
        pos: 0,
        budget: Budget {
            context,
            ticks: 0,
            values: 0,
        },
    };
    parser.document()
}

struct Line {
    number: usize,
    indent: usize,
    /// Content after the indentation, with trailing whitespace removed.
    text: String,
    /// The original line, used by block scalars, which keep their own spacing.
    raw: String,
}

fn split_lines(text: &str) -> Result<Vec<Line>, AppletError> {
    let mut lines = Vec::new();
    for (index, raw_line) in text.split('\n').enumerate() {
        let raw = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let body = raw[indent..].trim_end();
        if body.starts_with('\t') {
            return Err(syntax(
                FORMAT,
                index + 1,
                "tabs cannot be used for indentation",
            ));
        }
        lines.push(Line {
            number: index + 1,
            indent,
            text: body.to_owned(),
            raw: raw.to_owned(),
        });
    }
    if text.ends_with('\n') {
        lines.pop();
    }
    Ok(lines)
}

#[derive(Clone, Copy)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

/// Shared per-parse limits: a count of values and a cooperative cancellation
/// checkpoint. Block lines, flow items and scalars all draw from it, so one long
/// flow collection on a single line is bounded and cancellable too.
struct Budget<'c, 'x> {
    context: &'c ExecutionContext<'x>,
    ticks: usize,
    values: usize,
}

impl Budget<'_, '_> {
    fn tick(&mut self) -> Result<(), AppletError> {
        checkpoint_work(self.context, self.ticks)?;
        self.ticks += 1;
        Ok(())
    }

    fn count(&mut self) -> Result<(), AppletError> {
        count_value(&mut self.values)
    }
}

struct Parser<'c, 'x> {
    lines: Vec<Line>,
    pos: usize,
    budget: Budget<'c, 'x>,
}

impl Parser<'_, '_> {
    fn tick(&mut self) -> Result<(), AppletError> {
        self.budget.tick()
    }

    /// Skip blank lines and comment-only lines between structural lines.
    fn skip_ignorable(&mut self) {
        while let Some(line) = self.lines.get(self.pos) {
            if !(line.text.is_empty() || line.text.starts_with('#')) {
                break;
            }
            self.pos += 1;
        }
    }

    fn document(&mut self) -> Result<Value, AppletError> {
        self.skip_ignorable();
        if self
            .lines
            .get(self.pos)
            .is_some_and(|line| line.text == "---")
        {
            self.pos += 1;
            self.skip_ignorable();
        }
        let Some(first) = self.lines.get(self.pos) else {
            return Ok(Value::Null);
        };
        if first.text == "..."
            || first.text.starts_with("--- ")
            || first.text == "---"
            || (first.indent == 0 && first.text.starts_with('%'))
        {
            return Err(syntax(
                FORMAT,
                first.number,
                "multiple documents and directives are not supported",
            ));
        }
        let indent = first.indent;
        let value = self.block(indent, 0)?;
        self.skip_ignorable();
        if let Some(line) = self.lines.get(self.pos) {
            return Err(syntax(
                FORMAT,
                line.number,
                "unexpected content after the document",
            ));
        }
        Ok(value)
    }

    /// Parse the structure that starts on the current line at `indent`.
    fn block(&mut self, indent: usize, depth: usize) -> Result<Value, AppletError> {
        self.tick()?;
        let line = &self.lines[self.pos];
        let number = line.number;
        let text = line.text.clone();
        check_depth(depth, FORMAT, number)?;
        if is_sequence_entry(&text) {
            return self.sequence(indent, depth);
        }
        if split_key(&text, number)?.is_some() {
            return self.mapping(indent, depth);
        }
        self.pos += 1;
        if text.starts_with('|') || text.starts_with('>') {
            return self.block_scalar(&text, number, None);
        }
        parse_inline(&text, number, depth, &mut self.budget)
    }

    fn mapping(&mut self, indent: usize, depth: usize) -> Result<Value, AppletError> {
        let mut map = Map::new();
        loop {
            self.tick()?;
            self.skip_ignorable();
            let Some(line) = self.lines.get(self.pos) else {
                break;
            };
            if line.indent < indent {
                break;
            }
            let number = line.number;
            if line.indent > indent {
                return Err(syntax(
                    FORMAT,
                    number,
                    "unexpected indentation inside a mapping",
                ));
            }
            let text = line.text.clone();
            if text == "---" || text == "..." || text.starts_with("--- ") {
                return Err(syntax(
                    FORMAT,
                    number,
                    "multiple documents are not supported",
                ));
            }
            if is_sequence_entry(&text) {
                return Err(syntax(
                    FORMAT,
                    number,
                    "a sequence entry cannot appear among mapping keys at the same indentation",
                ));
            }
            let Some((key, rest)) = split_key(&text, number)? else {
                return Err(syntax(FORMAT, number, "expected a key followed by ':'"));
            };
            self.pos += 1;
            let value = self.entry_value(&rest, indent, number, depth)?;
            self.budget.count()?;
            insert_unique(&mut map, key, value, FORMAT, number)?;
        }
        Ok(Value::Object(map))
    }

    /// The value after `key:` on the same line, or the nested structure below.
    fn entry_value(
        &mut self,
        rest: &str,
        parent: usize,
        number: usize,
        depth: usize,
    ) -> Result<Value, AppletError> {
        let rest = rest.trim();
        if rest.is_empty() || rest.starts_with('#') {
            self.skip_ignorable();
            let Some(next) = self.lines.get(self.pos) else {
                return Ok(Value::Null);
            };
            if next.indent > parent {
                let child = next.indent;
                return self.block(child, depth + 1);
            }
            if next.indent == parent && is_sequence_entry(&next.text) {
                return self.sequence(parent, depth + 1);
            }
            return Ok(Value::Null);
        }
        if rest.starts_with('|') || rest.starts_with('>') {
            return self.block_scalar(rest, number, Some(parent));
        }
        parse_inline(rest, number, depth, &mut self.budget)
    }

    fn sequence(&mut self, indent: usize, depth: usize) -> Result<Value, AppletError> {
        let mut items = Vec::new();
        loop {
            self.tick()?;
            self.skip_ignorable();
            let Some(line) = self.lines.get(self.pos) else {
                break;
            };
            if line.indent < indent {
                break;
            }
            let number = line.number;
            if line.indent > indent {
                return Err(syntax(
                    FORMAT,
                    number,
                    "unexpected indentation inside a sequence",
                ));
            }
            if !is_sequence_entry(&line.text) {
                break;
            }
            check_depth(depth, FORMAT, number)?;
            let text = line.text.clone();
            let after_dash = &text[1..];
            let content = after_dash.trim_start();
            let spaces = after_dash.len() - content.len();
            let content = content.to_owned();
            if content.is_empty() || content.starts_with('#') {
                self.pos += 1;
                self.skip_ignorable();
                let item = match self.lines.get(self.pos) {
                    Some(next) if next.indent > indent => {
                        let child = next.indent;
                        self.block(child, depth + 1)?
                    }
                    _ => Value::Null,
                };
                items.push(item);
            } else if content.starts_with('|') || content.starts_with('>') {
                self.pos += 1;
                items.push(self.block_scalar(&content, number, Some(indent))?);
            } else if is_sequence_entry(&content) || split_key(&content, number)?.is_some() {
                // A nested sequence or a compact mapping starts on this line.
                // Rewrite the line at the column where its content begins.
                let child = indent + 1 + spaces;
                self.lines[self.pos].indent = child;
                self.lines[self.pos].text = content;
                items.push(self.block(child, depth + 1)?);
            } else {
                self.pos += 1;
                items.push(parse_inline(&content, number, depth, &mut self.budget)?);
            }
            self.budget.count()?;
        }
        Ok(Value::Array(items))
    }

    /// Read a literal or folded block scalar. The parser is positioned on the
    /// first line after the header. `parent` is the indentation of the owning
    /// key or entry; `None` means a top-level scalar.
    fn block_scalar(
        &mut self,
        header: &str,
        number: usize,
        parent: Option<usize>,
    ) -> Result<Value, AppletError> {
        let literal = header.starts_with('|');
        let options = &header[1..];
        let (chomp, options) = match options.as_bytes().first() {
            Some(b'-') => (Chomp::Strip, &options[1..]),
            Some(b'+') => (Chomp::Keep, &options[1..]),
            _ => (Chomp::Clip, options),
        };
        let options = options.trim();
        if !(options.is_empty() || options.starts_with('#')) {
            return Err(syntax(
                FORMAT,
                number,
                "block scalar indentation indicators are not supported",
            ));
        }

        let mut content_indent: Option<usize> = None;
        let mut lines_out: Vec<String> = Vec::new();
        let mut pending_blank = 0usize;
        let mut trailing_blank = 0usize;
        while let Some(line) = self.lines.get(self.pos) {
            let stripped = line.raw.trim_start_matches(' ');
            let indent = line.raw.len() - stripped.len();
            if stripped.trim_end().is_empty() {
                pending_blank += 1;
                trailing_blank = pending_blank;
                self.pos += 1;
                continue;
            }
            let inside = match (content_indent, parent) {
                (Some(content), _) => indent >= content,
                (None, Some(parent)) => indent > parent,
                (None, None) => true,
            };
            if !inside {
                break;
            }
            let content = *content_indent.get_or_insert(indent);
            if !literal && indent > content {
                return Err(syntax(
                    FORMAT,
                    line.number,
                    "more-indented folded scalar lines are not supported",
                ));
            }
            for _ in 0..pending_blank {
                lines_out.push(String::new());
            }
            pending_blank = 0;
            trailing_blank = 0;
            lines_out.push(line.raw[content..].to_owned());
            self.pos += 1;
        }

        if lines_out.is_empty() {
            let newlines = match chomp {
                Chomp::Keep => trailing_blank.max(pending_blank),
                _ => 0,
            };
            return Ok(Value::String("\n".repeat(newlines)));
        }
        let body = if literal {
            lines_out.join("\n")
        } else {
            fold(&lines_out)
        };
        let text = match chomp {
            Chomp::Clip => format!("{body}\n"),
            Chomp::Strip => body,
            Chomp::Keep => format!("{body}\n{}", "\n".repeat(trailing_blank)),
        };
        Ok(Value::String(text))
    }
}

/// Folded scalars join adjacent text lines with a space. A run of blank lines
/// between text lines keeps one newline per blank line instead.
fn fold(lines: &[String]) -> String {
    let mut out = String::new();
    let mut pending = 0usize;
    let mut seen_text = false;
    for line in lines {
        if line.is_empty() {
            pending += 1;
            continue;
        }
        if seen_text && pending == 0 {
            out.push(' ');
        } else {
            out.push_str(&"\n".repeat(pending));
        }
        out.push_str(line);
        pending = 0;
        seen_text = true;
    }
    out
}

fn is_sequence_entry(text: &str) -> bool {
    text == "-" || text.starts_with("- ") || text.starts_with("-\t")
}

/// Split `key: rest` into the key (plain or quoted) and the text after the
/// colon. Returns `None` for text that is not a mapping entry.
fn split_key(text: &str, number: usize) -> Result<Option<(String, String)>, AppletError> {
    if text.starts_with('"') || text.starts_with('\'') {
        let chars: Vec<char> = text.chars().collect();
        let mut pos = 0;
        let key = quoted(&chars, &mut pos, number)?;
        let rest: String = chars[pos..].iter().collect();
        let after_key = rest.trim_start();
        return Ok(after_key.strip_prefix(':').and_then(|after| {
            (after.is_empty() || after.starts_with([' ', '\t'])).then(|| (key, after.to_owned()))
        }));
    }
    if text.starts_with(['[', '{', '#']) {
        return Ok(None);
    }
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b':' {
            continue;
        }
        if index + 1 != bytes.len() && !matches!(bytes[index + 1], b' ' | b'\t') {
            continue;
        }
        let key = text[..index].trim_end();
        if key.is_empty() || key.contains(" #") {
            return Ok(None);
        }
        if key.starts_with(['&', '*', '!', '%', '@', '`']) {
            return Err(syntax(
                FORMAT,
                number,
                "anchors, aliases, tags and directives are not supported",
            ));
        }
        return Ok(Some((key.to_owned(), text[index + 1..].to_owned())));
    }
    Ok(None)
}

/// Parse a scalar or one-line flow collection that is not a block structure.
fn parse_inline(
    text: &str,
    number: usize,
    depth: usize,
    budget: &mut Budget<'_, '_>,
) -> Result<Value, AppletError> {
    let text = text.trim();
    let chars: Vec<char> = text.chars().collect();
    match chars.first() {
        None => Ok(Value::Null),
        Some('"') | Some('\'') => {
            let mut pos = 0;
            let value = quoted(&chars, &mut pos, number)?;
            let rest: String = chars[pos..].iter().collect();
            expect_end(&rest, number)?;
            Ok(Value::String(value))
        }
        Some('[') | Some('{') => {
            let mut flow = Flow {
                chars: &chars,
                pos: 0,
                number,
                budget,
            };
            let value = flow.collection(depth)?;
            let rest: String = chars[flow.pos..].iter().collect();
            expect_end(&rest, number)?;
            Ok(value)
        }
        Some(first) if "&*!%@`".contains(*first) => Err(syntax(
            FORMAT,
            number,
            "anchors, aliases, tags and directives are not supported",
        )),
        Some(_) => {
            let plain = strip_comment(text);
            if is_sequence_entry(plain) {
                return Err(syntax(
                    FORMAT,
                    number,
                    "a sequence entry cannot start a value here",
                ));
            }
            if plain.contains(": ") || plain.ends_with(':') {
                return Err(syntax(
                    FORMAT,
                    number,
                    "mapping values are not allowed here",
                ));
            }
            resolve_plain(plain, number)
        }
    }
}

fn expect_end(rest: &str, number: usize) -> Result<(), AppletError> {
    let rest = rest.trim();
    if rest.is_empty() || rest.starts_with('#') {
        Ok(())
    } else {
        Err(syntax(FORMAT, number, "unexpected text after a scalar"))
    }
}

fn strip_comment(text: &str) -> &str {
    if text.starts_with('#') {
        return "";
    }
    let bytes = text.as_bytes();
    for index in 1..bytes.len() {
        if bytes[index] == b'#' && matches!(bytes[index - 1], b' ' | b'\t') {
            return text[..index].trim_end();
        }
    }
    text.trim_end()
}

/// Read a quoted scalar that starts at `pos`. Multi-line quoted scalars are
/// rejected because the line is the unit of this subset.
fn quoted(chars: &[char], pos: &mut usize, number: usize) -> Result<String, AppletError> {
    let unclosed = || {
        syntax(
            FORMAT,
            number,
            "quoted scalar is not closed on this line; multi-line scalars are not supported",
        )
    };
    let quote = *chars.get(*pos).ok_or_else(unclosed)?;
    *pos += 1;
    let mut out = String::new();
    loop {
        let character = *chars.get(*pos).ok_or_else(unclosed)?;
        *pos += 1;
        if quote == '\'' {
            if character != '\'' {
                out.push(character);
            } else if chars.get(*pos) == Some(&'\'') {
                out.push('\'');
                *pos += 1;
            } else {
                return Ok(out);
            }
        } else {
            match character {
                '"' => return Ok(out),
                '\\' => {
                    let escaped = *chars.get(*pos).ok_or_else(unclosed)?;
                    *pos += 1;
                    out.push(escape(escaped, chars, pos, number)?);
                }
                _ => out.push(character),
            }
        }
    }
}

fn escape(
    escaped: char,
    chars: &[char],
    pos: &mut usize,
    number: usize,
) -> Result<char, AppletError> {
    let character = match escaped {
        '0' => '\0',
        'a' => '\u{7}',
        'b' => '\u{8}',
        't' | '\t' => '\t',
        'n' => '\n',
        'v' => '\u{b}',
        'f' => '\u{c}',
        'r' => '\r',
        'e' => '\u{1b}',
        ' ' => ' ',
        '"' => '"',
        '/' => '/',
        '\\' => '\\',
        'N' => '\u{85}',
        '_' => '\u{a0}',
        'x' => return hex_escape(chars, pos, 2, number),
        'u' => return hex_escape(chars, pos, 4, number),
        'U' => return hex_escape(chars, pos, 8, number),
        _ => {
            return Err(syntax(
                FORMAT,
                number,
                format!("unsupported escape sequence \\{escaped}"),
            ))
        }
    };
    Ok(character)
}

fn hex_escape(
    chars: &[char],
    pos: &mut usize,
    digits: usize,
    number: usize,
) -> Result<char, AppletError> {
    let end = *pos + digits;
    let hex: String = chars
        .get(*pos..end)
        .ok_or_else(|| syntax(FORMAT, number, "escape sequence is truncated"))?
        .iter()
        .collect();
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(syntax(
            FORMAT,
            number,
            "escape sequence needs hexadecimal digits",
        ));
    }
    let code = u32::from_str_radix(&hex, 16)
        .map_err(|_| syntax(FORMAT, number, "invalid escape sequence"))?;
    *pos = end;
    char::from_u32(code)
        .ok_or_else(|| syntax(FORMAT, number, "escape is not a Unicode scalar value"))
}

/// Resolve an unquoted scalar using the YAML 1.2 core schema's common forms.
fn resolve_plain(text: &str, number: usize) -> Result<Value, AppletError> {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Ok(Value::Null),
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        ".inf" | "+.inf" | "-.inf" | ".Inf" | "+.Inf" | "-.Inf" | ".INF" | "+.INF" | "-.INF"
        | ".nan" | ".NaN" | ".NAN" => {
            return Err(syntax(
                FORMAT,
                number,
                "infinite and NaN values are not supported",
            ));
        }
        _ => {}
    }
    if is_integer(text) {
        let value: i128 = text
            .parse()
            .map_err(|_| syntax(FORMAT, number, "integer is outside the signed 64-bit range"))?;
        return Ok(Value::from(integer_in_range(value, FORMAT, number)?));
    }
    if is_float(text) {
        let value: f64 = text
            .parse()
            .map_err(|_| syntax(FORMAT, number, format!("invalid number {text:?}")))?;
        return serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| syntax(FORMAT, number, "number is not finite"));
    }
    Ok(Value::String(text.to_owned()))
}

fn is_integer(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_float(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    let all_digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    let mantissa_ok = match mantissa.find('.') {
        Some(index) => {
            let (before, after) = (&mantissa[..index], &mantissa[index + 1..]);
            if before.is_empty() {
                !after.is_empty() && all_digits(after)
            } else {
                all_digits(before) && all_digits(after)
            }
        }
        None => exponent.is_some() && !mantissa.is_empty() && all_digits(mantissa),
    };
    let exponent_ok = match exponent {
        None => true,
        Some(exponent) => {
            let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
            !digits.is_empty() && all_digits(digits)
        }
    };
    mantissa_ok && exponent_ok && (mantissa.contains('.') || exponent.is_some())
}

/// A one-line flow collection, such as `[a, "b"]` or `{key: value}`.
struct Flow<'a, 'c, 'x> {
    chars: &'a [char],
    pos: usize,
    number: usize,
    budget: &'a mut Budget<'c, 'x>,
}

impl Flow<'_, '_, '_> {
    fn fail(&self, message: &str) -> AppletError {
        syntax(FORMAT, self.number, message)
    }

    fn skip_spaces(&mut self) {
        while matches!(self.chars.get(self.pos), Some(' ' | '\t')) {
            self.pos += 1;
        }
    }

    fn collection(&mut self, depth: usize) -> Result<Value, AppletError> {
        check_depth(depth, FORMAT, self.number)?;
        let open = self.chars.get(self.pos).copied();
        self.pos += 1;
        if open == Some('[') {
            let mut items = Vec::new();
            loop {
                self.budget.tick()?;
                self.skip_spaces();
                match self.chars.get(self.pos) {
                    Some(']') => {
                        self.pos += 1;
                        break;
                    }
                    None => return Err(self.fail("flow sequence is not closed on this line")),
                    _ => {}
                }
                if matches!(self.chars.get(self.pos), Some(',')) {
                    return Err(self.fail("flow sequence entries cannot be empty"));
                }
                let quoted = matches!(self.chars.get(self.pos), Some('\'' | '"'));
                items.push(self.value(depth + 1)?);
                if !quoted
                    && items
                        .last()
                        .and_then(Value::as_str)
                        .is_some_and(|value| value.contains(": "))
                {
                    return Err(
                        self.fail("single-pair mappings in flow sequences are not supported")
                    );
                }
                self.skip_spaces();
                match self.chars.get(self.pos) {
                    Some(',') => {
                        self.pos += 1;
                        self.skip_spaces();
                        if matches!(self.chars.get(self.pos), Some(',')) {
                            return Err(self.fail("flow sequence entries cannot be empty"));
                        }
                    }
                    Some(']') => {
                        self.pos += 1;
                        break;
                    }
                    None => return Err(self.fail("flow sequence is not closed on this line")),
                    _ => return Err(self.fail("expected ',' or ']' in a flow sequence")),
                }
            }
            Ok(Value::Array(items))
        } else {
            let mut map = Map::new();
            loop {
                self.budget.tick()?;
                self.skip_spaces();
                match self.chars.get(self.pos) {
                    Some('}') => {
                        self.pos += 1;
                        break;
                    }
                    None => return Err(self.fail("flow mapping is not closed on this line")),
                    _ => {}
                }
                let key = self.key()?;
                self.skip_spaces();
                if self.chars.get(self.pos) != Some(&':') {
                    return Err(self.fail("expected ':' in a flow mapping"));
                }
                self.pos += 1;
                let value = self.value(depth + 1)?;
                insert_unique(&mut map, key, value, FORMAT, self.number)?;
                self.skip_spaces();
                match self.chars.get(self.pos) {
                    Some(',') => self.pos += 1,
                    Some('}') => {
                        self.pos += 1;
                        break;
                    }
                    None => return Err(self.fail("flow mapping is not closed on this line")),
                    _ => return Err(self.fail("expected ',' or '}' in a flow mapping")),
                }
            }
            Ok(Value::Object(map))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, AppletError> {
        self.budget.count()?;
        self.skip_spaces();
        match self.chars.get(self.pos) {
            Some('[') | Some('{') => self.collection(depth),
            Some('"') | Some('\'') => {
                quoted(self.chars, &mut self.pos, self.number).map(Value::String)
            }
            _ => {
                let plain = self.plain(&[',', ']', '}']);
                resolve_plain(plain.trim(), self.number)
            }
        }
    }

    fn key(&mut self) -> Result<String, AppletError> {
        self.skip_spaces();
        match self.chars.get(self.pos) {
            Some('"') | Some('\'') => quoted(self.chars, &mut self.pos, self.number),
            _ => {
                let plain = self.plain(&[',', ']', '}', ':']);
                let key = plain.trim();
                if key.is_empty() {
                    return Err(self.fail("flow mapping key is empty"));
                }
                Ok(key.to_owned())
            }
        }
    }

    fn plain(&mut self, stops: &[char]) -> String {
        let start = self.pos;
        while let Some(character) = self.chars.get(self.pos) {
            if stops.contains(character) {
                break;
            }
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }
}

pub(super) fn stringify(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let value = args
        .get("value")
        .ok_or_else(|| invalid("yaml.stringify requires args.value"))?;
    validate_stringify_depth(value, 0)?;
    let mut out = String::new();
    match value {
        Value::Object(map) if !map.is_empty() => write_mapping(&mut out, map, 0)?,
        Value::Array(items) if !items.is_empty() => write_sequence(&mut out, items, 0)?,
        other => {
            out.push_str(&inline_text(other)?);
            out.push('\n');
        }
    }
    check_text_size(out.len())?;
    Ok(Value::String(out))
}

fn validate_stringify_depth(value: &Value, depth: usize) -> Result<(), AppletError> {
    check_depth(depth, FORMAT, 1)?;
    match value {
        Value::Array(items) => {
            for item in items {
                validate_stringify_depth(item, depth + 1)?;
            }
        }
        Value::Object(fields) => {
            for item in fields.values() {
                validate_stringify_depth(item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn write_mapping(
    out: &mut String,
    map: &Map<String, Value>,
    indent: usize,
) -> Result<(), AppletError> {
    for (key, value) in map {
        push_indent(out, indent);
        out.push_str(&yaml_string(key));
        out.push(':');
        match value {
            Value::Object(child) if !child.is_empty() => {
                out.push('\n');
                write_mapping(out, child, indent + 2)?;
            }
            Value::Array(items) if !items.is_empty() => {
                out.push('\n');
                write_sequence(out, items, indent + 2)?;
            }
            other => {
                out.push(' ');
                out.push_str(&inline_text(other)?);
                out.push('\n');
            }
        }
    }
    Ok(())
}

fn write_sequence(out: &mut String, items: &[Value], indent: usize) -> Result<(), AppletError> {
    for item in items {
        push_indent(out, indent);
        out.push('-');
        match item {
            Value::Object(child) if !child.is_empty() => {
                out.push('\n');
                write_mapping(out, child, indent + 2)?;
            }
            Value::Array(children) if !children.is_empty() => {
                out.push('\n');
                write_sequence(out, children, indent + 2)?;
            }
            other => {
                out.push(' ');
                out.push_str(&inline_text(other)?);
                out.push('\n');
            }
        }
    }
    Ok(())
}

fn push_indent(out: &mut String, indent: usize) {
    out.extend(std::iter::repeat_n(' ', indent));
}

fn inline_text(value: &Value) -> Result<String, AppletError> {
    Ok(match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => {
            if number.is_u64() && number.as_i64().is_none() {
                return Err(invalid("YAML integers must fit in a signed 64-bit range"));
            }
            number.to_string()
        }
        Value::String(text) => yaml_string(text),
        Value::Object(map) if map.is_empty() => "{}".to_owned(),
        Value::Array(items) if items.is_empty() => "[]".to_owned(),
        Value::Object(_) | Value::Array(_) => {
            return Err(invalid("internal: collection written as an inline scalar"))
        }
    })
}

/// Write text plainly when it reads back unchanged; otherwise use a JSON string,
/// whose escapes the parser accepts.
fn yaml_string(text: &str) -> String {
    if plain_is_safe(text) {
        text.to_owned()
    } else {
        serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
    }
}

fn plain_is_safe(text: &str) -> bool {
    if text.is_empty() || text != text.trim() || text == "..." || text == "---" {
        return false;
    }
    let first = text.chars().next().unwrap_or(' ');
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    if text.contains(": ")
        || text.ends_with(':')
        || text.contains(" #")
        || text.contains('\t')
        || text.chars().any(char::is_control)
    {
        return false;
    }
    if text == "=" || text == "<<" || matches_yaml_implicit_resolver(text) {
        return false;
    }
    matches!(resolve_plain(text, 0), Ok(Value::String(_)))
}

fn matches_yaml_implicit_resolver(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "y" | "n" | "yes" | "no" | "on" | "off" | "null" | "~"
    ) {
        return true;
    }
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.starts_with("0x")
        || digits.starts_with("0X")
        || digits.starts_with("0o")
        || digits.starts_with("0O")
        || digits.starts_with("0b")
        || digits.starts_with("0B")
    {
        return true;
    }
    if text.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        || (matches!(text.chars().next(), Some('+' | '-'))
            && text.chars().nth(1).is_some_and(|ch| ch.is_ascii_digit()))
    {
        return true;
    }
    false
}
