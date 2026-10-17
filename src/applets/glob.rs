//! `fs.glob`: explicit pathname matching that returns a sorted list of text.
//!
//! Syntax, applied one path component at a time:
//! - `*` matches any run of characters and `?` matches one character, both
//!   within a single component.
//! - `[...]` matches one character from a set. Ranges such as `a-z` are
//!   allowed, `!` or `^` after `[` negates the set, and `]` first in the set is
//!   literal. An unterminated set is an error.
//! - A component that is exactly `**` matches zero or more directory levels.
//!   It does not descend into symbolic links and cannot be the final component.
//! - Every other character is literal, so no backslash escape exists on Unix.
//!
//! Hidden entries (names starting with `.`) are skipped by wildcards unless
//! `include_hidden` is true or the component itself starts with a literal dot.
//! `.` and `..` are never produced by a wildcard. A leading `~` followed by a
//! separator or the end of the pattern expands to `HOME` (`USERPROFILE` on
//! Windows when `HOME` is unset); `~user` is not expanded. Relative patterns are
//! resolved against the program working directory and returned as written.
//!
//! Results are unique and sorted by their UTF-8 bytes, which is code-point
//! order rather than a locale collation. An empty match is an empty list. A
//! missing directory on the path is also an empty match, while other directory
//! read errors fail. Matches over 100,000 paths, more than 1,000,000 examined
//! entries, or recursion deeper than 256 directory levels fail with an explicit
//! error. Output uses the platform's native separator; a pattern ending in a
//! separator matches only directories and returns them with a trailing separator.

use super::{
    checkpoint_work, failed, invalid, io_error, required_string, AppletError, ExecutionContext,
};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf, MAIN_SEPARATOR};

const MAX_PATTERN_BYTES: usize = 4096;
const MAX_PATTERN_COMPONENTS: usize = 256;
const MAX_GLOB_RESULTS: usize = 100_000;
const MAX_GLOB_ENTRIES: usize = 1_000_000;
const MAX_GLOB_DEPTH: usize = 256;

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Literal(char),
    AnyOne,
    AnyRun,
    Set(CharSet),
}

#[derive(Debug, PartialEq, Eq)]
struct CharSet {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl CharSet {
    fn contains(&self, character: char) -> bool {
        let inside = self
            .ranges
            .iter()
            .any(|(low, high)| *low <= character && character <= *high);
        inside != self.negated
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    /// No wildcard characters: looked up directly, without reading a directory.
    Literal(String),
    /// A wildcard component. `leading_dot` records a literal first dot, which
    /// opts the component into matching hidden names.
    Wildcard {
        tokens: Vec<Token>,
        leading_dot: bool,
    },
    /// The whole component `**`.
    AnyDepth,
}

pub(super) fn glob(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let pattern = required_string(args, "pattern")?;
    let include_hidden = match args.get("include_hidden") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid("args.include_hidden must be a boolean")),
    };
    if pattern.is_empty() {
        return Err(invalid("args.pattern cannot be empty"));
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(invalid(format!(
            "args.pattern exceeds the {MAX_PATTERN_BYTES}-byte limit"
        )));
    }
    if pattern.contains('\0') {
        return Err(invalid("args.pattern must not contain NUL"));
    }
    let expanded = expand_home(pattern)?;
    let dirs_only = expanded.ends_with(is_separator);
    let (base, segments) = parse_pattern(&expanded)?;

    let fs_base = if base.as_os_str().is_empty() {
        cwd.to_path_buf()
    } else if base.is_absolute() {
        base.clone()
    } else {
        cwd.join(&base)
    };

    let mut walk = Walk {
        segments: &segments,
        include_hidden,
        dirs_only,
        context,
        results: Vec::new(),
        examined: 0,
        names: Vec::new(),
    };
    if segments.is_empty() {
        // A bare root or home directory has no component to read: answer with
        // the base itself when it exists.
        if lookup_metadata(&fs_base, true)?.is_some() {
            walk.emit(&base, &fs_base)?;
        }
    } else {
        walk.walk(&base, &fs_base, 0, 0)?;
    }

    let mut results = walk.results;
    results.sort_unstable();
    results.dedup();
    Ok(Value::Array(
        results.into_iter().map(Value::String).collect(),
    ))
}

fn is_separator(character: char) -> bool {
    character == '/' || (cfg!(windows) && character == '\\')
}

fn expand_home(pattern: &str) -> Result<Cow<'_, str>, AppletError> {
    let rest = match pattern.strip_prefix('~') {
        Some("") => Some(""),
        Some(rest) if rest.starts_with(is_separator) => Some(&rest[1..]),
        _ => None,
    };
    let Some(rest) = rest else {
        return Ok(Cow::Borrowed(pattern));
    };
    let home = home_directory()?;
    if rest.is_empty() {
        return Ok(Cow::Owned(home));
    }
    Ok(Cow::Owned(format!(
        "{}{MAIN_SEPARATOR}{rest}",
        home.trim_end_matches(is_separator)
    )))
}

