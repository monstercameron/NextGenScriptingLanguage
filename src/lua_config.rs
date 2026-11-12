//! Typed reading and writing of small Lua configuration files (#442).
//!
//! Nothing here executes Lua. The module writes a fixed subset of Lua source and
//! reads that same subset back, refusing everything else with a line and column.
//!
//! Subset and its mapping to `serde_json::Value`:
//!
//! | Lua source                     | Value                                         |
//! |--------------------------------|-----------------------------------------------|
//! | `nil`                          | `Null` (also allowed inside lists)            |
//! | `true`, `false`                | `Bool`                                        |
//! | `42`, `-7`                     | integer, must fit in 64 bits                  |
//! | `1.5`, `1e21`, `100.0`         | finite float (a float always has `.` or `e`)  |
//! | `"text"`, `'text'`             | string (UTF-8 once escapes are applied)       |
//! | `{ v1, v2 }`                   | list (`Array`)                                |
//! | `{ ["k"] = v }`, `{ k = v }`   | record (`Object`)                             |
//!
//! Table rules:
//! - A table with only positional entries decodes to an `Array`.
//! - A table with only keyed entries decodes to an `Object`. Keys are strings: either
//!   the bracketed form `["key"]` or a plain identifier that is not a Lua keyword.
//! - A table that mixes positional and keyed entries is refused at the offending entry.
//! - A duplicate key in one table is refused.
//! - `{}` decodes to an empty `Object`. An empty `Array` therefore encodes to `{}` and
//!   comes back as an empty `Object`; every other value round-trips exactly.
//!
//! The encoder always writes strings in double quotes. Every byte outside printable
//! ASCII, and `"` and `\`, is written as a three-digit decimal escape, so no long
//! bracket, newline or quote can change how the text parses. The decoder accepts the
//! standard short escapes and decimal escapes, and refuses long brackets.
//!
//! The one-call file form written by [`encode_call`] and read by [`decode_call`] is
//! `o.workspace_mode({ ["workspace"] = "1", ["mode"] = "tile" })`, the shape of
//! Omarchy's Hyprland Lua helpers. Its function path is data for the caller to
//! dispatch on; nothing here calls it.

use serde_json::{Map, Value};
use std::fmt::{self, Write as _};

/// Deepest table nesting accepted by both the encoder and the decoder.
pub const MAX_DEPTH: usize = 128;

const RESERVED: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// A value that cannot be written in the Lua subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeError {
    /// Where the problem is inside the value, such as `$.hosts[2]`.
    pub path: String,
    pub message: String,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot write {}: {}", self.path, self.message)
    }
}

impl std::error::Error for EncodeError {}

/// Source text that is not data in the Lua subset, with its position (1-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl fmt::Display for LuaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for LuaError {}

/// A decoded one-call file: the dotted function path and its record argument.
#[derive(Debug, Clone, PartialEq)]
pub struct LuaCall {
    pub function: String,
    pub args: Value,
}

/// Encodes one value as a Lua literal in the subset.
pub fn encode(value: &Value) -> Result<String, EncodeError> {
    let mut out = String::new();
    write_value(value, 0, "$", &mut out)?;
    Ok(out)
}

/// Encodes a one-call file, `function.path({ ... })` followed by a newline.
/// The function path must be dot-separated plain names that are not Lua keywords,
/// and the argument must be an object.
pub fn encode_call(function: &str, args: &Value) -> Result<String, EncodeError> {
    if let Err(message) = check_function_path(function) {
        return Err(EncodeError {
            path: "function".to_string(),
            message,
        });
    }
    if !args.is_object() {
        return Err(EncodeError {
            path: "args".to_string(),
            message: "the call argument must be a record (object)".to_string(),
        });
    }
    let mut out = String::new();
    out.push_str(function);
    out.push('(');
    write_value(args, 0, "args", &mut out)?;
    out.push_str(")\n");
    Ok(out)
}

