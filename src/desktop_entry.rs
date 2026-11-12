//! Desktop Entry files: spec-correct read, write and Exec field-code expansion (#352).
//!
//! Everything here is a pure function over text. File access, atomic writes and
//! cache refresh belong to the applet layer. The rules follow the freedesktop
//! Desktop Entry Specification:
//!
//! - a file is kept as lines. A write changes only the key it was asked to change;
//!   comments, blank lines, unknown keys and key order survive a read and write.
//! - string values escape `\s`, `\n`, `\t`, `\r` and `\\`. List values are
//!   `;`-separated and a literal `;` inside an item is written `\;`.
//! - `Exec` is read after string unescaping. Arguments use double quotes with the
//!   backslash escapes `\"`, `` \` ``, `\$` and `\\`. Reserved characters must be quoted.
//!   Field codes are expanded per argument, and substituted text is never parsed again,
//!   so a file name always becomes exactly one argument.

use std::fmt;

/// The group that holds the entry's own keys.
pub const DESKTOP_ENTRY_GROUP: &str = "Desktop Entry";

/// Characters that must be quoted in an Exec argument (other than space and tab).
const RESERVED: &str = "\"'\\`$><~|&;*?#()";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopError {
    /// A line that is not blank, a comment, a group header or a key line.
    MalformedLine {
        line: usize,
        reason: &'static str,
    },
    InvalidKey(String),
    InvalidEscape(char),
    TrailingBackslash,
    ControlCharacter(char),
    UnterminatedQuote,
    TextAfterQuote,
    ReservedCharacter(char),
    InvalidExecEscape(char),
    TrailingPercent,
    UnknownFieldCode(char),
    /// %F, %U or %i used inside a larger argument.
    EmbeddedFieldCode(char),
    /// %f or %u with several items. The caller must launch once per item.
    SingleFieldCodeWithSeveralItems(char),
}

impl fmt::Display for DesktopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedLine { line, reason } => write!(f, "line {line}: {reason}"),
            Self::InvalidKey(key) => write!(f, "invalid key name: {key}"),
            Self::InvalidEscape(c) => write!(f, "invalid escape sequence \\{c}"),
            Self::TrailingBackslash => f.write_str("value ends with a backslash"),
            Self::ControlCharacter(c) => {
                write!(f, "control character {c:?} cannot be stored in a value")
            }
            Self::UnterminatedQuote => f.write_str("unterminated double quote in Exec"),
            Self::TextAfterQuote => f.write_str("text after a closing double quote in Exec"),
            Self::ReservedCharacter(c) => write!(f, "character {c:?} must be quoted in Exec"),
            Self::InvalidExecEscape(c) => write!(f, "invalid escape \\{c} inside Exec quotes"),
            Self::TrailingPercent => f.write_str("% at the end of Exec must be written %%"),
            Self::UnknownFieldCode(c) => write!(f, "unknown field code %{c} in Exec"),
            Self::EmbeddedFieldCode(c) => write!(f, "field code %{c} must be a whole argument"),
            Self::SingleFieldCodeWithSeveralItems(c) => {
                write!(f, "field code %{c} takes one item but several were given")
            }
        }
    }
}

impl std::error::Error for DesktopError {}

fn is_space_or_cr(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r')
}

fn trim_value(raw: &str) -> &str {
    raw.trim_matches(is_space_or_cr)
}

/// Escapes a string value for one line. A leading or trailing space becomes `\s`,
/// so the value cannot be trimmed on read. Control characters other than tab, LF
/// and CR are refused.
pub fn escape_string(value: &str) -> Result<String, DesktopError> {
    let count = value.chars().count();
    let mut out = String::with_capacity(value.len());
    for (index, c) in value.chars().enumerate() {
        if c == ' ' && (index == 0 || index + 1 == count) {
            out.push_str("\\s");
            continue;
        }
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => return Err(DesktopError::ControlCharacter(c)),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Reverses [`escape_string`]. An unknown escape is an error.
pub fn unescape_string(raw: &str) -> Result<String, DesktopError> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => return Err(DesktopError::InvalidEscape(other)),
            None => return Err(DesktopError::TrailingBackslash),
        }
    }
    Ok(out)
}