fn home_directory() -> Result<String, AppletError> {
    let value = std::env::var_os("HOME")
        .or_else(|| {
            cfg!(windows)
                .then(|| std::env::var_os("USERPROFILE"))
                .flatten()
        })
        .ok_or_else(|| invalid("cannot expand ~ because HOME is not set"))?;
    value
        .into_string()
        .map_err(|_| invalid("HOME must be valid UTF-8 to expand ~"))
}

/// Split a pattern into its absolute (or empty) base and per-component
/// segments. `Path::components` handles Windows prefixes and collapses
/// redundant separators.
fn parse_pattern(expanded: &str) -> Result<(PathBuf, Vec<Segment>), AppletError> {
    let mut base = PathBuf::new();
    let mut segments = Vec::new();
    for component in Path::new(expanded).components() {
        match component {
            Component::Prefix(_) | Component::RootDir => base.push(component.as_os_str()),
            Component::CurDir => segments.push(Segment::Literal(".".into())),
            Component::ParentDir => segments.push(Segment::Literal("..".into())),
            Component::Normal(name) => {
                let text = name
                    .to_str()
                    .ok_or_else(|| invalid("args.pattern must be valid UTF-8"))?;
                segments.push(parse_segment(text)?);
            }
        }
    }
    if segments.len() > MAX_PATTERN_COMPONENTS {
        return Err(invalid(format!(
            "args.pattern exceeds the {MAX_PATTERN_COMPONENTS}-component limit"
        )));
    }
    if matches!(segments.last(), Some(Segment::AnyDepth)) {
        return Err(invalid(
            "** must be followed by another path component in args.pattern",
        ));
    }
    Ok((base, segments))
}

fn parse_segment(text: &str) -> Result<Segment, AppletError> {
    if text == "**" {
        return Ok(Segment::AnyDepth);
    }
    if !text.contains(['*', '?', '[']) {
        return Ok(Segment::Literal(text.to_owned()));
    }
    let characters: Vec<char> = text.chars().collect();
    let leading_dot = characters.first() == Some(&'.');
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        index += 1;
        match character {
            '*' => {
                if tokens.last() != Some(&Token::AnyRun) {
                    tokens.push(Token::AnyRun);
                }
            }
            '?' => tokens.push(Token::AnyOne),
            '[' => tokens.push(Token::Set(parse_set(&characters, &mut index)?)),
            other => tokens.push(Token::Literal(other)),
        }
    }
    Ok(Segment::Wildcard {
        tokens,
        leading_dot,
    })
}

/// Parse a bracket set whose opening `[` has already been consumed.
fn parse_set(characters: &[char], index: &mut usize) -> Result<CharSet, AppletError> {
    let unterminated = || invalid("args.pattern has an unterminated [ set");
    let negated = matches!(characters.get(*index), Some('!' | '^'));
    if negated {
        *index += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let low = *characters.get(*index).ok_or_else(unterminated)?;
        *index += 1;
        if low == ']' && !first {
            break;
        }
        first = false;
        let is_range = characters.get(*index) == Some(&'-')
            && characters.get(*index + 1).is_some_and(|next| *next != ']');
        if is_range {
            let high = characters[*index + 1];
            *index += 2;
            if high < low {
                return Err(invalid(format!(
                    "args.pattern has a reversed range {low}-{high} in a [ set"
                )));
            }
            ranges.push((low, high));
        } else {
            ranges.push((low, low));
        }
    }
    Ok(CharSet { negated, ranges })
}

/// Classic single-star backtracking over characters. Each non-star token
/// consumes exactly one character, so one saved star position is enough and
/// the cost stays polynomial for any pattern.
fn wildcard_matches(tokens: &[Token], name: &[char]) -> bool {
    let (mut token, mut character) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while character < name.len() {
        match tokens.get(token) {
            Some(Token::AnyRun) => {
                star = Some((token + 1, character));
                token += 1;
                continue;
            }
            Some(Token::AnyOne) => {
                token += 1;
                character += 1;
                continue;
            }
            Some(Token::Literal(expected)) if *expected == name[character] => {
                token += 1;
                character += 1;
                continue;
            }
            Some(Token::Set(set)) if set.contains(name[character]) => {
                token += 1;
                character += 1;
                continue;
            }
            _ => {}
        }
        match star {
            Some((resume_token, resume_character)) => {
                token = resume_token;
                character = resume_character + 1;
                star = Some((resume_token, character));
            }
            None => return false,
        }
    }
    while matches!(tokens.get(token), Some(Token::AnyRun)) {
        token += 1;
    }
    token == tokens.len()
}