/// Decodes one value written in the subset. Surrounding whitespace and comments are
/// allowed; anything after the value is refused.
pub fn decode(src: &str) -> Result<Value, LuaError> {
    let mut p = Parser::new(src);
    p.skip_space()?;
    let value = p.value(0)?;
    p.skip_space()?;
    if p.peek().is_some() {
        return Err(p.error(format!(
            "expected the end of the value, found {}",
            p.found()
        )));
    }
    Ok(value)
}

/// Decodes a one-call file: `name.path({ ... })`, optionally followed by `;`.
/// The argument must be a record. Nothing is executed; the caller decides what the
/// function path means.
pub fn decode_call(src: &str) -> Result<LuaCall, LuaError> {
    let mut p = Parser::new(src);
    p.skip_space()?;
    let function = p.function_path()?;
    p.skip_space()?;
    p.expect(b'(')?;
    p.skip_space()?;
    let at = p.pos;
    if p.peek() != Some(b'{') {
        return Err(p.error(format!(
            "a call takes one table argument, found {}",
            p.found()
        )));
    }
    let args = p.table(0)?;
    if !args.is_object() {
        return Err(p.error_at(
            at,
            "the call argument must be a record of named fields, not a list",
        ));
    }
    p.skip_space()?;
    p.expect(b')')?;
    p.skip_space()?;
    if p.peek() == Some(b';') {
        p.pos += 1;
        p.skip_space()?;
    }
    if p.peek().is_some() {
        return Err(p.error(format!(
            "a file holds one call; found {} after it",
            p.found()
        )));
    }
    Ok(LuaCall { function, args })
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
}

fn is_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(c) if is_ident_start(c)) && bytes.all(is_ident_continue)
}

fn check_function_path(path: &str) -> Result<(), String> {
    for segment in path.split('.') {
        if !is_identifier(segment) {
            return Err(format!("'{segment}' is not a plain Lua name"));
        }
        if is_reserved(segment) {
            return Err(format!("'{segment}' is a Lua keyword"));
        }
    }
    Ok(())
}

fn check_depth(depth: usize, path: &str) -> Result<(), EncodeError> {
    if depth >= MAX_DEPTH {
        return Err(EncodeError {
            path: path.to_string(),
            message: format!("nesting is deeper than {MAX_DEPTH} tables"),
        });
    }
    Ok(())
}

fn write_value(
    value: &Value,
    depth: usize,
    path: &str,
    out: &mut String,
) -> Result<(), EncodeError> {
    match value {
        Value::Null => out.push_str("nil"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(n, path, out)?,
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            check_depth(depth, path)?;
            if items.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push_str("{ ");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(item, depth + 1, &format!("{path}[{}]", i + 1), out)?;
            }
            out.push_str(" }");
        }
        Value::Object(map) => {
            check_depth(depth, path)?;
            if map.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            // serde_json's default map is sorted, but sort explicitly so the output does
            // not depend on feature unification elsewhere in the build.
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push_str("{ ");
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('[');
                write_string(key, out);
                out.push_str("] = ");
                write_value(item, depth + 1, &format!("{path}.{key}"), out)?;
            }
            out.push_str(" }");
        }
    }
    Ok(())
}