/// Splits a raw list value on unescaped `;` and unescapes each item. A trailing
/// `;` does not add an empty item.
pub fn split_list(raw: &str) -> Result<Vec<String>, DesktopError> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut chars = trim_value(raw).chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(';') => current.push(';'),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => return Err(DesktopError::TrailingBackslash),
            },
            ';' => items.push(unescape_string(&std::mem::take(&mut current))?),
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        items.push(unescape_string(&current)?);
    }
    Ok(items)
}

/// Joins items into a raw list value. Each item is followed by `;`, the form the
/// specification uses for lists.
pub fn join_list(items: &[&str]) -> Result<String, DesktopError> {
    let mut out = String::new();
    for item in items {
        out.push_str(&escape_string(item)?.replace(';', "\\;"));
        out.push(';');
    }
    Ok(out)
}

fn is_valid_group(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| matches!(c, ' '..='~') && c != '[' && c != ']')
}

/// A key is `Name` or `Name[locale]`. The name uses ASCII letters, digits and `-`.
/// The locale uses letters, digits and `_ @ . -`.
pub fn is_valid_key(key: &str) -> bool {
    let (base, locale) = match key.split_once('[') {
        Some((base, rest)) => match rest.strip_suffix(']') {
            Some(locale) => (base, Some(locale)),
            None => return false,
        },
        None => (key, None),
    };
    let base_ok = !base.is_empty() && base.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let locale_ok = locale.is_none_or(|l| {
        !l.is_empty()
            && l.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '.' | '-'))
    });
    base_ok && locale_ok
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind<'a> {
    Blank,
    Comment,
    Group(&'a str),
    Key { key: &'a str, value: &'a str },
}

fn classify(line: &str) -> Result<Kind<'_>, &'static str> {
    let text = line.trim_matches(is_space_or_cr);
    if text.is_empty() {
        return Ok(Kind::Blank);
    }
    if text.starts_with('#') {
        return Ok(Kind::Comment);
    }
    if let Some(rest) = text.strip_prefix('[') {
        let name = rest
            .strip_suffix(']')
            .ok_or("group header lacks a closing ']'")?;
        return if is_valid_group(name) {
            Ok(Kind::Group(name))
        } else {
            Err("invalid group name")
        };
    }
    let (key, value) = text
        .split_once('=')
        .ok_or("line is neither a key=value pair nor a group header")?;
    let key = key.trim_end_matches(is_space_or_cr);
    if !is_valid_key(key) {
        return Err("invalid key name");
    }
    Ok(Kind::Key {
        key,
        value: trim_value(value),
    })
}

/// Classifies every line. Keys must come after a group header.
fn scan(text: &str) -> Result<Vec<Kind<'_>>, DesktopError> {
    let mut kinds = Vec::new();
    let mut in_group = false;
    for (index, line) in text.split('\n').enumerate() {
        let kind = classify(line).map_err(|reason| DesktopError::MalformedLine {
            line: index + 1,
            reason,
        })?;
        match kind {
            Kind::Group(_) => in_group = true,
            Kind::Key { .. } if !in_group => {
                return Err(DesktopError::MalformedLine {
                    line: index + 1,
                    reason: "key appears before any group header",
                })
            }
            _ => {}
        }
        kinds.push(kind);
    }
    Ok(kinds)
}

