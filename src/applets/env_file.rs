//! `env.load`: read `KEY=VALUE` environment files without evaluating them.
//!
//! The file is parsed, never run. A line may begin with `export`, and blank and
//! `#` comment lines are skipped. A value is unquoted, single-quoted (literal), or
//! double-quoted with `\\`, `\"`, `\$` and `` \` `` escapes. Anything that a shell
//! would expand or run is refused with its line number: `$`, backquotes, and the
//! other shell metacharacters outside quotes. Errors name a line, never a value.
//! Duplicate names are refused rather than resolved by order.

use super::{invalid, AppletError};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// Parse the bytes of an environment file into a JSON object of text values.
pub(super) fn load(input: &[u8]) -> Result<Value, AppletError> {
    let input = crate::structured::strip_utf8_bom(input);
    let text =
        std::str::from_utf8(input).map_err(|_| invalid("environment file is not UTF-8 text"))?;
    parse(text).map(Value::Object).map_err(invalid)
}

/// Characters that a shell would expand, split, or interpret outside quotes.
const UNQUOTED_REFUSED: &[char] = &[
    '$', '`', '\\', '"', '\'', '(', ')', ';', '&', '|', '<', '>', '*', '?', '[', ']', '{', '}',
    '~', '!', '#',
];

fn parse(text: &str) -> Result<Map<String, Value>, String> {
    let mut values = Map::new();
    let mut seen = BTreeSet::new();
    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = strip_export(line);
        let (name, rest) = line
            .split_once('=')
            .ok_or_else(|| format!("line {line_number}: expected NAME=VALUE"))?;
        if !is_name(name) {
            return Err(format!("line {line_number}: invalid name"));
        }
        if !seen.insert(name.to_owned()) {
            return Err(format!("line {line_number}: name is set more than once"));
        }
        let value = parse_value(rest, line_number)?;
        values.insert(name.to_owned(), Value::String(value));
    }
    Ok(values)
}

fn strip_export(line: &str) -> &str {
    match line.strip_prefix("export") {
        Some(rest) if rest.starts_with([' ', '\t']) => rest.trim_start(),
        _ => line,
    }
}

fn is_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && characters.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The value after `=`, with the line's trailing comment and whitespace checked.
fn parse_value(rest: &str, line: usize) -> Result<String, String> {
    if let Some(body) = rest.strip_prefix('\'') {
        let end = body
            .find('\'')
            .ok_or_else(|| format!("line {line}: unterminated single quote"))?;
        require_end(&body[end + 1..], line)?;
        return Ok(body[..end].to_owned());
    }
    if let Some(body) = rest.strip_prefix('"') {
        return parse_double_quoted(body, line);
    }
    // An unquoted value ends at whitespace. A `#` inside it is refused, not treated
    // as a comment, so no part of the value is dropped without notice.
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (value, tail) = rest.split_at(end);
    if value.chars().any(|c| UNQUOTED_REFUSED.contains(&c)) {
        return Err(format!(
            "line {line}: unquoted value uses shell syntax; quote it or remove the expansion"
        ));
    }
    require_end(tail, line)?;
    Ok(value.to_owned())
}

fn parse_double_quoted(body: &str, line: usize) -> Result<String, String> {
    let mut value = String::new();
    let mut characters = body.char_indices();
    while let Some((offset, character)) = characters.next() {
        match character {
            '"' => {
                require_end(&body[offset + 1..], line)?;
                return Ok(value);
            }
            '\\' => match characters.next() {
                Some((_, escaped @ ('\\' | '"' | '$' | '`'))) => value.push(escaped),
                _ => return Err(format!("line {line}: unsupported escape in double quotes")),
            },
            '$' | '`' => {
                return Err(format!(
                    "line {line}: expansion in double quotes is not evaluated; use single quotes"
                ))
            }
            other => value.push(other),
        }
    }
    Err(format!("line {line}: unterminated double quote"))
}

/// After a value, only whitespace or a `#` comment may follow.
fn require_end(tail: &str, line: usize) -> Result<(), String> {
    let trimmed = tail.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        Ok(())
    } else {
        Err(format!("line {line}: unexpected text after the value"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Map<String, Value> {
        parse(text).unwrap_or_else(|error| panic!("{error}"))
    }

    fn refused(text: &str) -> String {
        parse(text).expect_err("the file must be refused")
    }

    #[test]
    fn plain_quoted_and_exported_values_load_without_evaluation() {
        let values = parsed(
            "# comment\n\nNAME=arch\nexport ID=\"arch linux\"\nPRETTY='Arch $HOME literal'\nEMPTY=\nTRAIL=x # note\n",
        );
        assert_eq!(values["NAME"], "arch");
        assert_eq!(values["ID"], "arch linux");
        assert_eq!(values["PRETTY"], "Arch $HOME literal");
        assert_eq!(values["EMPTY"], "");
        assert_eq!(values["TRAIL"], "x");
    }

    #[test]
    fn double_quotes_keep_only_the_four_escapes() {
        let values = parsed("A=\"say \\\"hi\\\" \\\\ \\$ \\`\"\n");
        assert_eq!(values["A"], "say \"hi\" \\ $ `");
    }

    #[test]
    fn expansions_and_commands_are_refused_with_the_line_number() {
        assert!(refused("A=1\nB=$(id)\n").starts_with("line 2:"));
        assert!(refused("B=\"$HOME\"\n").starts_with("line 1:"));
        assert!(refused("B=`id`\n").starts_with("line 1:"));
        assert!(refused("B=x;rm\n").starts_with("line 1:"));
        assert!(refused("B=a b\n").starts_with("line 1:"));
        assert!(refused("B=\"open\n").starts_with("line 1:"));
        assert!(refused("B='open\n").starts_with("line 1:"));
    }

    #[test]
    fn duplicate_and_invalid_names_are_refused_without_their_values() {
        assert!(refused("A=1\nA=2\n").starts_with("line 2:"));
        let message = refused("9X=secret-token\n");
        assert_eq!(message, "line 1: invalid name");
        assert!(!message.contains("secret-token"));
    }

    #[test]
    fn non_utf8_input_is_refused() {
        assert!(load(&[0xff, 0xfe]).is_err());
    }
}
