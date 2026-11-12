//! Shell-quoted environment export lines (issue #404).

/// Why a variable cannot be written as a shell assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// The name is empty or is not a POSIX shell identifier.
    InvalidName(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::InvalidName(name) => write!(f, "invalid shell variable name: {name:?}"),
        }
    }
}

/// True when `name` is a POSIX shell identifier: an ASCII letter or underscore,
/// then ASCII letters, digits or underscores.
pub fn is_shell_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Quote `value` as one POSIX sh single-quoted word. Inside single quotes no
/// character is special, so `$`, backticks, backslashes, double quotes and
/// newlines pass through literally. An embedded single quote is written as
/// `'\''` (close, escaped quote, reopen).
pub fn sh_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// One `export NAME='value'` line for bash and zsh, without a trailing newline.
pub fn bash_export_line(name: &str, value: &str) -> Result<String, ExportError> {
    if !is_shell_name(name) {
        return Err(ExportError::InvalidName(name.to_string()));
    }
    Ok(format!("export {name}={}", sh_single_quote(value)))
}

/// One export line per `(name, value)` pair, each terminated by a newline.
pub fn bash_export_lines<'a, I>(pairs: I) -> Result<String, ExportError>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut out = String::new();
    for (name, value) in pairs {
        out.push_str(&bash_export_line(name, value)?);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_spaces_and_metacharacters() {
        assert_eq!(sh_single_quote("a b"), "'a b'");
        assert_eq!(sh_single_quote("say \"hi\""), "'say \"hi\"'");
        assert_eq!(sh_single_quote("$(id) `x` \\"), "'$(id) `x` \\'");
        assert_eq!(sh_single_quote(""), "''");
    }

    #[test]
    fn escapes_single_quotes_and_keeps_newlines() {
        assert_eq!(sh_single_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_single_quote("l1\nl2"), "'l1\nl2'");
    }

    #[test]
    fn validates_names() {
        assert!(is_shell_name("LANG"));
        assert!(is_shell_name("_x1"));
        assert!(!is_shell_name(""));
        assert!(!is_shell_name("1X"));
        assert!(!is_shell_name("A B"));
        assert!(!is_shell_name("A=B"));
    }
}