/// Returns the header index and the index one past the group's last line.
fn entry_group(kinds: &[Kind<'_>]) -> Option<(usize, usize)> {
    let header = kinds
        .iter()
        .position(|kind| matches!(kind, Kind::Group(name) if *name == DESKTOP_ENTRY_GROUP))?;
    let end = kinds
        .iter()
        .enumerate()
        .skip(header + 1)
        .find(|(_, kind)| matches!(kind, Kind::Group(_)))
        .map_or(kinds.len(), |(index, _)| index);
    Some((header, end))
}

/// Line indexes of every `key` line in the Desktop Entry group.
fn key_lines(kinds: &[Kind<'_>], key: &str) -> Vec<usize> {
    let Some((header, end)) = entry_group(kinds) else {
        return Vec::new();
    };
    (header + 1..end)
        .filter(|&index| matches!(kinds.get(index), Some(Kind::Key { key: k, .. }) if *k == key))
        .collect()
}

/// The raw (still escaped, trimmed) value of the first matching key.
fn first_value<'a>(kinds: &[Kind<'a>], key: &str) -> Option<&'a str> {
    let index = *key_lines(kinds, key).first()?;
    match kinds.get(index) {
        Some(Kind::Key { value, .. }) => Some(*value),
        _ => None,
    }
}

/// Reads a string value from the Desktop Entry group, unescaped.
pub fn get_string(text: &str, key: &str) -> Result<Option<String>, DesktopError> {
    let kinds = scan(text)?;
    first_value(&kinds, key).map(unescape_string).transpose()
}

/// Reads a list value from the Desktop Entry group.
pub fn get_list(text: &str, key: &str) -> Result<Option<Vec<String>>, DesktopError> {
    let kinds = scan(text)?;
    first_value(&kinds, key).map(split_list).transpose()
}

/// Checks the syntax of every line.
pub fn validate(text: &str) -> Result<(), DesktopError> {
    scan(text).map(|_| ())
}

/// Sets a key to an already encoded value. Returns `None` when the stored value
/// already has the requested meaning. Otherwise it replaces every matching line,
/// or inserts the key after the group's last entry (creating the group first if
/// the text has none).
fn set_encoded(
    text: &str,
    key: &str,
    encoded: &str,
    same: impl Fn(&str) -> bool,
) -> Result<Option<String>, DesktopError> {
    if !is_valid_key(key) {
        return Err(DesktopError::InvalidKey(key.to_string()));
    }
    let kinds = scan(text)?;
    let new_line = format!("{key}={encoded}");
    let found = key_lines(&kinds, key);

    if found.is_empty() {
        let Some((header, end)) = entry_group(&kinds) else {
            return Ok(Some(format!("[{DESKTOP_ENTRY_GROUP}]\n{new_line}\n{text}")));
        };
        let at = (header + 1..end)
            .rev()
            .find(|&index| !matches!(kinds.get(index), Some(Kind::Blank)))
            .map_or(header + 1, |index| index + 1);
        let mut out: Vec<String> = text.split('\n').map(str::to_string).collect();
        out.insert(at, new_line);
        return Ok(Some(out.join("\n")));
    }

    let unchanged = found
        .iter()
        .all(|&index| matches!(kinds.get(index), Some(Kind::Key { value, .. }) if same(trim_value(value))));
    if unchanged {
        return Ok(None);
    }
    let mut out: Vec<String> = text.split('\n').map(str::to_string).collect();
    for &index in &found {
        if let Some(slot) = out.get_mut(index) {
            *slot = new_line.clone();
        }
    }
    Ok(Some(out.join("\n")))
}

/// Sets a string value in the Desktop Entry group. `None` means nothing changed.
pub fn set_string(text: &str, key: &str, value: &str) -> Result<Option<String>, DesktopError> {
    let encoded = escape_string(value)?;
    set_encoded(text, key, &encoded, |raw| {
        unescape_string(raw).is_ok_and(|v| v == value)
    })
}

/// Sets a list value in the Desktop Entry group. `None` means nothing changed.
pub fn set_list(text: &str, key: &str, items: &[&str]) -> Result<Option<String>, DesktopError> {
    let encoded = join_list(items)?;
    set_encoded(text, key, &encoded, |raw| {
        split_list(raw)
            .is_ok_and(|values| values.iter().map(String::as_str).eq(items.iter().copied()))
    })
}

