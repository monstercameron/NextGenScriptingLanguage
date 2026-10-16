//! Structural JSONC edits. The parser accepts line/block comments and trailing
//! commas, records byte spans, and changes only the selected root property.
//! `set` updates the last active duplicate (matching common JSON parsers' last
//! value behavior); `remove` removes every active duplicate. Keys in comments
//! and nested objects are not considered root settings.

const MAX_NESTING_DEPTH: usize = 256;
const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;

use super::filesystem::write_jsonc_update;
use super::{checkpoint_work, failed, invalid, required_string, AppletError, ExecutionContext};
use serde_json::{Map, Value};
use std::path::Path;

pub(super) fn set(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let key = required_string(args, "key")?.to_owned();
    let value = args
        .get("value")
        .ok_or_else(|| invalid("args.value is required"))?;
    let replacement = serde_json::to_string(value)
        .map_err(|error| failed("serialization_error", error.to_string()))?;
    write_jsonc_update(args, cwd, move |source| {
        Ok(Some(set_source(source, &key, &replacement, context)?))
    })
}

pub(super) fn remove(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let key = required_string(args, "key")?.to_owned();
    write_jsonc_update(args, cwd, move |source| {
        remove_source(source, &key, context)
    })
}

pub(super) fn edit_content(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let content = match args.get("content") {
        Some(Value::String(content)) => content.as_str(),
        Some(_) => return Err(invalid("args.content must be a string")),
        None => std::str::from_utf8(input)
            .map_err(|error| invalid(format!("stdin must be UTF-8 JSONC: {error}")))?,
    };
    if content.len() > MAX_CONTENT_BYTES {
        return Err(invalid(format!(
            "JSONC content exceeds the {MAX_CONTENT_BYTES}-byte limit"
        )));
    }
    let key = required_string(args, "key")?;
    let updated = match required_string(args, "action")? {
        "set" => {
            let value = args
                .get("value")
                .ok_or_else(|| invalid("args.value is required for action 'set'"))?;
            let replacement = serde_json::to_string(value)
                .map_err(|error| failed("serialization_error", error.to_string()))?;
            set_source(content, key, &replacement, context)?
        }
        "remove" => remove_source(content, key, context)?.unwrap_or_else(|| content.to_owned()),
        action => {
            return Err(invalid(format!(
                "args.action must be set or remove; received {action:?}"
            )));
        }
    };
    if updated.len() > MAX_CONTENT_BYTES {
        return Err(invalid(format!(
            "updated JSONC content exceeds the {MAX_CONTENT_BYTES}-byte limit"
        )));
    }
    context.checkpoint()?;
    Ok(Value::String(updated))
}

fn set_source(
    source: &str,
    key: &str,
    replacement: &str,
    context: &ExecutionContext<'_>,
) -> Result<String, AppletError> {
    context.checkpoint()?;
    let document = Parser::new(source, context).parse()?;
    let members = root_members(&document)?;
    for (index, member) in members.iter().rev().enumerate() {
        checkpoint_work(context, index)?;
        if member.key == key {
            let mut updated = source.to_owned();
            updated.replace_range(member.value.start..member.value.end, replacement);
            context.checkpoint()?;
            return Ok(updated);
        }
    }
    insert_root_member(source, &document, key, replacement, context)
}

fn remove_source(
    source: &str,
    key: &str,
    context: &ExecutionContext<'_>,
) -> Result<Option<String>, AppletError> {
    let mut updated = source.to_owned();
    let mut changed = false;
    loop {
        context.checkpoint()?;
        let document = Parser::new(&updated, context).parse()?;
        let members = root_members(&document)?;
        let mut found = None;
        for (index, member) in members.iter().enumerate() {
            checkpoint_work(context, index)?;
            if member.key == key {
                found = Some(index);
                break;
            }
        }
        let Some(index) = found else {
            break;
        };
        let range = removal_range(members, index);
        updated.replace_range(range, "");
        changed = true;
    }
    context.checkpoint()?;
    Ok(changed.then_some(updated))
}

#[derive(Debug)]
struct Node {
    start: usize,
    end: usize,
    kind: NodeKind,
}

#[derive(Debug)]
enum NodeKind {
    Object(Vec<Member>),
    Other,
}

#[derive(Debug)]
struct Member {
    key: String,
    key_start: usize,
    value: Node,
    comma: Option<(usize, usize)>,
}

fn root_members(node: &Node) -> Result<&[Member], AppletError> {
    match &node.kind {
        NodeKind::Object(members) => Ok(members),
        NodeKind::Other => Err(invalid("JSONC root value must be an object")),
    }
}

fn removal_range(members: &[Member], index: usize) -> std::ops::Range<usize> {
    let member = &members[index];
    if let Some((_, comma_end)) = member.comma {
        member.key_start..comma_end
    } else {
        // Leave the preceding comma as a valid JSONC trailing comma. This
        // preserves comments and whitespace attached to the previous member.
        member.key_start..member.value.end
    }
}

