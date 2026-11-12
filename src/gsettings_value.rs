//! Parser for the GVariant text that `gsettings get` prints (issue #413).
//!
//! `gsettings get` prints values in GVariant text form: `'dark'`, `true`, `1.5`,
//! `uint32 7`, `['a', 'b']`, `(1, 'x')`, `@a{sv} {'k': <1>}`. [`parse_gvariant_text`]
//! maps that text to a `serde_json::Value`. It starts no process and reads no file.

use serde_json::Value;
use std::fmt;

/// Nesting limit for arrays, tuples, dictionaries and variants.
const MAX_DEPTH: usize = 128;

/// A parse failure, with the byte offset in the input where it was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GVariantError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for GVariantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid GVariant text at byte {}: {}",
            self.offset, self.message
        )
    }
}

impl std::error::Error for GVariantError {}

/// Parses one GVariant text value, as printed by `gsettings get`.
///
/// Booleans, strings, integers and doubles map to JSON scalars. Arrays and tuples
/// map to JSON arrays, and dictionaries with string keys map to JSON objects. Variant
/// wrappers `<...>` and type annotations (`@as`, `uint32 7`) are unwrapped. Values
/// JSON cannot hold (non-finite doubles, `nothing`, `just`, non-string dictionary
/// keys) and malformed text return a [`GVariantError`].
pub fn parse_gvariant_text(text: &str) -> Result<Value, GVariantError> {
    let mut parser = Parser {
        src: text,
        pos: 0,
        depth: 0,
    };
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != text.len() {
        return Err(parser.error("unexpected text after the value"));
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy)]
enum Num {
    Int(i128),
    Float(f64),
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: impl Into<String>) -> GVariantError {
        GVariantError {
            offset: self.pos,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn take_while(&mut self, keep: impl Fn(char) -> bool) -> &'a str {
        let src: &'a str = self.src;
        let start = self.pos;
        while let Some(c) = self.peek() {
            if !keep(c) {
                break;
            }
            self.pos += c.len_utf8();
        }
        &src[start..self.pos]
    }

    fn expect(&mut self, want: char) -> Result<(), GVariantError> {
        match self.peek() {
            Some(c) if c == want => {
                self.bump();
                Ok(())
            }
            Some(c) => Err(self.error(format!("expected '{want}', found '{c}'"))),
            None => Err(self.error(format!("expected '{want}', found end of input"))),
        }
    }

    fn value(&mut self) -> Result<Value, GVariantError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error("value nests too deeply"));
        }
        self.depth += 1;
        let result = self.value_inner();
        self.depth -= 1;
        result
    }

    fn value_inner(&mut self) -> Result<Value, GVariantError> {
        self.skip_ws();
        match self.peek() {
            None => Err(self.error("expected a value, found end of input")),
            Some('@') => {
                // A type annotation such as `@as` or `@a{sv}` is implied by the value
                // that follows, so the annotation is skipped.
                self.bump();
                if self.take_while(|c| !c.is_whitespace()).is_empty() {
                    return Err(self.error("expected a type after '@'"));
                }
                self.value()
            }
            Some('<') => {
                self.bump();
                let value = self.value()?;
                self.skip_ws();
                self.expect('>')?;
                Ok(value)
            }
            Some('\'' | '"') => self.string().map(Value::String),
            Some('[') => {
                self.expect('[')?;
                Ok(Value::Array(self.sequence(']', false)?))
            }
            Some('(') => {
                self.expect('(')?;
                Ok(Value::Array(self.sequence(')', true)?))
            }
            Some('{') => self.dict(),
            Some(c) if c == '-' || c.is_ascii_digit() => {
                let n = self.number()?;
                self.json_number(n)
            }
            Some(c) if c.is_ascii_alphabetic() => self.keyword(),
            Some(c) => Err(self.error(format!("unexpected character '{c}'"))),
        }
    }

    /// Parses `value, value, ...` up to `close`, which is consumed.
    fn sequence(&mut self, close: char, trailing_comma: bool) -> Result<Vec<Value>, GVariantError> {
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(close) {
            self.bump();
            return Ok(items);
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                    self.skip_ws();
                    if trailing_comma && self.peek() == Some(close) {
                        self.bump();
                        return Ok(items);
                    }
                }
                Some(c) if c == close => {
                    self.bump();
                    return Ok(items);
                }
                _ => return Err(self.error(format!("expected ',' or '{close}'"))),
            }
        }
    }

    /// Parses `{key: value, ...}`. Keys must be strings, and repeated keys are rejected.
    fn dict(&mut self) -> Result<Value, GVariantError> {
        self.expect('{')?;
        let mut map = serde_json::Map::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_ws();
            let key_offset = self.pos;
            let key = match self.value()? {
                Value::String(s) => s,
                _ => {
                    return Err(GVariantError {
                        offset: key_offset,
                        message: "dictionary keys must be strings".to_string(),
                    })
                }
            };
            if map.contains_key(&key) {
                return Err(GVariantError {
                    offset: key_offset,
                    message: format!("duplicate dictionary key '{key}'"),
                });
            }
            self.skip_ws();
            self.expect(':')?;
            let value = self.value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    return Ok(Value::Object(map));
                }
                _ => return Err(self.error("expected ',' or '}' in dictionary")),
            }
        }
    }

    /// Parses a quoted string. GVariant text uses single quotes; double quotes are also accepted.
    fn string(&mut self) -> Result<String, GVariantError> {
        let quote = self.bump().ok_or_else(|| self.error("expected a quote"))?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.error("unterminated string")),
                Some(c) if c == quote => return Ok(out),
                Some('\\') => {
                    let c = self.escape()?;
                    out.push(c);
                }
                Some(c) => out.push(c),
            }
        }
    }

    fn escape(&mut self) -> Result<char, GVariantError> {
        let c = self
            .bump()
            .ok_or_else(|| self.error("unterminated escape sequence"))?;
        let decoded = match c {
            '\\' | '\'' | '"' => c,
            'a' => '\u{7}',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\u{b}',
            'u' | 'U' => {
                let width = if c == 'u' { 4 } else { 8 };
                let mut code: u32 = 0;
                for _ in 0..width {
                    let digit = self
                        .bump()
                        .and_then(|d| d.to_digit(16))
                        .ok_or_else(|| self.error("expected hex digits in escape"))?;
                    code = code * 16 + digit;
                }
                char::from_u32(code).ok_or_else(|| self.error("escape is not a valid character"))?
            }
            _ => return Err(self.error(format!("unknown escape '\\{c}'"))),
        };
        Ok(decoded)
    }

    /// Parses a number: an optionally negative integer, a decimal or exponent form
    /// (a double), or a `0x` hex integer as GVariant prints bytes.
    fn number(&mut self) -> Result<Num, GVariantError> {
        let src: &'a str = self.src;
        let start = self.pos;
        if src[self.pos..].starts_with("0x") {
            self.pos += 2;
            let digits = self.take_while(|c| c.is_ascii_hexdigit());
            if digits.is_empty() {
                return Err(self.error("expected hex digits after '0x'"));
            }
            let value = u64::from_str_radix(digits, 16)
                .map_err(|_| self.error("hex number out of range"))?;
            return Ok(Num::Int(i128::from(value)));
        }
        if self.peek() == Some('-') {
            self.bump();
        }
        if self.take_while(|c| c.is_ascii_digit()).is_empty() {
            return Err(self.error("expected a number"));
        }
        let mut is_float = false;
        if self.peek() == Some('.') {
            is_float = true;
            self.bump();
            if self.take_while(|c| c.is_ascii_digit()).is_empty() {
                return Err(self.error("expected digits after '.'"));
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if self.take_while(|c| c.is_ascii_digit()).is_empty() {
                return Err(self.error("expected digits in exponent"));
            }
        }
        let text = &src[start..self.pos];
        if is_float {
            let f: f64 = text
                .parse()
                .map_err(|_| self.error("invalid floating-point number"))?;
            Ok(Num::Float(f))
        } else {
            let i: i128 = text
                .parse()
                .map_err(|_| self.error("integer out of range"))?;
            Ok(Num::Int(i))
        }
    }

    fn json_number(&self, n: Num) -> Result<Value, GVariantError> {
        match n {
            Num::Int(i) => {
                if let Ok(u) = u64::try_from(i) {
                    Ok(Value::from(u))
                } else if let Ok(s) = i64::try_from(i) {
                    Ok(Value::from(s))
                } else {
                    Err(self.error("integer out of range"))
                }
            }
            Num::Float(f) => serde_json::Number::from_f64(f)
                .map(Value::Number)
                .ok_or_else(|| self.error("floating-point number is not finite")),
        }
    }

    /// Parses `true`, `false`, or a typed number such as `uint32 7` or `double 1.5`.
    fn keyword(&mut self) -> Result<Value, GVariantError> {
        let start = self.pos;
        let word = self.take_while(|c| c.is_ascii_alphanumeric());
        match word {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "double" | "byte" | "int16" | "uint16" | "int32" | "uint32" | "int64" | "uint64"
            | "handle" => {
                self.skip_ws();
                let at = self.pos;
                let n = self.number()?;
                let typed = if word == "double" {
                    match n {
                        Num::Float(f) => Num::Float(f),
                        Num::Int(i) => Num::Float(i as f64),
                    }
                } else {
                    match n {
                        Num::Int(i) => {
                            let (min, max) = integer_range(word);
                            if i < min || i > max {
                                return Err(GVariantError {
                                    offset: at,
                                    message: format!("{i} is out of range for {word}"),
                                });
                            }
                            Num::Int(i)
                        }
                        Num::Float(_) => {
                            return Err(GVariantError {
                                offset: at,
                                message: format!("{word} needs an integer"),
                            })
                        }
                    }
                };
                self.json_number(typed)
            }
            other => Err(GVariantError {
                offset: start,
                message: format!("unsupported value '{other}'"),
            }),
        }
    }
}

/// Inclusive bounds for a GVariant integer type name.
fn integer_range(ty: &str) -> (i128, i128) {
    match ty {
        "byte" => (0, 255),
        "int16" => (i16::MIN.into(), i16::MAX.into()),
        "uint16" => (0, u16::MAX.into()),
        "int32" | "handle" => (i32::MIN.into(), i32::MAX.into()),
        "uint32" => (0, u32::MAX.into()),
        "int64" => (i64::MIN.into(), i64::MAX.into()),
        _ => (0, u64::MAX.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_escapes_decode() {
        assert_eq!(
            parse_gvariant_text(r"'it\'s\né'").unwrap(),
            Value::String("it's\n\u{e9}".to_string())
        );
    }

    #[test]
    fn errors_report_the_byte_offset() {
        let err = parse_gvariant_text("['a' 'b']").unwrap_err();
        assert_eq!(err.offset, 5);
    }
}