/// Removes every line that sets `key` in the Desktop Entry group. `None` means the
/// key was absent.
pub fn remove_key(text: &str, key: &str) -> Result<Option<String>, DesktopError> {
    if !is_valid_key(key) {
        return Err(DesktopError::InvalidKey(key.to_string()));
    }
    let kinds = scan(text)?;
    let found = key_lines(&kinds, key);
    if found.is_empty() {
        return Ok(None);
    }
    let kept: Vec<&str> = text
        .split('\n')
        .enumerate()
        .filter(|(index, _)| !found.contains(index))
        .map(|(_, line)| line)
        .collect();
    Ok(Some(kept.join("\n")))
}

/// One piece of an Exec argument after quote handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    /// The letter of a field code, or a deprecated code that expands to nothing.
    Code(char),
}

/// One Exec argument: text pieces and field codes.
pub type Arg = Vec<Piece>;

fn push_char(arg: &mut Arg, c: char) {
    if let Some(Piece::Text(text)) = arg.last_mut() {
        text.push(c);
    } else {
        arg.push(Piece::Text(c.to_string()));
    }
}

fn field_code(next: Option<char>, arg: &mut Arg) -> Result<(), DesktopError> {
    match next {
        Some('%') => push_char(arg, '%'),
        Some(c) if "fFuUickdDnNvm".contains(c) => arg.push(Piece::Code(c)),
        Some(c) => return Err(DesktopError::UnknownFieldCode(c)),
        None => return Err(DesktopError::TrailingPercent),
    }
    Ok(())
}

/// Parses an Exec value (already string-unescaped) into arguments. Arguments are
/// separated by spaces or tabs. An argument is either wholly double-quoted or free
/// of reserved characters.
pub fn parse_exec(exec: &str) -> Result<Vec<Arg>, DesktopError> {
    let mut args = Vec::new();
    let mut chars = exec.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(' ' | '\t')) {
            chars.next();
        }
        let Some(&first) = chars.peek() else {
            break;
        };
        let mut arg = Arg::new();
        if first == '"' {
            chars.next();
            loop {
                match chars.next() {
                    None => return Err(DesktopError::UnterminatedQuote),
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(c @ ('"' | '`' | '$' | '\\')) => push_char(&mut arg, c),
                        Some(other) => return Err(DesktopError::InvalidExecEscape(other)),
                        None => return Err(DesktopError::UnterminatedQuote),
                    },
                    Some(c @ ('`' | '$')) => return Err(DesktopError::ReservedCharacter(c)),
                    Some('%') => field_code(chars.next(), &mut arg)?,
                    Some(c) => push_char(&mut arg, c),
                }
            }
            if !matches!(chars.peek(), None | Some(' ' | '\t')) {
                return Err(DesktopError::TextAfterQuote);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ' ' || c == '\t' {
                    break;
                }
                chars.next();
                match c {
                    '%' => field_code(chars.next(), &mut arg)?,
                    c if RESERVED.contains(c) => return Err(DesktopError::ReservedCharacter(c)),
                    c => push_char(&mut arg, c),
                }
            }
        }
        args.push(arg);
    }
    Ok(args)
}

/// What a field code expands to. Each field is substituted as one value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldContext<'a> {
    /// Local file names for %f and %F.
    pub files: &'a [&'a str],
    /// URLs for %u and %U.
    pub urls: &'a [&'a str],
    /// Value of the Icon key for %i.
    pub icon: Option<&'a str>,
    /// Translated name for %c.
    pub name: Option<&'a str>,
    /// Location of the desktop file for %k.
    pub desktop_file: Option<&'a str>,
}

fn expand_code(code: char, ctx: &FieldContext<'_>, out: &mut String) -> Result<(), DesktopError> {
    match code {
        'f' | 'u' => {
            let items = if code == 'f' { ctx.files } else { ctx.urls };
            match items {
                [] => {}
                [one] => out.push_str(one),
                _ => return Err(DesktopError::SingleFieldCodeWithSeveralItems(code)),
            }
        }
        'c' => {
            if let Some(name) = ctx.name {
                out.push_str(name);
            }
        }
        'k' => {
            if let Some(path) = ctx.desktop_file {
                out.push_str(path);
            }
        }
        'F' | 'U' | 'i' => return Err(DesktopError::EmbeddedFieldCode(code)),
        'd' | 'D' | 'n' | 'N' | 'v' | 'm' => {}
        other => return Err(DesktopError::UnknownFieldCode(other)),
    }
    Ok(())
}