fn insert_root_member(
    source: &str,
    root: &Node,
    key: &str,
    value: &str,
    context: &ExecutionContext<'_>,
) -> Result<String, AppletError> {
    context.checkpoint()?;
    let members = root_members(root)?;
    let close = root.end - 1;
    let quoted_key = serde_json::to_string(key)
        .map_err(|error| failed("serialization_error", error.to_string()))?;
    let mut result = source.to_owned();
    let separator_position = members
        .last()
        .filter(|last| last.comma.is_none())
        .map(|last| last.value.end);

    // Multiline objects get a member on its own line. Insert at the start of
    // the closing-brace line so its original indentation remains with `}`.
    let line_start = source[..close].rfind('\n').map_or(0, |index| index + 1);
    let close_line_prefix = &source[line_start..close];
    let multiline_close = close_line_prefix
        .bytes()
        .all(|byte| byte == b' ' || byte == b'\t');
    if source[..close].contains('\n') && multiline_close {
        let closing_indent = close_line_prefix;
        let member_indent = members
            .first()
            .and_then(|first| indentation_at(source, first.key_start))
            .unwrap_or_else(|| format!("{closing_indent}  "));
        let newline = if source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let trailing_comma = members.last().is_some_and(|member| member.comma.is_some());
        let member = format!(
            "{member_indent}{quoted_key}: {value}{}{newline}",
            if trailing_comma { "," } else { "" }
        );
        result.insert_str(line_start, &member);
    } else {
        let trailing_comma = members.last().is_some_and(|member| member.comma.is_some());
        let member = format!(
            "{quoted_key}: {value}{}",
            if trailing_comma { "," } else { "" }
        );
        result.insert_str(close, &member);
    }
    if let Some(position) = separator_position {
        // Insert after adding the new member: this position is earlier than
        // the insertion point and therefore still indexes the original text.
        result.insert(position, ',');
    }
    context.checkpoint()?;
    Ok(result)
}

fn indentation_at(source: &str, position: usize) -> Option<String> {
    let line_start = source[..position].rfind('\n').map_or(0, |index| index + 1);
    let prefix = &source[line_start..position];
    prefix
        .bytes()
        .all(|byte| byte == b' ' || byte == b'\t')
        .then(|| prefix.to_owned())
}

struct Parser<'source, 'context, 'budget> {
    source: &'source str,
    bytes: &'source [u8],
    context: &'context ExecutionContext<'budget>,
    at: usize,
    depth: usize,
    next_checkpoint: usize,
}

