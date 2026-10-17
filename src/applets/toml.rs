//! Strict TOML subset converted to and from JSON.
//!
//! Supported: bare, quoted and dotted keys; basic, literal and multi-line strings
//! with the TOML escapes; decimal, hexadecimal, octal and binary integers with
//! underscores; decimal floats; booleans; arrays that may span lines with a
//! trailing comma; inline tables; standard tables `[a.b]`; arrays of tables
//! `[[a]]`; and comments. Dates, times, `inf` and `nan` fail explicitly.
//!
//! A table defined by a header may not be defined twice, a table created by a
//! dotted key or an inline table may not be extended by a header, and a key may
//! not change kind. Duplicate keys fail.
//!
//! Stringify writes scalars and arrays first, then sub-tables and arrays of
//! tables. Null fails because TOML has no null value.

use super::config::{
    check_depth, count_value, insert_unique, integer_in_range, syntax, MAX_CONFIG_DEPTH,
};
use super::text::{check_text_size, text_input};
use super::{checkpoint_work, invalid, AppletError, ExecutionContext};
use serde_json::{Map, Value};
use std::cell::Cell;
use std::collections::HashSet;
use std::fmt::Display;

const FORMAT: &str = "toml";

pub(super) fn parse(
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let text = text_input(args, input)?;
    check_text_size(text.len())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
        line_cache: Cell::new((0, 1)),
        context,
        ticks: 0,
        statements: 0,
        values: 0,
        root: Map::new(),
        current: Vec::new(),
        explicit: HashSet::new(),
        implicit: HashSet::new(),
        arrays: HashSet::new(),
    };
    parser.document()?;
    Ok(Value::Object(parser.root))
}

struct Parser<'a, 'c, 'x> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// Last (byte offset, line number) computed, so line lookups stay linear.
    line_cache: Cell<(usize, usize)>,
    context: &'c ExecutionContext<'x>,
    ticks: usize,
    statements: usize,
    values: usize,
    root: Map<String, Value>,
    current: Vec<String>,
    /// Tables defined by a `[header]`.
    explicit: HashSet<Vec<String>>,
    /// Tables created implicitly as the prefix of a later header.
    implicit: HashSet<Vec<String>>,
    /// Arrays of tables defined by `[[header]]`.
    arrays: HashSet<Vec<String>>,
}