/// Expands field codes into the final argument vector. Substituted text is appended
/// to the argument and never scanned again. An argument that was only a code with
/// nothing to substitute is dropped. A standalone %F or %U becomes one argument per
/// item. A standalone %i becomes `--icon` and the icon name.
pub fn expand_exec(args: &[Arg], ctx: &FieldContext<'_>) -> Result<Vec<String>, DesktopError> {
    let mut argv = Vec::new();
    for arg in args {
        match arg.as_slice() {
            [Piece::Code(code @ ('F' | 'U'))] => {
                let items = if *code == 'F' { ctx.files } else { ctx.urls };
                argv.extend(items.iter().map(|item| item.to_string()));
                continue;
            }
            [Piece::Code('i')] => {
                if let Some(icon) = ctx.icon {
                    argv.push("--icon".to_string());
                    argv.push(icon.to_string());
                }
                continue;
            }
            _ => {}
        }
        let mut out = String::new();
        let mut had_code = false;
        for piece in arg {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::Code(code) => {
                    had_code = true;
                    expand_code(*code, ctx, &mut out)?;
                }
            }
        }
        if !had_code || !out.is_empty() {
            argv.push(out);
        }
    }
    Ok(argv)
}

/// Builds an Exec value (before string escaping) from an argument vector. Arguments
/// with spaces or reserved characters are double-quoted. `%` becomes `%%`.
pub fn exec_text(argv: &[&str]) -> String {
    let mut out = String::new();
    for (index, arg) in argv.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let quote = arg.is_empty()
            || arg
                .chars()
                .any(|c| c.is_whitespace() || RESERVED.contains(c));
        if quote {
            out.push('"');
        }
        for c in arg.chars() {
            match c {
                '"' | '`' | '$' | '\\' => {
                    out.push('\\');
                    out.push(c);
                }
                '%' => out.push_str("%%"),
                c => out.push(c),
            }
        }
        if quote {
            out.push('"');
        }
    }
    out
}