impl<'source, 'context, 'budget> Parser<'source, 'context, 'budget> {
    fn new(source: &'source str, context: &'context ExecutionContext<'budget>) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            context,
            at: 0,
            depth: 0,
            next_checkpoint: 0,
        }
    }

    fn checkpoint_progress(&mut self) -> Result<(), AppletError> {
        if self.at >= self.next_checkpoint {
            self.next_checkpoint = self.at.saturating_add(256);
            self.context.checkpoint()?;
        }
        Ok(())
    }

    fn parse(mut self) -> Result<Node, AppletError> {
        self.context.checkpoint()?;
        self.skip_trivia()?;
        let root = self.value()?;
        self.skip_trivia()?;
        if self.at != self.bytes.len() {
            return Err(self.error("unexpected text after the root value"));
        }
        Ok(root)
    }

    fn value(&mut self) -> Result<Node, AppletError> {
        self.skip_trivia()?;
        let start = self.at;
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => {
                self.string()?;
                Ok(Node {
                    start,
                    end: self.at,
                    kind: NodeKind::Other,
                })
            }
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.error("expected a JSON value")),
        }
    }

    fn object(&mut self) -> Result<Node, AppletError> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.error("maximum nesting depth exceeded"));
        }
        self.depth += 1;
        let result = self.object_contents();
        self.depth -= 1;
        result
    }

    fn object_contents(&mut self) -> Result<Node, AppletError> {
        let start = self.at;
        self.at += 1;
        self.skip_trivia()?;
        let mut members = Vec::new();
        if self.consume(b'}') {
            return Ok(Node {
                start,
                end: self.at,
                kind: NodeKind::Object(members),
            });
        }
        loop {
            self.checkpoint_progress()?;
            self.skip_trivia()?;
            let key_start = self.at;
            let key = self.string()?;
            self.skip_trivia()?;
            self.expect(b':', "expected ':' after an object key")?;
            let value = self.value()?;
            self.skip_trivia()?;
            let comma = if self.consume(b',') {
                let span = (self.at - 1, self.at);
                self.skip_trivia()?;
                Some(span)
            } else {
                None
            };
            members.push(Member {
                key,
                key_start,
                value,
                comma,
            });
            if self.consume(b'}') {
                break;
            }
            if members.last().is_some_and(|member| member.comma.is_none()) {
                return Err(self.error("expected ',' or '}' after an object member"));
            }
            if self.peek().is_none() {
                return Err(self.error("unterminated object"));
            }
        }
        Ok(Node {
            start,
            end: self.at,
            kind: NodeKind::Object(members),
        })
    }

    fn array(&mut self) -> Result<Node, AppletError> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.error("maximum nesting depth exceeded"));
        }
        self.depth += 1;
        let result = self.array_contents();
        self.depth -= 1;
        result
    }

    fn array_contents(&mut self) -> Result<Node, AppletError> {
        let start = self.at;
        self.at += 1;
        self.skip_trivia()?;
        if self.consume(b']') {
            return Ok(Node {
                start,
                end: self.at,
                kind: NodeKind::Other,
            });
        }
        loop {
            self.checkpoint_progress()?;
            self.value()?;
            self.skip_trivia()?;
            if self.consume(b']') {
                break;
            }
            self.expect(b',', "expected ',' or ']' after an array item")?;
            self.skip_trivia()?;
            if self.consume(b']') {
                break;
            }
            if self.peek().is_none() {
                return Err(self.error("unterminated array"));
            }
        }
        Ok(Node {
            start,
            end: self.at,
            kind: NodeKind::Other,
        })
    }

    fn string(&mut self) -> Result<String, AppletError> {
        let start = self.at;
        self.expect(b'"', "expected a quoted JSON string")?;
        while let Some(byte) = self.peek() {
            self.checkpoint_progress()?;
            match byte {
                b'"' => {
                    self.at += 1;
                    let token = &self.source[start..self.at];
                    return serde_json::from_str(token)
                        .map_err(|error| self.error(&format!("invalid string: {error}")));
                }
                b'\\' => {
                    self.at += 1;
                    match self.peek() {
                        Some(b'u') => {
                            self.at += 1;
                            for _ in 0..4 {
                                match self.peek() {
                                    Some(byte) if byte.is_ascii_hexdigit() => self.at += 1,
                                    _ => return Err(self.error("invalid Unicode escape")),
                                }
                            }
                        }
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.at += 1;
                        }
                        _ => return Err(self.error("invalid string escape")),
                    }
                }
                byte if byte < 0x20 => return Err(self.error("control byte in string")),
                _ => self.at += 1,
            }
        }
        Err(self.error("unterminated string"))
    }

    fn literal(&mut self, literal: &str) -> Result<Node, AppletError> {
        let start = self.at;
        let end = start + literal.len();
        if self.bytes.get(start..end) != Some(literal.as_bytes()) {
            return Err(self.error("invalid JSON literal"));
        }
        self.at = end;
        Ok(Node {
            start,
            end,
            kind: NodeKind::Other,
        })
    }

    fn number(&mut self) -> Result<Node, AppletError> {
        let start = self.at;
        while self.peek().is_some_and(|byte| {
            !byte.is_ascii_whitespace() && !matches!(byte, b',' | b']' | b'}' | b'/' | b':')
        }) {
            self.checkpoint_progress()?;
            self.at += 1;
        }
        let token = &self.source[start..self.at];
        serde_json::from_str::<Value>(token)
            .map_err(|error| self.error(&format!("invalid number: {error}")))?;
        Ok(Node {
            start,
            end: self.at,
            kind: NodeKind::Other,
        })
    }

    fn skip_trivia(&mut self) -> Result<(), AppletError> {
        loop {
            self.checkpoint_progress()?;
            while self
                .peek()
                .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
            {
                self.checkpoint_progress()?;
                self.at += 1;
            }
            match (self.peek(), self.bytes.get(self.at + 1).copied()) {
                (Some(b'/'), Some(b'/')) => {
                    self.at += 2;
                    while self
                        .peek()
                        .is_some_and(|byte| byte != b'\n' && byte != b'\r')
                    {
                        self.checkpoint_progress()?;
                        self.at += 1;
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    self.at += 2;
                    let mut closed = false;
                    while self.at + 1 < self.bytes.len() {
                        self.checkpoint_progress()?;
                        if self.bytes[self.at] == b'*' && self.bytes[self.at + 1] == b'/' {
                            self.at += 2;
                            closed = true;
                            break;
                        }
                        self.at += 1;
                    }
                    if !closed {
                        return Err(self.error("unterminated block comment"));
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn expect(&mut self, byte: u8, message: &str) -> Result<(), AppletError> {
        if self.consume(byte) {
            Ok(())
        } else {
            Err(self.error(message))
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn error(&self, message: &str) -> AppletError {
        invalid(format!("invalid JSONC at byte {}: {message}", self.at))
    }
}