impl Parser<'_, '_, '_> {
    fn error(&self, message: impl Display) -> AppletError {
        syntax(FORMAT, self.line(), message)
    }

    /// One-based line of the cursor. The position only moves forward, so each
    /// lookup scans only the bytes since the previous lookup.
    fn line(&self) -> usize {
        let end = self.pos.min(self.bytes.len());
        let (mut cached_at, mut line) = self.line_cache.get();
        if end < cached_at {
            cached_at = 0;
            line = 1;
        }
        line += self.bytes[cached_at..end]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        self.line_cache.set((end, line));
        line
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some(b'#') {
            while !matches!(self.peek(), None | Some(b'\n')) {
                self.pos += 1;
            }
        }
    }

    /// Skip whitespace, line breaks and comments between statements and inside
    /// arrays.
    fn skip_blank(&mut self) {
        loop {
            self.skip_spaces();
            self.skip_comment();
            match self.peek() {
                Some(b'\n') => self.pos += 1,
                Some(b'\r') if self.peek_at(1) == Some(b'\n') => self.pos += 2,
                _ => break,
            }
        }
    }

    /// After a statement, only spaces, an optional comment and a line break may
    /// follow.
    fn expect_line_end(&mut self) -> Result<(), AppletError> {
        self.skip_spaces();
        self.skip_comment();
        match self.peek() {
            None => Ok(()),
            Some(b'\n') => {
                self.pos += 1;
                Ok(())
            }
            Some(b'\r') if self.peek_at(1) == Some(b'\n') => {
                self.pos += 2;
                Ok(())
            }
            Some(_) => Err(self.error("unexpected characters after a value")),
        }
    }

    fn document(&mut self) -> Result<(), AppletError> {
        loop {
            self.skip_blank();
            let Some(byte) = self.peek() else {
                return Ok(());
            };
            checkpoint_work(self.context, self.statements)?;
            self.statements += 1;
            if byte == b'[' {
                if self.peek_at(1) == Some(b'[') {
                    self.array_header()?;
                } else {
                    self.table_header()?;
                }
            } else {
                let keys = self.key_path()?;
                self.skip_spaces();
                if self.peek() != Some(b'=') {
                    return Err(self.error("expected '=' after a key"));
                }
                self.pos += 1;
                self.skip_spaces();
                let value = self.value(0)?;
                self.insert_current(keys, value)?;
            }
            self.expect_line_end()?;
        }
    }

    /// Parse a dotted key. Each component counts toward the value cap, and the
    /// path is rejected as soon as it would exceed the nesting limit, so a
    /// 10,000-component key fails after 65 components rather than building the
    /// whole vector.
    fn key_path(&mut self) -> Result<Vec<String>, AppletError> {
        let mut keys = Vec::new();
        loop {
            if keys.len() == MAX_CONFIG_DEPTH {
                return Err(self.error(format!("dotted key exceeds {MAX_CONFIG_DEPTH} components")));
            }
            count_value(&mut self.values)?;
            keys.push(self.key()?);
            self.skip_spaces();
            if self.peek() != Some(b'.') {
                return Ok(keys);
            }
            self.pos += 1;
            self.skip_spaces();
        }
    }

    fn key(&mut self) -> Result<String, AppletError> {
        match self.peek() {
            Some(b'"') => self.basic_string(),
            Some(b'\'') => self.literal_string(),
            _ => {
                let start = self.pos;
                while matches!(self.peek(), Some(byte) if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                {
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(self.error("expected a key"));
                }
                Ok(self.text[start..self.pos].to_owned())
            }
        }
    }

    fn table_header(&mut self) -> Result<(), AppletError> {
        self.pos += 1;
        self.skip_spaces();
        let keys = self.key_path()?;
        self.skip_spaces();
        if self.peek() != Some(b']') {
            return Err(self.error("expected ']' to close a table header"));
        }
        self.pos += 1;
        self.define_table(keys)
    }

    fn array_header(&mut self) -> Result<(), AppletError> {
        self.pos += 2;
        self.skip_spaces();
        let keys = self.key_path()?;
        self.skip_spaces();
        if self.peek() != Some(b']') || self.peek_at(1) != Some(b']') {
            return Err(self.error("expected ']]' to close an array-of-tables header"));
        }
        self.pos += 2;
        self.define_array(keys)
    }

    fn define_table(&mut self, keys: Vec<String>) -> Result<(), AppletError> {
        self.ensure_prefixes(&keys)?;
        let line = self.line();
        let (last, prefix) = keys.split_last().expect("key paths are nonempty");
        let parent = navigate_mut(&mut self.root, prefix)
            .map_err(|_| syntax(FORMAT, line, "table header parent is not a table"))?;
        match parent.get(last) {
            None => {
                parent.insert(last.clone(), Value::Object(Map::new()));
            }
            Some(Value::Object(_))
                if self.implicit.contains(&keys) && !self.explicit.contains(&keys) => {}
            Some(Value::Array(_)) => {
                return Err(syntax(
                    FORMAT,
                    line,
                    "table header conflicts with an array of tables",
                ));
            }
            Some(_) => {
                return Err(syntax(
                    FORMAT,
                    line,
                    "table header repeats a table or value",
                ))
            }
        }
        self.explicit.insert(keys.clone());
        self.current = keys;
        Ok(())
    }

    fn define_array(&mut self, keys: Vec<String>) -> Result<(), AppletError> {
        self.ensure_prefixes(&keys)?;
        let line = self.line();
        let (last, prefix) = keys.split_last().expect("key paths are nonempty");
        let parent = navigate_mut(&mut self.root, prefix)
            .map_err(|_| syntax(FORMAT, line, "array-of-tables parent is not a table"))?;
        match parent.get_mut(last) {
            None => {
                parent.insert(last.clone(), Value::Array(vec![Value::Object(Map::new())]));
                self.arrays.insert(keys.clone());
            }
            Some(Value::Array(items)) if self.arrays.contains(&keys) => {
                items.push(Value::Object(Map::new()));
            }
            Some(_) => {
                return Err(syntax(
                    FORMAT,
                    line,
                    "array-of-tables header conflicts with a table or value",
                ))
            }
        }
        // Tables under the previous element belong to that element only.
        self.explicit
            .retain(|path| !is_strict_descendant(path, &keys));
        self.implicit
            .retain(|path| !is_strict_descendant(path, &keys));
        self.arrays
            .retain(|path| !is_strict_descendant(path, &keys));
        self.current = keys;
        Ok(())
    }

    /// Create missing implicit tables above a header and check that existing
    /// prefixes may be extended by a header.
    fn ensure_prefixes(&mut self, keys: &[String]) -> Result<(), AppletError> {
        for depth in 1..keys.len() {
            let path = &keys[..depth];
            let (last, parent_path) = path.split_last().expect("prefix is nonempty");
            let line = self.line();
            let parent = navigate_mut(&mut self.root, parent_path)
                .map_err(|_| syntax(FORMAT, line, "table header parent is not a table"))?;
            if !parent.contains_key(last) {
                parent.insert(last.clone(), Value::Object(Map::new()));
                self.implicit.insert(path.to_vec());
                continue;
            }
            let extendable = match parent.get(last) {
                Some(Value::Object(_)) => {
                    self.implicit.contains(path) || self.explicit.contains(path)
                }
                Some(Value::Array(_)) => self.arrays.contains(path),
                _ => false,
            };
            if !extendable {
                return Err(syntax(
                    FORMAT,
                    line,
                    "table header extends a value that is not a table",
                ));
            }
        }
        Ok(())
    }

    fn insert_current(&mut self, keys: Vec<String>, value: Value) -> Result<(), AppletError> {
        let current = self.current.clone();
        let line = self.line();
        let table = navigate_mut(&mut self.root, &current)
            .map_err(|_| syntax(FORMAT, line, "current table is not a table"))?;
        insert_path(table, &keys, value, FORMAT, line, current.len())
    }

    /// Cooperative cancellation point, checked every 256 steps so a long flat
    /// array or inline table can be interrupted by a timeout or cancellation.
    fn tick(&mut self) -> Result<(), AppletError> {
        checkpoint_work(self.context, self.ticks)?;
        self.ticks += 1;
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<Value, AppletError> {
        self.tick()?;
        check_depth(depth, FORMAT, self.line())?;
        count_value(&mut self.values)?;
        match self.peek() {
            Some(b'"') => {
                if self.starts_with("\"\"\"") {
                    self.multi_line_string(b'"')
                } else {
                    self.basic_string().map(Value::String)
                }
            }
            Some(b'\'') => {
                if self.starts_with("'''") {
                    self.multi_line_string(b'\'')
                } else {
                    self.literal_string().map(Value::String)
                }
            }
            Some(b'[') => self.array(depth),
            Some(b'{') => self.inline_table(depth),
            Some(_) => self.bare_value(),
            None => Err(self.error("expected a value")),
        }
    }

    fn starts_with(&self, pattern: &str) -> bool {
        self.bytes[self.pos..].starts_with(pattern.as_bytes())
    }

    fn array(&mut self, depth: usize) -> Result<Value, AppletError> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.tick()?;
            self.skip_blank();
            if self.peek() == Some(b']') {
                self.pos += 1;
                break;
            }
            items.push(self.value(depth + 1)?);
            self.skip_blank();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or ']' in an array")),
            }
        }
        Ok(Value::Array(items))
    }

    fn inline_table(&mut self, depth: usize) -> Result<Value, AppletError> {
        self.pos += 1;
        let mut map = Map::new();
        self.skip_spaces();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.tick()?;
            self.skip_spaces();
            let keys = self.key_path()?;
            self.skip_spaces();
            if self.peek() != Some(b'=') {
                return Err(self.error("expected '=' in an inline table"));
            }
            self.pos += 1;
            self.skip_spaces();
            let value = self.value(depth + 1)?;
            let line = self.line();
            // The inline table sits one level below its enclosing table, and
            // `depth` counts the array and inline-table nesting around it.
            insert_path(
                &mut map,
                &keys,
                value,
                FORMAT,
                line,
                self.current.len() + 1 + depth,
            )?;
            self.skip_spaces();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or '}' in an inline table")),
            }
        }
        Ok(Value::Object(map))
    }

    fn bare_value(&mut self) -> Result<Value, AppletError> {
        let start = self.pos;
        while matches!(self.peek(), Some(byte) if byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.' | b'_' | b':'))
        {
            self.pos += 1;
        }
        let token = &self.text[start..self.pos];
        if token.is_empty() {
            return Err(self.error("expected a value"));
        }
        let line = self.line();
        classify_token(token, line)
    }

    fn basic_string(&mut self) -> Result<String, AppletError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None | Some(b'\n' | b'\r') => {
                    return Err(self.error("unterminated basic string"));
                }
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    out.push(self.escape()?);
                }
                Some(_) => {
                    let character = self.next_char()?;
                    if is_disallowed_control(character) {
                        return Err(self.error("control characters must be escaped"));
                    }
                    out.push(character);
                }
            }
        }
    }

    fn literal_string(&mut self) -> Result<String, AppletError> {
        self.pos += 1;
        let start = self.pos;
        loop {
            match self.peek() {
                None | Some(b'\n' | b'\r') => {
                    return Err(self.error("unterminated literal string"));
                }
                Some(b'\'') => {
                    let value = self.text[start..self.pos].to_owned();
                    self.pos += 1;
                    if value.chars().any(is_disallowed_control) {
                        return Err(self.error("control characters are not allowed here"));
                    }
                    return Ok(value);
                }
                Some(_) => {
                    self.next_char()?;
                }
            }
        }
    }

    fn multi_line_string(&mut self, quote: u8) -> Result<Value, AppletError> {
        self.pos += 3;
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        } else if self.peek() == Some(b'\r') && self.peek_at(1) == Some(b'\n') {
            self.pos += 2;
        }
        let mut out = String::new();
        loop {
            if self.peek() == Some(quote) {
                let mut run = 0;
                while self.peek_at(run) == Some(quote) {
                    run += 1;
                }
                if run >= 3 {
                    if run > 5 {
                        return Err(
                            self.error("too many quotation marks before a closing delimiter")
                        );
                    }
                    for _ in 0..run - 3 {
                        out.push(quote as char);
                    }
                    self.pos += run;
                    return Ok(Value::String(out));
                }
                for _ in 0..run {
                    out.push(quote as char);
                }
                self.pos += run;
                continue;
            }
            match self.peek() {
                None => return Err(self.error("unterminated multi-line string")),
                Some(b'\\') if quote == b'"' => {
                    let mut look = self.pos + 1;
                    while matches!(self.bytes.get(look), Some(b' ' | b'\t')) {
                        look += 1;
                    }
                    if matches!(self.bytes.get(look), Some(b'\n' | b'\r')) {
                        // A backslash before a line break trims the whitespace
                        // that follows, including further line breaks.
                        self.pos = look;
                        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                            self.pos += 1;
                        }
                    } else {
                        self.pos += 1;
                        out.push(self.escape()?);
                    }
                }
                Some(b'\r') if self.peek_at(1) == Some(b'\n') => {
                    out.push('\n');
                    self.pos += 2;
                }
                Some(_) => {
                    let character = self.next_char()?;
                    if is_disallowed_control(character) && character != '\n' {
                        return Err(self.error("control characters must be escaped"));
                    }
                    out.push(character);
                }
            }
        }
    }

    fn escape(&mut self) -> Result<char, AppletError> {
        let code = self
            .peek()
            .ok_or_else(|| self.error("unterminated escape sequence"))?;
        self.pos += 1;
        let character = match code {
            b'b' => '\u{8}',
            b't' => '\t',
            b'n' => '\n',
            b'f' => '\u{c}',
            b'r' => '\r',
            b'"' => '"',
            b'\\' => '\\',
            b'u' => return self.unicode_escape(4),
            b'U' => return self.unicode_escape(8),
            _ => return Err(self.error("unsupported escape sequence")),
        };
        Ok(character)
    }

    fn unicode_escape(&mut self, digits: usize) -> Result<char, AppletError> {
        let end = self.pos + digits;
        let hex = self
            .text
            .get(self.pos..end)
            .filter(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| self.error("unicode escape needs hexadecimal digits"))?;
        let code =
            u32::from_str_radix(hex, 16).map_err(|_| self.error("invalid unicode escape"))?;
        self.pos = end;
        char::from_u32(code).ok_or_else(|| self.error("unicode escape is not a scalar value"))
    }

    fn next_char(&mut self) -> Result<char, AppletError> {
        let character = self.text[self.pos..]
            .chars()
            .next()
            .ok_or_else(|| self.error("unexpected end of input"))?;
        self.pos += character.len_utf8();
        Ok(character)
    }
}

