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
    InvalidGroup(String),
    InvalidMimeType(String),
    InvalidDesktopId(String),
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
            Self::InvalidGroup(group) => write!(f, "invalid group name: {group:?}"),
            Self::InvalidMimeType(mime) => write!(f, "invalid MIME type: {mime:?}"),
            Self::InvalidDesktopId(id) => write!(f, "invalid desktop file id: {id:?}"),
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

fn classify(line: &str, key_ok: fn(&str) -> bool) -> Result<Kind<'_>, &'static str> {
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
    if !key_ok(key) {
        return Err("invalid key name");
    }
    Ok(Kind::Key {
        key,
        value: trim_value(value),
    })
}

/// Classifies every line, checking keys with `key_ok`. Keys must come after a group header.
fn scan_with(text: &str, key_ok: fn(&str) -> bool) -> Result<Vec<Kind<'_>>, DesktopError> {
    let mut kinds = Vec::new();
    let mut in_group = false;
    for (index, line) in text.split('\n').enumerate() {
        let kind = classify(line, key_ok).map_err(|reason| DesktopError::MalformedLine {
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

/// Classifies every line of a Desktop Entry file. Keys must come after a group header.
fn scan(text: &str) -> Result<Vec<Kind<'_>>, DesktopError> {
    scan_with(text, is_valid_key)
}

/// Rejects a group name that the specification does not allow.
fn check_group(group: &str) -> Result<(), DesktopError> {
    if is_valid_group(group) {
        Ok(())
    } else {
        Err(DesktopError::InvalidGroup(group.to_string()))
    }
}

/// Returns the header index and the index one past the group's last line.
fn group_range(kinds: &[Kind<'_>], group: &str) -> Option<(usize, usize)> {
    let header = kinds
        .iter()
        .position(|kind| matches!(kind, Kind::Group(name) if *name == group))?;
    let end = kinds
        .iter()
        .enumerate()
        .skip(header + 1)
        .find(|(_, kind)| matches!(kind, Kind::Group(_)))
        .map_or(kinds.len(), |(index, _)| index);
    Some((header, end))
}

/// Line indexes of every `key` line in `group`.
fn key_lines(kinds: &[Kind<'_>], group: &str, key: &str) -> Vec<usize> {
    let Some((header, end)) = group_range(kinds, group) else {
        return Vec::new();
    };
    (header + 1..end)
        .filter(|&index| matches!(kinds.get(index), Some(Kind::Key { key: k, .. }) if *k == key))
        .collect()
}

/// The raw (still escaped, trimmed) value of the first matching key in `group`.
fn first_value<'a>(kinds: &[Kind<'a>], group: &str, key: &str) -> Option<&'a str> {
    let index = *key_lines(kinds, group, key).first()?;
    match kinds.get(index) {
        Some(Kind::Key { value, .. }) => Some(*value),
        _ => None,
    }
}

/// Reads a string value from the Desktop Entry group, unescaped.
pub fn get_string(text: &str, key: &str) -> Result<Option<String>, DesktopError> {
    get_string_in(text, DESKTOP_ENTRY_GROUP, key)
}

/// Reads a string value from the named group, unescaped. Other groups are never read.
pub fn get_string_in(text: &str, group: &str, key: &str) -> Result<Option<String>, DesktopError> {
    check_group(group)?;
    let kinds = scan(text)?;
    first_value(&kinds, group, key)
        .map(unescape_string)
        .transpose()
}

/// Reads a list value from the Desktop Entry group.
pub fn get_list(text: &str, key: &str) -> Result<Option<Vec<String>>, DesktopError> {
    let kinds = scan(text)?;
    first_value(&kinds, DESKTOP_ENTRY_GROUP, key)
        .map(split_list)
        .transpose()
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
    set_encoded_in(
        text,
        DESKTOP_ENTRY_GROUP,
        key,
        encoded,
        same,
        is_valid_key,
        NewGroup::Top,
    )
}

/// Where a group that the text does not have yet is created.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NewGroup {
    /// Before the existing text (the Desktop Entry group comes first).
    Top,
    /// After the existing text, separated by one blank line.
    End,
}