/// Reads the Exec key of the Desktop Entry group and expands it. `None` when there
/// is no Exec key.
pub fn exec_argv(text: &str, ctx: &FieldContext<'_>) -> Result<Option<Vec<String>>, DesktopError> {
    match get_string(text, "Exec")? {
        Some(exec) => expand_exec(&parse_exec(&exec)?, ctx).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "\
# Written by hand
[Desktop Entry]
Version=1.0
Type=Application
Name=Example Web App
Name[de]=Beispiel-Webapp
Comment=Launch the example
Comment[de]=Die Beispiel-App starten
Exec=\"/opt/My Apps/webapp\" --app=https://example.com/ %U
Icon=example
Terminal=false
MimeType=text/html;x-scheme-handler/https;
Categories=Network;
X-Custom=keep me

[Desktop Action New]
Name=New Window
Exec=webapp --new
";

    #[test]
    fn string_escapes_round_trip() {
        let value = "  tab\there\nnew\r\\ back  ";
        let raw = escape_string(value).unwrap();
        assert!(!raw.contains('\n') && !raw.contains('\t') && !raw.contains('\r'));
        assert!(raw.starts_with("\\s"));
        assert!(raw.ends_with("\\s"));
        assert_eq!(unescape_string(&raw).unwrap(), value);
    }

    #[test]
    fn newline_in_a_value_cannot_start_a_new_line() {
        let raw = escape_string("x\nExec=/bin/evil").unwrap();
        assert_eq!(raw, "x\\nExec=/bin/evil");
    }

    #[test]
    fn percent_is_stored_as_is_and_control_characters_are_refused() {
        assert_eq!(escape_string("100%").unwrap(), "100%");
        assert_eq!(
            escape_string("a\u{1b}b"),
            Err(DesktopError::ControlCharacter('\u{1b}'))
        );
    }

    #[test]
    fn unknown_or_trailing_escapes_are_errors() {
        assert_eq!(
            unescape_string("a\\q"),
            Err(DesktopError::InvalidEscape('q'))
        );
        assert_eq!(unescape_string("a\\"), Err(DesktopError::TrailingBackslash));
    }

    #[test]
    fn list_items_split_on_unescaped_semicolons() {
        assert_eq!(split_list("a;b\\;c;").unwrap(), vec!["a", "b;c"]);
        assert_eq!(split_list("a;;b").unwrap(), vec!["a", "", "b"]);
        assert_eq!(split_list("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn list_items_join_and_split_back() {
        let raw = join_list(&["a;b", "c\\d"]).unwrap();
        assert_eq!(raw, "a\\;b;c\\\\d;");
        assert_eq!(split_list(&raw).unwrap(), vec!["a;b", "c\\d"]);
    }

    #[test]
    fn invalid_key_names_are_refused() {
        assert!(is_valid_key("Name[de]"));
        assert!(is_valid_key("X-Custom"));
        assert!(!is_valid_key("Bad Key"));
        assert!(!is_valid_key("Name[de"));
        assert!(!is_valid_key("[de]"));
        assert!(matches!(
            set_string(EXAMPLE, "Bad Key", "x"),
            Err(DesktopError::InvalidKey(_))
        ));
    }

    #[test]
    fn malformed_lines_report_their_number() {
        assert!(matches!(
            validate("[Desktop Entry]\nBad line\n"),
            Err(DesktopError::MalformedLine { line: 2, .. })
        ));
        assert!(matches!(
            validate("Name=x\n"),
            Err(DesktopError::MalformedLine { line: 1, .. })
        ));
        assert!(validate(EXAMPLE).is_ok());
    }

    #[test]
    fn set_changes_only_the_asked_line() {
        let out = set_string(EXAMPLE, "Name", "Renamed").unwrap().unwrap();
        assert_eq!(
            out,
            EXAMPLE.replace("Name=Example Web App\n", "Name=Renamed\n")
        );
    }

    #[test]
    fn set_returns_none_when_the_value_is_unchanged() {
        assert_eq!(
            set_string(EXAMPLE, "Name", "Example Web App").unwrap(),
            None
        );
        assert_eq!(
            set_list(
                EXAMPLE,
                "MimeType",
                &["text/html", "x-scheme-handler/https"]
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn set_list_writes_the_list_form() {
        let out = set_list(EXAMPLE, "Categories", &["Network", "Utility"])
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            EXAMPLE.replace("Categories=Network;\n", "Categories=Network;Utility;\n")
        );
    }

    #[test]
    fn new_key_goes_after_the_last_entry_of_the_group() {
        let out = set_string(EXAMPLE, "StartupWMClass", "example")
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            EXAMPLE.replace(
                "X-Custom=keep me\n",
                "X-Custom=keep me\nStartupWMClass=example\n"
            )
        );
    }

    #[test]
    fn missing_group_is_created_at_the_top() {
        assert_eq!(
            set_string("", "Name", "X").unwrap(),
            Some("[Desktop Entry]\nName=X\n".to_string())
        );
    }

    #[test]
    fn remove_deletes_only_the_desktop_entry_line() {
        let out = remove_key(EXAMPLE, "Exec").unwrap().unwrap();
        assert!(!out.contains("%U"));
        assert!(out.contains("Exec=webapp --new"));
        assert_eq!(out.lines().count(), EXAMPLE.lines().count() - 1);
        assert_eq!(remove_key(EXAMPLE, "Missing").unwrap(), None);
        assert_eq!(
            remove_key(EXAMPLE, "X-Custom").unwrap().unwrap(),
            EXAMPLE.replace("X-Custom=keep me\n", "")
        );
    }

    #[test]
    fn exec_parses_quotes_and_escapes() {
        let args = parse_exec(r#""/opt/My Apps/webapp" "--name=a\"b" "say \$x" plain"#).unwrap();
        let argv = expand_exec(&args, &FieldContext::default()).unwrap();
        assert_eq!(
            argv,
            vec!["/opt/My Apps/webapp", "--name=a\"b", "say $x", "plain"]
        );
    }

    #[test]
    fn exec_parse_errors() {
        assert_eq!(
            parse_exec("a\"b"),
            Err(DesktopError::ReservedCharacter('"'))
        );
        assert_eq!(parse_exec("\"open"), Err(DesktopError::UnterminatedQuote));
        assert_eq!(parse_exec("\"a\"b"), Err(DesktopError::TextAfterQuote));
        assert_eq!(
            parse_exec("\"a$b\""),
            Err(DesktopError::ReservedCharacter('$'))
        );
        assert_eq!(
            parse_exec("\"a`b\""),
            Err(DesktopError::ReservedCharacter('`'))
        );
        assert_eq!(
            parse_exec("\"a\\q\""),
            Err(DesktopError::InvalidExecEscape('q'))
        );
        assert_eq!(parse_exec("a|b"), Err(DesktopError::ReservedCharacter('|')));
        assert_eq!(
            parse_exec("app %x"),
            Err(DesktopError::UnknownFieldCode('x'))
        );
        assert_eq!(parse_exec("app 100%"), Err(DesktopError::TrailingPercent));
    }

    #[test]
    fn exec_percent_percent_is_a_literal_percent() {
        let args = parse_exec("echo 100%%").unwrap();
        assert_eq!(
            expand_exec(&args, &FieldContext::default()).unwrap(),
            vec!["echo", "100%"]
        );
    }

    #[test]
    fn single_file_and_url_codes_are_one_argument_each() {
        let args = parse_exec("viewer %f").unwrap();
        let ctx = FieldContext {
            files: &["/home/u/a b.txt"],
            ..Default::default()
        };
        assert_eq!(
            expand_exec(&args, &ctx).unwrap(),
            vec!["viewer", "/home/u/a b.txt"]
        );

        let args = parse_exec("viewer %u").unwrap();
        let ctx = FieldContext {
            urls: &["https://example.com/a b"],
            ..Default::default()
        };
        assert_eq!(
            expand_exec(&args, &ctx).unwrap(),
            vec!["viewer", "https://example.com/a b"]
        );
    }

    #[test]
    fn list_codes_expand_to_one_argument_per_item() {
        let args = parse_exec("app %F").unwrap();
        let ctx = FieldContext {
            files: &["a", "b c"],
            ..Default::default()
        };
        assert_eq!(expand_exec(&args, &ctx).unwrap(), vec!["app", "a", "b c"]);

        let args = parse_exec("app %U").unwrap();
        let ctx = FieldContext {
            urls: &["https://x/1", "https://x/2"],
            ..Default::default()
        };
        assert_eq!(
            expand_exec(&args, &ctx).unwrap(),
            vec!["app", "https://x/1", "https://x/2"]
        );
        assert_eq!(
            expand_exec(&args, &FieldContext::default()).unwrap(),
            vec!["app"]
        );
    }

    #[test]
    fn icon_name_and_desktop_path_codes() {
        let args = parse_exec("app %i %c %k").unwrap();
        let ctx = FieldContext {
            icon: Some("foo"),
            name: Some("My App"),
            desktop_file: Some("/usr/share/applications/x.desktop"),
            ..Default::default()
        };
        assert_eq!(
            expand_exec(&args, &ctx).unwrap(),
            vec![
                "app",
                "--icon",
                "foo",
                "My App",
                "/usr/share/applications/x.desktop"
            ]
        );
        assert_eq!(
            expand_exec(&args, &FieldContext::default()).unwrap(),
            vec!["app"]
        );
    }

    #[test]
    fn deprecated_codes_are_removed() {
        let args = parse_exec("app %d %D %n %N %v %m --x=%m").unwrap();
        assert_eq!(
            expand_exec(&args, &FieldContext::default()).unwrap(),
            vec!["app", "--x="]
        );
    }

    #[test]
    fn list_and_icon_codes_must_be_whole_arguments() {
        let args = parse_exec("app --x=%F").unwrap();
        assert_eq!(
            expand_exec(&args, &FieldContext::default()),
            Err(DesktopError::EmbeddedFieldCode('F'))
        );
        let args = parse_exec("app %f").unwrap();
        let ctx = FieldContext {
            files: &["a", "b"],
            ..Default::default()
        };
        assert_eq!(
            expand_exec(&args, &ctx),
            Err(DesktopError::SingleFieldCodeWithSeveralItems('f'))
        );
    }

    #[test]
    fn substituted_names_stay_exactly_one_argument() {
        let nasty = [
            "a b",
            "$HOME",
            "`id`",
            "x;rm -rf /",
            "say \"hi\"",
            "%U",
            "%f",
            "back\\slash",
            "--flag=evil",
            "tab\tsep",
            "quote'and*glob?",
        ];
        for name in nasty {
            let args = parse_exec("viewer %f").unwrap();
            let ctx = FieldContext {
                files: &[name],
                ..Default::default()
            };
            assert_eq!(
                expand_exec(&args, &ctx).unwrap(),
                vec!["viewer".to_string(), name.to_string()]
            );

            let args = parse_exec("viewer --file=%f").unwrap();
            assert_eq!(
                expand_exec(&args, &ctx).unwrap(),
                vec!["viewer".to_string(), format!("--file={name}")]
            );
        }
        let all: Vec<&str> = nasty.to_vec();
        let args = parse_exec("viewer %F").unwrap();
        let ctx = FieldContext {
            files: &all,
            ..Default::default()
        };
        let mut expected = vec!["viewer".to_string()];
        expected.extend(all.iter().map(|s| s.to_string()));
        assert_eq!(expand_exec(&args, &ctx).unwrap(), expected);
    }

    #[test]
    fn exec_text_round_trips_through_the_file() {
        let argv = [
            "/opt/My Apps/webapp",
            "--app=https://example.com/?q=a b",
            "%f",
            "",
            "say \"hi\" $x `y` \\",
            "line\nExec=/bin/evil",
            "semi;colon",
        ];
        let text = set_string(EXAMPLE, "Exec", &exec_text(&argv))
            .unwrap()
            .unwrap();
        // The main group and the action group each keep one Exec line.
        assert_eq!(text.lines().filter(|l| l.starts_with("Exec=")).count(), 2);
        let back = exec_argv(&text, &FieldContext::default()).unwrap().unwrap();
        assert_eq!(back, argv.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    }

    #[test]
    fn realistic_entry_round_trips_with_localized_keys() {
        assert_eq!(
            get_string(EXAMPLE, "Name[de]").unwrap().as_deref(),
            Some("Beispiel-Webapp")
        );
        assert_eq!(
            get_string(EXAMPLE, "Name").unwrap().as_deref(),
            Some("Example Web App")
        );
        assert_eq!(
            get_list(EXAMPLE, "MimeType").unwrap(),
            Some(vec![
                "text/html".to_string(),
                "x-scheme-handler/https".to_string()
            ])
        );
        let ctx = FieldContext {
            urls: &["https://example.com/a b"],
            ..Default::default()
        };
        assert_eq!(
            exec_argv(EXAMPLE, &ctx).unwrap().unwrap(),
            vec![
                "/opt/My Apps/webapp",
                "--app=https://example.com/",
                "https://example.com/a b"
            ]
        );
        assert_eq!(
            set_string(EXAMPLE, "Name[de]", "Beispiel-Webapp").unwrap(),
            None
        );
        let out = set_string(EXAMPLE, "Name[de]", "Neue App")
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            EXAMPLE.replace("Name[de]=Beispiel-Webapp\n", "Name[de]=Neue App\n")
        );
    }
}