fn is_disallowed_control(character: char) -> bool {
    (character < ' ' && character != '\t') || character == '\u{7f}'
}

fn is_strict_descendant(path: &[String], ancestor: &[String]) -> bool {
    path.len() > ancestor.len() && path.starts_with(ancestor)
}

/// Walk into a table, stepping into the last element of an array of tables.
fn navigate_mut<'m>(
    root: &'m mut Map<String, Value>,
    path: &[String],
) -> Result<&'m mut Map<String, Value>, ()> {
    let mut table = root;
    for key in path {
        table = match table.get_mut(key) {
            Some(Value::Object(map)) => map,
            Some(Value::Array(items)) => match items.last_mut() {
                Some(Value::Object(map)) => map,
                _ => return Err(()),
            },
            _ => return Err(()),
        };
    }
    Ok(table)
}

/// Insert a value at a dotted path below `map`. `base_depth` is the nesting
/// already consumed by the table that owns `map`, so the total cannot exceed the
/// shared limit.
fn insert_path(
    map: &mut Map<String, Value>,
    keys: &[String],
    value: Value,
    format: &str,
    line: usize,
    base_depth: usize,
) -> Result<(), AppletError> {
    if base_depth.saturating_add(keys.len()) > MAX_CONFIG_DEPTH {
        return Err(syntax(
            format,
            line,
            format!("key nesting exceeds {MAX_CONFIG_DEPTH} levels"),
        ));
    }
    let (last, prefix) = keys.split_last().expect("key paths are nonempty");
    let mut table = map;
    for key in prefix {
        if !table.contains_key(key) {
            table.insert(key.clone(), Value::Object(Map::new()));
        }
        table = match table.get_mut(key) {
            Some(Value::Object(inner)) => inner,
            _ => return Err(syntax(format, line, format!("key {key:?} is not a table"))),
        };
    }
    insert_unique(table, last.clone(), value, format, line)
}