/// Sets `key` in `group` to an already encoded value, checking the key with `key_ok`.
fn set_encoded_in(
    text: &str,
    group: &str,
    key: &str,
    encoded: &str,
    same: impl Fn(&str) -> bool,
    key_ok: fn(&str) -> bool,
    new_group: NewGroup,
) -> Result<Option<String>, DesktopError> {
    if !key_ok(key) {
        return Err(DesktopError::InvalidKey(key.to_string()));
    }
    check_group(group)?;
    let kinds = scan_with(text, key_ok)?;
    let new_line = format!("{key}={encoded}");
    let found = key_lines(&kinds, group, key);

    if found.is_empty() {
        let Some((header, end)) = group_range(&kinds, group) else {
            let mut out = match new_group {
                NewGroup::Top => format!("[{group}]\n{new_line}\n{text}"),
                NewGroup::End => {
                    let mut out = text.to_string();
                    if !out.is_empty() {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push('\n');
                    }
                    out
                }
            };
            if new_group == NewGroup::End {
                out.push_str(&format!("[{group}]\n{new_line}\n"));
            }
            return Ok(Some(out));
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

/// Sets a string value in the named group. A missing group is created at the end of the
/// text. Other groups are never changed. `None` means nothing changed.
pub fn set_string_in(
    text: &str,
    group: &str,
    key: &str,
    value: &str,
) -> Result<Option<String>, DesktopError> {
    let encoded = escape_string(value)?;
    set_encoded_in(
        text,
        group,
        key,
        &encoded,
        |raw| unescape_string(raw).is_ok_and(|v| v == value),
        is_valid_key,
        NewGroup::End,
    )
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
    remove_key_in(text, DESKTOP_ENTRY_GROUP, key)
}

/// Removes every line that sets `key` in `group`. `None` means the key was absent.
pub fn remove_key_in(text: &str, group: &str, key: &str) -> Result<Option<String>, DesktopError> {
    if !is_valid_key(key) {
        return Err(DesktopError::InvalidKey(key.to_string()));
    }
    check_group(group)?;
    let kinds = scan(text)?;
    let found = key_lines(&kinds, group, key);
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

/// Expands the Exec key of the Desktop Entry group and fills the context codes from the
/// entry itself: `%i` becomes `--icon` and the Icon value, `%c` the translated Name, and
/// `%k` is `desktop_file`, the path the entry was read from. `locale` is a POSIX locale
/// such as `de_DE.UTF-8`; a localized key is preferred over the plain key. Returns `None`
/// when there is no Exec key. Each substituted value stays exactly one argument.
pub fn exec_argv_in_entry(
    text: &str,
    files: &[&str],
    urls: &[&str],
    desktop_file: &str,
    locale: Option<&str>,
) -> Result<Option<Vec<String>>, DesktopError> {
    let Some(exec) = get_string(text, "Exec")? else {
        return Ok(None);
    };
    let icon = localized_string(text, "Icon", locale)?.filter(|icon| !icon.is_empty());
    let name = localized_string(text, "Name", locale)?;
    let ctx = FieldContext {
        files,
        urls,
        icon: icon.as_deref(),
        name: name.as_deref(),
        desktop_file: Some(desktop_file),
    };
    expand_exec(&parse_exec(&exec)?, &ctx).map(Some)
}

/// The localized value of `key` for the first locale candidate that has one, else the
/// plain value.
fn localized_string(
    text: &str,
    key: &str,
    locale: Option<&str>,
) -> Result<Option<String>, DesktopError> {
    for candidate in locale.map_or_else(Vec::new, locale_candidates) {
        if let Some(value) = get_string(text, &format!("{key}[{candidate}]"))? {
            return Ok(Some(value));
        }
    }
    get_string(text, key)
}

/// The lookup order for a POSIX locale, as the specification gives it:
/// `lang_COUNTRY.ENCODING@MODIFIER` first, then each part dropped in turn, then `lang`.
/// Empty candidates are skipped and duplicates removed.
pub fn locale_candidates(locale: &str) -> Vec<String> {
    let (rest, modifier) = match locale.split_once('@') {
        Some((rest, modifier)) => (rest, Some(modifier)),
        None => (locale, None),
    };
    let (rest, encoding) = match rest.split_once('.') {
        Some((rest, encoding)) => (rest, Some(encoding)),
        None => (rest, None),
    };
    let (lang, country) = match rest.split_once('_') {
        Some((lang, country)) => (lang, Some(country)),
        None => (rest, None),
    };
    let mut heads = Vec::new();
    if let Some(country) = country {
        heads.push(format!("{lang}_{country}"));
    }
    heads.push(lang.to_string());
    let mut out: Vec<String> = Vec::new();
    for head in &heads {
        for (enc, modi) in [
            (encoding, modifier),
            (None, modifier),
            (encoding, None),
            (None, None),
        ] {
            let mut candidate = head.clone();
            if let Some(enc) = enc {
                candidate.push('.');
                candidate.push_str(enc);
            }
            if let Some(modi) = modi {
                candidate.push('@');
                candidate.push_str(modi);
            }
            if !candidate.is_empty() && !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
    out
}

/// The `mimeapps.list` group that sets default applications.
pub const DEFAULT_APPLICATIONS: &str = "Default Applications";
/// The `mimeapps.list` group that adds applications to a MIME type's list.
pub const ADDED_ASSOCIATIONS: &str = "Added Associations";
/// The `mimeapps.list` group that removes applications from a MIME type's list.
pub const REMOVED_ASSOCIATIONS: &str = "Removed Associations";

fn is_association_group(name: &str) -> bool {
    matches!(
        name,
        DEFAULT_APPLICATIONS | ADDED_ASSOCIATIONS | REMOVED_ASSOCIATIONS
    )
}

fn is_mime_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

/// Checks `type/subtype`. Each side uses ASCII letters, digits and `-_.+`.
pub fn is_valid_mime_type(mime_type: &str) -> bool {
    mime_type
        .split_once('/')
        .is_some_and(|(kind, subtype)| is_mime_part(kind) && is_mime_part(subtype))
}

/// Checks a desktop file id: `name.desktop`, where the name does not start with `.` and
/// uses ASCII letters, digits and `-_.+@`. Path separators, `;`, `%`, spaces and control
/// characters are refused, so an id can never name a path or inject a list item.
pub fn is_valid_desktop_id(id: &str) -> bool {
    id.strip_suffix(".desktop").is_some_and(|stem| {
        !stem.is_empty()
            && !stem.starts_with('.')
            && stem
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '@'))
    })
}

/// Splits a `;`-separated `mimeapps.list` value. The format has no escapes. Items are
/// trimmed of spaces and tabs, and empty items are dropped.
fn list_items(raw: &str) -> Vec<&str> {
    raw.split(';')
        .map(|item| item.trim_matches(|c| c == ' ' || c == '\t'))
        .filter(|item| !item.is_empty())
        .collect()
}

/// One `type/subtype=ids` line of a `mimeapps.list` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimeEntry {
    /// `Default Applications`, `Added Associations` or `Removed Associations`.
    pub group: String,
    pub mime_type: String,
    /// The desktop file ids in file order. Items that are not valid ids are dropped.
    pub desktop_ids: Vec<String>,
}

/// The association lines of one `mimeapps.list` file, in file order. Only the three
/// association groups are kept; other groups are checked for syntax and then ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MimeApps {
    pub entries: Vec<MimeEntry>,
}

/// Parses a `mimeapps.list` text. Every key must be a valid MIME type and every line must
/// be well formed.
pub fn parse_mimeapps(text: &str) -> Result<MimeApps, DesktopError> {
    let kinds = scan_with(text, is_valid_mime_type)?;
    let mut entries = Vec::new();
    let mut group = None;
    for kind in kinds {
        match kind {
            Kind::Group(name) => group = Some(name),
            Kind::Key { key, value } => {
                if let Some(name) = group.filter(|name| is_association_group(name)) {
                    entries.push(MimeEntry {
                        group: name.to_string(),
                        mime_type: key.to_string(),
                        desktop_ids: list_items(value)
                            .into_iter()
                            .filter(|id| is_valid_desktop_id(id))
                            .map(str::to_string)
                            .collect(),
                    });
                }
            }
            Kind::Blank | Kind::Comment => {}
        }
    }
    Ok(MimeApps { entries })
}

/// The ids that `list` gives for `mime_type` in `group`, in file order.
fn ids_in<'a>(
    list: &'a MimeApps,
    group: &'a str,
    mime_type: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    list.entries
        .iter()
        .filter(move |entry| entry.group == group && entry.mime_type == mime_type)
        .flat_map(|entry| entry.desktop_ids.iter().map(String::as_str))
}

/// Whether `id` is removed for `mime_type` by the file at `upto` or by a file of higher
/// precedence (a lower index). A removal in a lower-precedence file does not count.
fn is_removed(lists: &[MimeApps], upto: usize, mime_type: &str, id: &str) -> bool {
    lists
        .iter()
        .take(upto + 1)
        .any(|list| ids_in(list, REMOVED_ASSOCIATIONS, mime_type).any(|removed| removed == id))
}

/// The default application for `mime_type`. `lists` are the parsed files in precedence
/// order, highest first: `~/.config/mimeapps.list`, then `$XDG_CONFIG_DIRS/mimeapps.list`,
/// then `$XDG_DATA_HOME/applications/mimeapps.list`, then the same file under
/// `$XDG_DATA_DIRS`. The first id of the highest-precedence file's `Default Applications`
/// entry wins, unless a removal in that file or a higher one hides it; then the next id
/// and the next file are tried. The caller checks that the application is installed.
/// `None` means the caller should fall back to the association caches.
pub fn default_for(mime_type: &str, lists: &[MimeApps]) -> Option<String> {
    for (index, list) in lists.iter().enumerate() {
        let found = ids_in(list, DEFAULT_APPLICATIONS, mime_type)
            .find(|id| !is_removed(lists, index, mime_type, id));
        if let Some(id) = found {
            return Some(id.to_string());
        }
    }
    None
}

/// Every application for `mime_type` in order of preference: the default applications of
/// all files first, then the added associations of all files, in precedence order. An
/// application appears once. A removal hides an application in its own file and in every
/// file of lower precedence, so a removal in a higher file hides an addition in a lower
/// one, while a removal in a lower file never hides a higher file's entry.
pub fn associations_for(mime_type: &str, lists: &[MimeApps]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for group in [DEFAULT_APPLICATIONS, ADDED_ASSOCIATIONS] {
        for (index, list) in lists.iter().enumerate() {
            for id in ids_in(list, group, mime_type) {
                if !is_removed(lists, index, mime_type, id) && !out.iter().any(|seen| seen == id) {
                    out.push(id.to_string());
                }
            }
        }
    }
    out
}

/// Makes `desktop_id` the first default application for `mime_type` in one `mimeapps.list`
/// text. The ids already listed for the type follow it. Only the `Default Applications`
/// group changes: comments, other groups and other keys are kept, and the group is created
/// at the end of the text when it is missing. `None` means the file already has this
/// default in front. Invalid MIME types and desktop ids are errors.
pub fn set_default_text(
    text: &str,
    mime_type: &str,
    desktop_id: &str,
) -> Result<Option<String>, DesktopError> {
    if !is_valid_mime_type(mime_type) {
        return Err(DesktopError::InvalidMimeType(mime_type.to_string()));
    }
    if !is_valid_desktop_id(desktop_id) {
        return Err(DesktopError::InvalidDesktopId(desktop_id.to_string()));
    }
    let kinds = scan_with(text, is_valid_mime_type)?;
    let mut wanted = vec![desktop_id];
    if let Some(raw) = first_value(&kinds, DEFAULT_APPLICATIONS, mime_type) {
        wanted.extend(
            list_items(raw)
                .into_iter()
                .filter(|item| *item != desktop_id),
        );
    }
    let encoded: String = wanted.iter().map(|item| format!("{item};")).collect();
    set_encoded_in(
        text,
        DEFAULT_APPLICATIONS,
        mime_type,
        &encoded,
        |raw| list_items(raw) == wanted,
        is_valid_mime_type,
        NewGroup::End,
    )
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

    const LAUNCH: &str =
        "[Desktop Entry]\nName=Plain Name\nName[de]=Deutscher Name\nIcon=web-icon\nExec=app %i %c %k\n";

    #[test]
    fn named_group_reads_and_edits_only_that_group() {
        let action = "Desktop Action New";
        assert_eq!(
            get_string_in(EXAMPLE, action, "Exec").unwrap().as_deref(),
            Some("webapp --new")
        );
        assert_eq!(get_string_in(EXAMPLE, action, "Icon").unwrap(), None);
        assert_eq!(
            get_string_in(EXAMPLE, DESKTOP_ENTRY_GROUP, "Exec")
                .unwrap()
                .as_deref(),
            Some("\"/opt/My Apps/webapp\" --app=https://example.com/ %U")
        );
        let out = set_string_in(EXAMPLE, action, "Exec", "webapp --other")
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            EXAMPLE.replace("Exec=webapp --new\n", "Exec=webapp --other\n")
        );
        assert_eq!(
            set_string_in(EXAMPLE, action, "Name", "New Window").unwrap(),
            None
        );
        assert_eq!(
            remove_key_in(EXAMPLE, action, "Name").unwrap().unwrap(),
            EXAMPLE.replace("Name=New Window\n", "")
        );
        assert_eq!(remove_key_in(EXAMPLE, action, "Icon").unwrap(), None);
    }

    #[test]
    fn named_group_is_created_at_the_end_when_missing() {
        let out = set_string_in(EXAMPLE, "Desktop Action Quit", "Name", "Quit")
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            format!("{EXAMPLE}\n[Desktop Action Quit]\nName=Quit\n")
        );
        assert_eq!(
            get_string_in(&out, "Desktop Action Quit", "Name")
                .unwrap()
                .as_deref(),
            Some("Quit")
        );
        let bare = set_string_in(
            "[Desktop Entry]\nName=a",
            "Desktop Action Quit",
            "Exec",
            "q",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            bare,
            "[Desktop Entry]\nName=a\n\n[Desktop Action Quit]\nExec=q\n"
        );
        assert_eq!(
            set_string_in("", "G", "K", "V").unwrap().unwrap(),
            "[G]\nK=V\n"
        );
    }

    #[test]
    fn named_group_refuses_hostile_group_names() {
        for group in ["A]", "A[B", "", "A\nExec=/bin/evil", "A\tB", "Ünïcode"] {
            assert!(
                matches!(
                    set_string_in(EXAMPLE, group, "Name", "x"),
                    Err(DesktopError::InvalidGroup(_))
                ),
                "{group:?}"
            );
            assert!(matches!(
                get_string_in(EXAMPLE, group, "Name"),
                Err(DesktopError::InvalidGroup(_))
            ));
            assert!(matches!(
                remove_key_in(EXAMPLE, group, "Name"),
                Err(DesktopError::InvalidGroup(_))
            ));
        }
    }

    #[test]
    fn named_group_value_newlines_cannot_inject_a_key() {
        let out = set_string_in(EXAMPLE, "Desktop Action New", "Name", "x\nExec=/bin/evil")
            .unwrap()
            .unwrap();
        assert_eq!(out.lines().filter(|l| l.starts_with("Exec=")).count(), 2);
        assert_eq!(
            get_string_in(&out, "Desktop Action New", "Name")
                .unwrap()
                .as_deref(),
            Some("x\nExec=/bin/evil")
        );
        assert_eq!(
            get_string_in(&out, DESKTOP_ENTRY_GROUP, "Exec").unwrap(),
            get_string(EXAMPLE, "Exec").unwrap()
        );
    }

    #[test]
    fn exec_context_fills_icon_name_and_path_once_each() {
        let none: &[&str] = &[];
        let out = exec_argv_in_entry(
            LAUNCH,
            none,
            none,
            "/usr/share/applications/x.desktop",
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            out,
            vec![
                "app",
                "--icon",
                "web-icon",
                "Plain Name",
                "/usr/share/applications/x.desktop"
            ]
        );
        let german = exec_argv_in_entry(LAUNCH, none, none, "/x.desktop", Some("de_DE.UTF-8"))
            .unwrap()
            .unwrap();
        assert_eq!(
            german,
            vec!["app", "--icon", "web-icon", "Deutscher Name", "/x.desktop"]
        );
        let bare = "[Desktop Entry]\nExec=app %i %c %k\nIcon=\n";
        assert_eq!(
            exec_argv_in_entry(bare, none, none, "/x.desktop", None)
                .unwrap()
                .unwrap(),
            vec!["app", "/x.desktop"]
        );
        assert_eq!(
            exec_argv_in_entry("[Desktop Entry]\nName=x\n", none, none, "/x", None).unwrap(),
            None
        );
    }

    #[test]
    fn exec_context_values_are_never_split_or_reparsed() {
        let none: &[&str] = &[];
        let hostile =
            "[Desktop Entry]\nName=a b %U $HOME;rm\nIcon=evil name\nExec=app %c %i --x=%k\n";
        assert_eq!(
            exec_argv_in_entry(hostile, none, none, "/p/My %U Apps/x.desktop", None)
                .unwrap()
                .unwrap(),
            vec![
                "app",
                "a b %U $HOME;rm",
                "--icon",
                "evil name",
                "--x=/p/My %U Apps/x.desktop"
            ]
        );
        let injected = "[Desktop Entry]\nName=x\\nExec=/bin/evil\nExec=app %c\n";
        assert_eq!(
            exec_argv_in_entry(injected, none, none, "/x", None)
                .unwrap()
                .unwrap(),
            vec!["app", "x\nExec=/bin/evil"]
        );
        let embedded = "[Desktop Entry]\nIcon=x\nExec=app --icon=%i\n";
        assert_eq!(
            exec_argv_in_entry(embedded, none, none, "/x", None),
            Err(DesktopError::EmbeddedFieldCode('i'))
        );
    }

    #[test]
    fn locale_candidates_follow_the_specification_order() {
        assert_eq!(
            locale_candidates("de_DE.UTF-8@euro"),
            vec![
                "de_DE.UTF-8@euro",
                "de_DE@euro",
                "de_DE.UTF-8",
                "de_DE",
                "de.UTF-8@euro",
                "de@euro",
                "de.UTF-8",
                "de"
            ]
        );
        assert_eq!(locale_candidates("fr"), vec!["fr"]);
        assert_eq!(locale_candidates(""), Vec::<String>::new());
    }

    const USER_MIME: &str = "\
# per-user choices
[Default Applications]
text/html=firefox.desktop;chromium.desktop;
image/png=gimp.desktop;