/// Look up a path without reading a directory. `Ok(None)` means absent.
fn lookup_metadata(path: &Path, follow: bool) -> Result<Option<fs::Metadata>, AppletError> {
    let result = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    match result {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if is_absent(&error) => Ok(None),
        Err(error) => Err(io_error(error, format!("globbing {}", path.display()))),
    }
}

fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn is_directory(path: &Path) -> Result<bool, AppletError> {
    Ok(lookup_metadata(path, true)?.is_some_and(|metadata| metadata.is_dir()))
}

struct Walk<'segments, 'context, 'cancel> {
    segments: &'segments [Segment],
    include_hidden: bool,
    dirs_only: bool,
    context: &'context ExecutionContext<'cancel>,
    results: Vec<String>,
    examined: usize,
    names: Vec<char>,
}

impl Walk<'_, '_, '_> {
    fn walk(
        &mut self,
        display: &Path,
        fs_dir: &Path,
        index: usize,
        depth: usize,
    ) -> Result<(), AppletError> {
        if index == self.segments.len() {
            return self.emit(display, fs_dir);
        }
        if depth > MAX_GLOB_DEPTH {
            return Err(failed(
                "glob_limit",
                format!("fs.glob recursion exceeds {MAX_GLOB_DEPTH} directory levels"),
            ));
        }
        // Copy the slice reference so segment borrows do not overlap `&mut self`.
        let segments = self.segments;
        let last = index + 1 == segments.len();
        match &segments[index] {
            Segment::Literal(name) => {
                let child_display = display.join(name);
                let child_fs = fs_dir.join(name);
                if last {
                    if lookup_metadata(&child_fs, false)?.is_some() {
                        self.emit(&child_display, &child_fs)?;
                    }
                } else if is_directory(&child_fs)? {
                    self.walk(&child_display, &child_fs, index + 1, depth + 1)?;
                }
            }
            Segment::Wildcard {
                tokens,
                leading_dot,
            } => {
                let mut matched = Vec::new();
                for name in self.read_names(fs_dir)? {
                    if !self.visible(&name, *leading_dot) {
                        continue;
                    }
                    let Some(text) = name.to_str() else {
                        return Err(failed(
                            "io_error",
                            format!("fs.glob found a non-UTF-8 name in {}", fs_dir.display()),
                        ));
                    };
                    self.names.clear();
                    self.names.extend(text.chars());
                    if wildcard_matches(tokens, &self.names) {
                        matched.push(name);
                    }
                }
                for name in matched {
                    let child_display = display.join(&name);
                    let child_fs = fs_dir.join(&name);
                    if last {
                        self.emit(&child_display, &child_fs)?;
                    } else if is_directory(&child_fs)? {
                        self.walk(&child_display, &child_fs, index + 1, depth + 1)?;
                    }
                }
            }
            Segment::AnyDepth => {
                // Zero directory levels: continue with the next component here.
                self.walk(display, fs_dir, index + 1, depth)?;
                // One or more levels: descend into real subdirectories (symbolic
                // links are not followed) and keep the same `**` segment.
                for name in self.read_names(fs_dir)? {
                    if !self.visible(&name, false) {
                        continue;
                    }
                    let child_fs = fs_dir.join(&name);
                    let is_real_directory =
                        fs::symlink_metadata(&child_fs).is_ok_and(|metadata| metadata.is_dir());
                    if is_real_directory {
                        self.walk(&display.join(&name), &child_fs, index, depth + 1)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Read the entry names of one directory. Absent or non-directory paths
    /// yield nothing; other failures are reported.
    fn read_names(&mut self, fs_dir: &Path) -> Result<Vec<OsString>, AppletError> {
        let entries = match fs::read_dir(fs_dir) {
            Ok(entries) => entries,
            Err(error) if is_absent(&error) => return Ok(Vec::new()),
            Err(error) => return Err(io_error(error, format!("reading {}", fs_dir.display()))),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| io_error(error, format!("reading {}", fs_dir.display())))?;
            self.examined += 1;
            if self.examined > MAX_GLOB_ENTRIES {
                return Err(failed(
                    "glob_limit",
                    format!("fs.glob examined more than {MAX_GLOB_ENTRIES} directory entries"),
                ));
            }
            checkpoint_work(self.context, self.examined)?;
            names.push(entry.file_name());
        }
        Ok(names)
    }

    fn visible(&self, name: &OsStr, leading_dot_literal: bool) -> bool {
        self.include_hidden || leading_dot_literal || name.as_encoded_bytes().first() != Some(&b'.')
    }

    /// Record one match. `display` is the path as written (relative to the
    /// program directory when the pattern is relative); `fs_path` is what the
    /// filesystem checks use.
    fn emit(&mut self, display: &Path, fs_path: &Path) -> Result<(), AppletError> {
        if self.dirs_only && !is_directory(fs_path)? {
            return Ok(());
        }
        if self.results.len() >= MAX_GLOB_RESULTS {
            return Err(failed(
                "glob_limit",
                format!("fs.glob matched more than {MAX_GLOB_RESULTS} paths"),
            ));
        }
        let text = display.to_str().ok_or_else(|| {
            failed(
                "io_error",
                "fs.glob produced a path that is not valid UTF-8",
            )
        })?;
        let mut owned = text.to_owned();
        if self.dirs_only && !owned.ends_with(is_separator) {
            owned.push(MAIN_SEPARATOR);
        }
        self.results.push(owned);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn matches(pattern: &str, name: &str) -> bool {
        let Segment::Wildcard { tokens, .. } = parse_segment(pattern).unwrap() else {
            panic!("{pattern:?} should contain a wildcard");
        };
        let characters: Vec<char> = name.chars().collect();
        wildcard_matches(&tokens, &characters)
    }

    #[test]
    fn star_question_and_sets_match_within_one_name() {
        assert!(matches("*.txt", "notes.txt"));
        assert!(matches("*", "any name"));
        assert!(matches("a*b*c", "axxbyyc"));
        assert!(!matches("*.txt", "notes.md"));
        assert!(matches("?.rs", "a.rs"));
        assert!(!matches("?.rs", "ab.rs"));
        assert!(matches("file[0-9].log", "file7.log"));
        assert!(!matches("file[0-9].log", "filex.log"));
        assert!(matches("[!a-c]x", "dx"));
        assert!(!matches("[!a-c]x", "bx"));
        assert!(matches("[^a]x", "bx"));
        assert!(matches("[]]", "]"));
        assert!(matches("[a-]", "-"));
        assert!(matches("café*", "café-au-lait"));
        assert!(matches("?", "é"));
    }

    #[test]
    fn unterminated_and_reversed_sets_are_errors() {
        assert!(parse_segment("file[0-9").is_err());
        assert!(parse_segment("x[z-a]").is_err());
        assert!(parse_segment("[").is_err());
    }

    #[test]
    fn literal_components_skip_the_wildcard_engine() {
        assert_eq!(
            parse_segment("plain.txt").unwrap(),
            Segment::Literal("plain.txt".into())
        );
        assert_eq!(parse_segment("**").unwrap(), Segment::AnyDepth);
    }

    #[test]
    fn dot_prefixed_components_control_hidden_matching() {
        let Segment::Wildcard { leading_dot, .. } = parse_segment(".*").unwrap() else {
            panic!("expected wildcard");
        };
        assert!(leading_dot);
        let Segment::Wildcard { leading_dot, .. } = parse_segment("*").unwrap() else {
            panic!("expected wildcard");
        };
        assert!(!leading_dot);
        assert!(matches(".*", ".profile"));
        assert!(matches("*", ".profile"));
    }

    #[test]
    fn star_runs_collapse_and_pathological_patterns_stay_fast() {
        let Segment::Wildcard { tokens, .. } = parse_segment("a***b").unwrap() else {
            panic!("expected wildcard");
        };
        assert_eq!(
            tokens,
            vec![Token::Literal('a'), Token::AnyRun, Token::Literal('b')]
        );
        let name: Vec<char> = "a".repeat(5_000).chars().collect();
        let pattern = format!("{}b", "*a".repeat(200));
        let Segment::Wildcard { tokens, .. } = parse_segment(&pattern).unwrap() else {
            panic!("expected wildcard");
        };
        assert!(!wildcard_matches(&tokens, &name));
    }

    #[test]
    fn expansion_of_home_is_explicit() {
        assert_eq!(
            expand_home("relative/*").unwrap(),
            Cow::Borrowed("relative/*")
        );
        assert_eq!(expand_home("~user/x").unwrap(), Cow::Borrowed("~user/x"));
    }

    #[test]
    fn caps_and_argument_errors_are_explicit() {
        let context = ExecutionContext::unrestricted();
        let cwd = std::env::temp_dir();
        let run = |args: Value| {
            let args = args.as_object().cloned().unwrap();
            glob(&args, &cwd, &context)
        };
        assert!(run(json!({"pattern": ""})).is_err());
        assert!(run(json!({"pattern": "a\u{0}b"})).is_err());
        assert!(run(json!({"pattern": "x".repeat(MAX_PATTERN_BYTES + 1)})).is_err());
        assert!(run(json!({"pattern": "*", "include_hidden": "yes"})).is_err());
        assert!(run(json!({"pattern": "**"})).is_err());
        assert!(run(json!({"pattern": "a/**"})).is_err());
    }
}