fn classify_token(token: &str, line: usize) -> Result<Value, AppletError> {
    match token {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "inf" | "+inf" | "-inf" | "nan" | "+nan" | "-nan" => {
            return Err(syntax(FORMAT, line, "inf and nan values are not supported"));
        }
        _ => {}
    }
    let bytes = token.as_bytes();
    let date_like =
        bytes.len() >= 5 && bytes[..4].iter().all(u8::is_ascii_digit) && bytes[4] == b'-';
    if date_like || token.contains(':') {
        return Err(syntax(FORMAT, line, "dates and times are not supported"));
    }
    let (sign, body) = match token.as_bytes().first() {
        Some(b'+') => (1i128, &token[1..]),
        Some(b'-') => (-1i128, &token[1..]),
        _ => (1i128, token),
    };
    if let Some(radix_body) = body
        .strip_prefix("0x")
        .or_else(|| body.strip_prefix("0o"))
        .or_else(|| body.strip_prefix("0b"))
    {
        if sign < 0 || !underscores_are_valid(radix_body) {
            return Err(syntax(FORMAT, line, "invalid prefixed integer"));
        }
        let radix = match &body[..2] {
            "0x" => 16,
            "0o" => 8,
            _ => 2,
        };
        let digits: String = radix_body.chars().filter(|c| *c != '_').collect();
        let value = i128::from_str_radix(&digits, radix)
            .map_err(|_| syntax(FORMAT, line, "invalid prefixed integer"))?;
        return Ok(Value::from(integer_in_range(value, FORMAT, line)?));
    }
    if !body.as_bytes().first().is_some_and(u8::is_ascii_digit) || !underscores_are_valid(body) {
        return Err(syntax(FORMAT, line, format!("invalid value {token:?}")));
    }
    let digits: String = body.chars().filter(|c| *c != '_').collect();
    if body.contains(['.', 'e', 'E']) {
        if digits.starts_with('0') && digits.len() > 1 && digits.as_bytes()[1].is_ascii_digit() {
            return Err(syntax(FORMAT, line, "leading zeros are not allowed"));
        }
        if !float_shape_is_valid(&digits) {
            return Err(syntax(FORMAT, line, format!("invalid float {token:?}")));
        }
        let signed = format!("{}{}", if sign < 0 { "-" } else { "" }, digits);
        let number: f64 = signed
            .parse()
            .map_err(|_| syntax(FORMAT, line, format!("invalid float {token:?}")))?;
        return serde_json::Number::from_f64(number)
            .map(Value::Number)
            .ok_or_else(|| syntax(FORMAT, line, "float is not finite"));
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(syntax(FORMAT, line, "leading zeros are not allowed"));
    }
    let value: i128 = digits
        .parse::<i128>()
        .map_err(|_| syntax(FORMAT, line, "integer is outside the signed 64-bit range"))?
        * sign;
    Ok(Value::from(integer_in_range(value, FORMAT, line)?))
}