fn write_number(n: &serde_json::Number, path: &str, out: &mut String) -> Result<(), EncodeError> {
    if n.is_f64() {
        return match n.as_f64() {
            // Debug formatting of f64 is the shortest round-trip form and always has
            // `.` or an exponent, so the decoder reads it back as a float.
            Some(f) if f.is_finite() => {
                let _ = write!(out, "{f:?}");
                Ok(())
            }
            _ => Err(EncodeError {
                path: path.to_string(),
                message: "NaN and infinity have no Lua literal in this subset".to_string(),
            }),
        };
    }
    match n.as_i64() {
        Some(i) => {
            let _ = write!(out, "{i}");
            Ok(())
        }
        None => Err(EncodeError {
            path: path.to_string(),
            message: "integer is outside the 64-bit Lua integer range".to_string(),
        }),
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for &b in s.as_bytes() {
        match b {
            b'"' | b'\\' | 0x00..=0x1f | 0x7f..=0xff => {
                let _ = write!(out, "\\{b:03}");
            }
            _ => out.push(char::from(b)),
        }
    }
    out.push('"');
}

enum Entry {
    Positional(Value),
    Keyed(String, Value),
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Parser {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    /// Line and column (both 1-based, column counted in characters) of byte offset `at`.
    fn location(&self, at: usize) -> (usize, usize) {
        let upto = self
            .bytes
            .get(..at.min(self.bytes.len()))
            .unwrap_or_default();
        let line = 1 + upto.iter().filter(|&&b| b == b'\n').count();
        let line_start = upto.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let column = 1 + upto
            .get(line_start..)
            .unwrap_or_default()
            .iter()
            .filter(|&&b| b & 0xC0 != 0x80)
            .count();
        (line, column)
    }

    fn error_at(&self, at: usize, message: impl Into<String>) -> LuaError {
        let (line, column) = self.location(at);
        LuaError {
            line,
            column,
            message: message.into(),
        }
    }

    fn error(&self, message: impl Into<String>) -> LuaError {
        self.error_at(self.pos, message)
    }

    /// Describes the character at the cursor for an error message.
    fn found(&self) -> String {
        match self
            .src
            .get(self.pos..)
            .and_then(|rest| rest.chars().next())
        {
            None => "end of input".to_string(),
            Some(c) if c.is_control() => format!("control character U+{:04X}", u32::from(c)),
            Some(c) => format!("'{c}'"),
        }
    }

    fn expect(&mut self, want: u8) -> Result<(), LuaError> {
        if self.peek() == Some(want) {
            self.pos += 1;
            return Ok(());
        }
        Err(self.error(format!(
            "expected '{}', found {}",
            char::from(want),
            self.found()
        )))
    }

    /// Skips whitespace and comments. `--[[ ]]` and `--` line comments are the only
    /// comments; `--[=[` long comments are refused.
    fn skip_space(&mut self) -> Result<(), LuaError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) => self.pos += 1,
                Some(b'-') if self.peek_at(1) == Some(b'-') => self.skip_comment()?,
                _ => return Ok(()),
            }
        }
    }

    fn skip_comment(&mut self) -> Result<(), LuaError> {
        let start = self.pos;
        self.pos += 2;
        if self.peek() == Some(b'[') {
            let mut bracket = self.pos + 1;
            while self.bytes.get(bracket) == Some(&b'=') {
                bracket += 1;
            }
            if self.bytes.get(bracket) == Some(&b'[') {
                if bracket != self.pos + 1 {
                    return Err(
                        self.error_at(start, "block comments with '=' levels are not supported")
                    );
                }
                self.pos = bracket + 1;
                loop {
                    match self.peek() {
                        None => return Err(self.error_at(start, "unterminated block comment")),
                        Some(b']') if self.peek_at(1) == Some(b']') => {
                            self.pos += 2;
                            return Ok(());
                        }
                        Some(_) => self.pos += 1,
                    }
                }
            }
        }
        while let Some(b) = self.peek() {
            if b == b'\n' {
                break;
            }
            self.pos += 1;
        }
        Ok(())
    }

    fn identifier(&mut self) -> String {
        let src = self.src;
        let start = self.pos;
        while self.peek().is_some_and(is_ident_continue) {
            self.pos += 1;
        }
        src.get(start..self.pos).unwrap_or_default().to_string()
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        self.pos - start
    }

    /// Dotted function path such as `o.workspace_mode`. No spaces, no `:` method calls.
    fn function_path(&mut self) -> Result<String, LuaError> {
        let mut segments: Vec<String> = Vec::new();
        loop {
            if !self.peek().is_some_and(is_ident_start) {
                return Err(self.error(format!(
                    "expected a function name such as o.name, found {}",
                    self.found()
                )));
            }
            let at = self.pos;
            let name = self.identifier();
            if is_reserved(&name) {
                return Err(self.error_at(at, format!("'{name}' is a Lua keyword")));
            }
            segments.push(name);
            if self.peek() != Some(b'.') {
                break;
            }
            self.pos += 1;
        }
        Ok(segments.join("."))
    }

    fn value(&mut self, depth: usize) -> Result<Value, LuaError> {
        match self.peek() {
            None => Err(self.error("expected a value, found end of input")),
            Some(b'{') => self.table(depth),
            Some(b'"' | b'\'') => self.string().map(Value::String),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b'[') if matches!(self.peek_at(1), Some(b'[' | b'=')) => Err(self
                .error("long-bracket strings are not supported; use a quoted string with escapes")),
            Some(c) if is_ident_start(c) => self.word(),
            Some(_) => Err(self.error(format!(
                "expected a value, found {}; only literals and tables are data",
                self.found()
            ))),
        }
    }

    fn word(&mut self) -> Result<Value, LuaError> {
        let at = self.pos;
        let name = self.identifier();
        match name.as_str() {
            "nil" => Ok(Value::Null),
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "function" => Err(self.error_at(at, "function definitions are code, not data")),
            _ if is_reserved(&name) => {
                Err(self.error_at(at, format!("'{name}' is a Lua keyword, not a value")))
            }
            _ => Err(self.error_at(
                at,
                format!("'{name}' is a variable reference; only literals and tables are data"),
            )),
        }
    }

    fn number(&mut self) -> Result<Value, LuaError> {
        let src = self.src;
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        if self.digits() == 0 {
            return Err(self.error_at(start, "expected digits after '-'"));
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.error_at(start, "expected digits after the decimal point"));
            }
            is_float = true;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.error_at(start, "expected digits in the exponent"));
            }
            is_float = true;
        }
        if self.peek().is_some_and(is_ident_continue) {
            return Err(self.error_at(
                start,
                "malformed number; hex and suffixes are not supported",
            ));
        }
        let text = src.get(start..self.pos).unwrap_or_default();
        if is_float {
            let f: f64 = text
                .parse()
                .map_err(|_| self.error_at(start, "malformed number"))?;
            let number = serde_json::Number::from_f64(f)
                .ok_or_else(|| self.error_at(start, "number is out of the finite float range"))?;
            Ok(Value::Number(number))
        } else {
            let i: i64 = text.parse().map_err(|_| {
                self.error_at(start, "integer is outside the 64-bit Lua integer range")
            })?;
            Ok(Value::Number(i.into()))
        }
    }

    fn string(&mut self) -> Result<String, LuaError> {
        let src = self.src;
        let start = self.pos;
        let Some(quote) = self.peek() else {
            return Err(self.error("expected a string"));
        };
        self.pos += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            match self.peek() {
                None => return Err(self.error_at(start, "unterminated string")),
                Some(b'\n' | b'\r') => {
                    return Err(self.error_at(
                        start,
                        "unterminated string: a line break ends it; write \\010 for a newline",
                    ))
                }
                Some(b'\\') => self.escape(&mut out)?,
                Some(c) if c == quote => {
                    self.pos += 1;
                    break;
                }
                Some(c) if c < 0x20 || c == 0x7f => {
                    return Err(self.error(format!(
                        "control character 0x{c:02X} in a string; write it as a decimal escape such as \\000"
                    )))
                }
                Some(_) => {
                    let Some(ch) = src.get(self.pos..).and_then(|rest| rest.chars().next()) else {
                        return Err(self.error("invalid text position"));
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    self.pos += ch.len_utf8();
                }
            }
        }
        String::from_utf8(out)
            .map_err(|_| self.error_at(start, "the escapes in this string do not form valid UTF-8"))
    }

    /// Standard short escapes and `\ddd` decimal escapes (one to three digits, at most 255).
    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), LuaError> {
        let start = self.pos;
        self.pos += 1;
        let Some(c) = self.peek() else {
            return Err(self.error_at(start, "unterminated escape at end of input"));
        };
        let short = match c {
            b'a' => Some(0x07),
            b'b' => Some(0x08),
            b'f' => Some(0x0c),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'v' => Some(0x0b),
            b'\\' => Some(b'\\'),
            b'"' => Some(b'"'),
            b'\'' => Some(b'\''),
            _ => None,
        };
        if let Some(byte) = short {
            out.push(byte);
            self.pos += 1;
            return Ok(());
        }
        if c.is_ascii_digit() {
            let mut value: u32 = 0;
            let mut digits = 0;
            while digits < 3 {
                match self.peek() {
                    Some(d) if d.is_ascii_digit() => {
                        value = value * 10 + u32::from(d - b'0');
                        self.pos += 1;
                        digits += 1;
                    }
                    _ => break,
                }
            }
            let byte = u8::try_from(value).map_err(|_| {
                self.error_at(start, format!("decimal escape \\{value} is above 255"))
            })?;
            out.push(byte);
            return Ok(());
        }
        Err(self.error(format!(
            "unsupported escape: a backslash followed by {}; use a short escape or \\ddd",
            self.found()
        )))
    }

    fn table(&mut self, depth: usize) -> Result<Value, LuaError> {
        let open = self.pos;
        if depth >= MAX_DEPTH {
            return Err(self.error_at(open, format!("nesting is deeper than {MAX_DEPTH} tables")));
        }
        self.expect(b'{')?;
        let mut positional: Vec<Value> = Vec::new();
        let mut keyed: Map<String, Value> = Map::new();
        loop {
            self.skip_space()?;
            match self.peek() {
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(self.error_at(open, "unterminated table: expected '}'")),
                Some(_) => {}
            }
            let entry_at = self.pos;
            match self.entry(depth)? {
                Entry::Positional(value) => {
                    if !keyed.is_empty() {
                        return Err(self.error_at(
                            entry_at,
                            "a table cannot mix positional and keyed entries",
                        ));
                    }
                    positional.push(value);
                }
                Entry::Keyed(key, value) => {
                    if !positional.is_empty() {
                        return Err(self.error_at(
                            entry_at,
                            "a table cannot mix positional and keyed entries",
                        ));
                    }
                    if keyed.contains_key(&key) {
                        return Err(self.error_at(entry_at, format!("duplicate key {key:?}")));
                    }
                    keyed.insert(key, value);
                }
            }
            self.skip_space()?;
            match self.peek() {
                Some(b',' | b';') => self.pos += 1,
                Some(b'}') => {}
                None => return Err(self.error_at(open, "unterminated table: expected '}'")),
                Some(_) => {
                    return Err(self.error(format!(
                        "expected ',', ';' or '}}' after a table entry, found {}",
                        self.found()
                    )))
                }
            }
        }
        if positional.is_empty() {
            Ok(Value::Object(keyed))
        } else {
            Ok(Value::Array(positional))
        }
    }

    fn entry(&mut self, depth: usize) -> Result<Entry, LuaError> {
        match self.peek() {
            Some(b'[') => {
                if matches!(self.peek_at(1), Some(b'[' | b'=')) {
                    return Err(self.error(
                        "long-bracket strings are not supported; use a quoted string with escapes",
                    ));
                }
                self.pos += 1;
                self.skip_space()?;
                if !matches!(self.peek(), Some(b'"' | b'\'')) {
                    return Err(self.error(format!(
                        "table keys must be strings written as [\"name\"], found {}",
                        self.found()
                    )));
                }
                let key = self.string()?;
                self.skip_space()?;
                self.expect(b']')?;
                self.skip_space()?;
                self.expect(b'=')?;
                self.skip_space()?;
                let value = self.value(depth + 1)?;
                Ok(Entry::Keyed(key, value))
            }
            Some(c) if is_ident_start(c) => {
                let start = self.pos;
                let name = self.identifier();
                self.skip_space()?;
                if self.peek() == Some(b'=') && self.peek_at(1) != Some(b'=') {
                    if is_reserved(&name) {
                        return Err(self.error_at(
                            start,
                            format!("'{name}' is a Lua keyword and cannot be a plain key; write [\"{name}\"]"),
                        ));
                    }
                    self.pos += 1;
                    self.skip_space()?;
                    let value = self.value(depth + 1)?;
                    return Ok(Entry::Keyed(name, value));
                }
                self.pos = start;
                Ok(Entry::Positional(self.value(depth + 1)?))
            }
            _ => Ok(Entry::Positional(self.value(depth + 1)?)),
        }
    }
}