[Added Associations]
text/html=code.desktop;
x-scheme-handler/https=firefox.desktop;

[Removed Associations]
image/png=eog.desktop;
";

    const SYSTEM_MIME: &str = "\
[Default Applications]
text/html=vivaldi.desktop;
image/png=eog.desktop;
video/mp4=vlc.desktop;

[Added Associations]
image/png=eog.desktop;gimp.desktop;
text/plain=gedit.desktop;
";

    #[test]
    fn mimeapps_parse_keeps_the_association_groups_in_order() {
        let user = parse_mimeapps(USER_MIME).unwrap();
        assert_eq!(user.entries.len(), 5);
        assert_eq!(
            user.entries[0],
            MimeEntry {
                group: DEFAULT_APPLICATIONS.to_string(),
                mime_type: "text/html".to_string(),
                desktop_ids: vec![
                    "firefox.desktop".to_string(),
                    "chromium.desktop".to_string()
                ],
            }
        );
        assert_eq!(user.entries[4].group, REMOVED_ASSOCIATIONS);
    }

    #[test]
    fn mimeapps_drops_hostile_ids_and_refuses_bad_keys() {
        let text = "[Default Applications]\ntext/html=x.desktop;%U.desktop;../evil.desktop;a b.desktop;a.desktop;;\n";
        let parsed = parse_mimeapps(text).unwrap();
        assert_eq!(
            parsed.entries[0].desktop_ids,
            vec!["x.desktop", "a.desktop"]
        );
        assert!(matches!(
            parse_mimeapps("[Default Applications]\nnot a mime=x.desktop\n"),
            Err(DesktopError::MalformedLine { line: 2, .. })
        ));
        assert!(matches!(
            parse_mimeapps("text/html=x.desktop\n"),
            Err(DesktopError::MalformedLine { line: 1, .. })
        ));
    }

    #[test]
    fn mime_types_and_desktop_ids_are_validated() {
        assert!(is_valid_mime_type("text/html"));
        assert!(is_valid_mime_type("x-scheme-handler/https"));
        assert!(is_valid_mime_type("application/vnd.ms-excel+xml"));
        for bad in [
            "text",
            "text/",
            "/html",
            "text/html/x",
            "te xt/html",
            "text/ht\nml",
            "a=b/c",
            "a%/b",
        ] {
            assert!(!is_valid_mime_type(bad), "{bad:?}");
        }
        assert!(is_valid_desktop_id("firefox.desktop"));
        assert!(is_valid_desktop_id("org.gnome.Nautilus.desktop"));
        assert!(is_valid_desktop_id("app-1_x+y@z.desktop"));
        for bad in [
            "",
            ".desktop",
            ".hidden.desktop",
            "../x.desktop",
            "a/b.desktop",
            "a\\b.desktop",
            "a b.desktop",
            "a%U.desktop",
            "a;b.desktop",
            "a.desktop;b.desktop",
            "a\nb.desktop",
            "x.DESKTOP",
            "x.desktop.txt",
        ] {
            assert!(!is_valid_desktop_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn default_for_follows_precedence_and_removals() {
        let user = parse_mimeapps(USER_MIME).unwrap();
        let system = parse_mimeapps(SYSTEM_MIME).unwrap();
        let lists = [user, system.clone()];
        assert_eq!(
            default_for("text/html", &lists),
            Some("firefox.desktop".to_string())
        );
        assert_eq!(
            default_for("video/mp4", &lists),
            Some("vlc.desktop".to_string())
        );
        assert_eq!(
            default_for("image/png", &lists),
            Some("gimp.desktop".to_string())
        );
        assert_eq!(default_for("text/plain", &lists), None);
        let hide = parse_mimeapps("[Removed Associations]\nimage/png=eog.desktop;\n").unwrap();
        assert_eq!(
            default_for("image/png", &[hide.clone(), system.clone()]),
            None
        );
        assert_eq!(
            default_for("image/png", &[system, hide]),
            Some("eog.desktop".to_string())
        );
    }

    #[test]
    fn associations_put_defaults_first_and_apply_removals_downward() {
        let lists = [
            parse_mimeapps(USER_MIME).unwrap(),
            parse_mimeapps(SYSTEM_MIME).unwrap(),
        ];
        assert_eq!(
            associations_for("text/html", &lists),
            vec![
                "firefox.desktop",
                "chromium.desktop",
                "vivaldi.desktop",
                "code.desktop"
            ]
        );
        // The user file removes eog, which hides the system's default and addition of eog.
        assert_eq!(associations_for("image/png", &lists), vec!["gimp.desktop"]);
        assert_eq!(
            associations_for("text/plain", &lists),
            vec!["gedit.desktop"]
        );
        let hide = parse_mimeapps("[Removed Associations]\ntext/plain=gedit.desktop;\n").unwrap();
        assert_eq!(
            associations_for("text/plain", &[hide, lists[1].clone()]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn set_default_changes_only_the_default_group() {
        let out = set_default_text(USER_MIME, "text/html", "chromium.desktop")
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            USER_MIME.replace(
                "text/html=firefox.desktop;chromium.desktop;\n",
                "text/html=chromium.desktop;firefox.desktop;\n"
            )
        );
        assert_eq!(
            set_default_text(USER_MIME, "text/html", "firefox.desktop").unwrap(),
            None
        );
        let added = set_default_text(USER_MIME, "video/ogg", "mpv.desktop")
            .unwrap()
            .unwrap();
        assert_eq!(
            added,
            USER_MIME.replace(
                "image/png=gimp.desktop;\n",
                "image/png=gimp.desktop;\nvideo/ogg=mpv.desktop;\n"
            )
        );
        assert_eq!(
            set_default_text("", "text/html", "a.desktop")
                .unwrap()
                .unwrap(),
            "[Default Applications]\ntext/html=a.desktop;\n"
        );
    }

    #[test]
    fn set_default_keeps_comments_and_refuses_hostile_input() {
        let text = "# mine\n[Default Applications]\n# keep\ntext/html=a.desktop;\n";
        let out = set_default_text(text, "video/mp4", "mpv.desktop")
            .unwrap()
            .unwrap();
        assert!(out.starts_with("# mine\n[Default Applications]\n# keep\ntext/html=a.desktop;\n"));
        assert!(out.ends_with("video/mp4=mpv.desktop;\n"));
        for id in [
            "a%U.desktop",
            "../a.desktop",
            "a.desktop;b.desktop",
            "a b.desktop",
            "a\nb.desktop",
            "a.desktop\n[Removed Associations]",
        ] {
            assert!(
                matches!(
                    set_default_text(text, "text/html", id),
                    Err(DesktopError::InvalidDesktopId(_))
                ),
                "{id:?}"
            );
        }
        assert!(matches!(
            set_default_text(text, "text/html\n", "a.desktop"),
            Err(DesktopError::InvalidMimeType(_))
        ));
        assert!(matches!(
            set_default_text(text, "text/html=x", "a.desktop"),
            Err(DesktopError::InvalidMimeType(_))
        ));
    }

    #[test]
    fn set_default_round_trips_through_default_for() {
        let system = parse_mimeapps(SYSTEM_MIME).unwrap();
        assert_eq!(
            default_for("video/mp4", std::slice::from_ref(&system)),
            Some("vlc.desktop".to_string())
        );
        let edited = set_default_text(SYSTEM_MIME, "video/mp4", "mpv.desktop")
            .unwrap()
            .unwrap();
        let reread = parse_mimeapps(&edited).unwrap();
        assert_eq!(
            default_for("video/mp4", std::slice::from_ref(&reread)),
            Some("mpv.desktop".to_string())
        );
        let user = parse_mimeapps(USER_MIME).unwrap();
        assert_eq!(
            default_for("video/mp4", &[user, reread]),
            Some("mpv.desktop".to_string())
        );
        let front = set_default_text(SYSTEM_MIME, "image/png", "gimp.desktop")
            .unwrap()
            .unwrap();
        assert_eq!(
            associations_for("image/png", &[parse_mimeapps(&front).unwrap()]),
            vec!["gimp.desktop", "eog.desktop"]
        );
    }
}