/// Underscores are allowed only between two digits.
fn underscores_are_valid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().all(|(index, byte)| {
        *byte != b'_'
            || (index > 0
                && index + 1 < bytes.len()
                && bytes[index - 1].is_ascii_hexdigit()
                && bytes[index + 1].is_ascii_hexdigit())
    })
}

/// Digits, then an optional fraction with at least one digit, then an optional
/// exponent with at least one digit.
fn float_shape_is_valid(text: &str) -> bool {
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    let (integer, fraction) = match mantissa.find('.') {
        Some(index) => (&mantissa[..index], Some(&mantissa[index + 1..])),
        None => (mantissa, None),
    };
    let digits_only =
        |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if !digits_only(integer) {
        return false;
    }
    if let Some(fraction) = fraction {
        if !digits_only(fraction) {
            return false;
        }
    }
    match exponent {
        None => fraction.is_some(),
        Some(exponent) => {
            let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
            digits_only(exponent)
        }
    }
}

pub(super) fn stringify(args: &Map<String, Value>) -> Result<Value, AppletError> {
    let Some(Value::Object(map)) = args.get("value") else {
        return Err(invalid(
            "toml.stringify requires args.value to be a JSON object",
        ));
    };
    let mut out = String::new();
    write_table(&mut out, &mut Vec::new(), map)?;
    check_text_size(out.len())?;
    Ok(Value::String(out))
}

fn is_table_array(value: &Value) -> bool {
    matches!(value, Value::Array(items) if !items.is_empty() && items.iter().all(|item| matches!(item, Value::Object(_))))
}

fn write_table(
    out: &mut String,
    path: &mut Vec<String>,
    map: &Map<String, Value>,
) -> Result<(), AppletError> {
    for (key, value) in map {
        if matches!(value, Value::Object(_)) || is_table_array(value) {
            continue;
        }
        out.push_str(&key_text(key));
        out.push_str(" = ");
        write_inline(out, value)?;
        out.push('\n');
    }
    for (key, value) in map {
        match value {
            Value::Object(child) => {
                path.push(key.clone());
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push('[');
                out.push_str(&path_text(path));
                out.push_str("]\n");
                write_table(out, path, child)?;
                path.pop();
            }
            Value::Array(items) if is_table_array(value) => {
                path.push(key.clone());
                for item in items {
                    if let Value::Object(child) = item {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str("[[");
                        out.push_str(&path_text(path));
                        out.push_str("]]\n");
                        write_table(out, path, child)?;
                    }
                }
                path.pop();
            }
            _ => {}
        }
    }
    Ok(())
}

fn write_inline(out: &mut String, value: &Value) -> Result<(), AppletError> {
    match value {
        Value::Null => Err(invalid(
            "TOML has no null value; remove the key or use a string",
        )),
        Value::Bool(flag) => {
            out.push_str(if *flag { "true" } else { "false" });
            Ok(())
        }
        Value::Number(number) => {
            if number.is_u64() && number.as_i64().is_none() {
                return Err(invalid("TOML integers must fit in a signed 64-bit range"));
            }
            out.push_str(&number.to_string());
            Ok(())
        }
        Value::String(text) => {
            out.push_str(&string_text(text));
            Ok(())
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_inline(out, item)?;
            }
            out.push(']');
            Ok(())
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push_str("{ ");
            for (index, (key, member)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&key_text(key));
                out.push_str(" = ");
                write_inline(out, member)?;
            }
            out.push_str(" }");
            Ok(())
        }
    }
}

/// JSON string escapes are a subset of TOML basic-string escapes, so the JSON
/// encoding is a valid TOML basic string.
fn string_text(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

fn key_text(key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        key.to_owned()
    } else {
        string_text(key)
    }
}

fn path_text(path: &[String]) -> String {
    path.iter()
        .map(|key| key_text(key))
        .collect::<Vec<_>>()
        .join(".")
}
