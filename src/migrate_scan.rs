//! Read-only scan of a user's own Omarchy customisations (issue #402).
//!
//! [`scan`] walks the locations in [`ROOTS`] under an explicit home directory,
//! lists every file there as an entry point, and classifies each shell script
//! from its text alone. Nothing is executed and nothing is written.
//!
//! The classifier is a port of `tools/omarchy_port_report.py`. The lexer, the
//! command parser, the feature categories and the verdict rule keep the Python
//! names and order. Comments on each rule give the Python lines it mirrors.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

/// Protocol id of the `--json` rendering, in the `execute.<name>/<n>` house style.
pub const PROTOCOL: &str = "execute.migrate-scan/1";

/// Nesting depth of `$(`, `${`, `"` and `(` groups the lexer follows before it stops.
/// The Python tool recurses without a limit; a deep file is blocked here instead of
/// overflowing the stack.
const MAX_NESTING: usize = 64;

/// Where users keep customisations, each with the source that names it.
///
/// * `.config/omarchy/hooks`: issue #402 Gap ("hook files and `.d` directories under
///   `~/.config/omarchy/hooks/`"); `docs/omarchy-inventory.json` summary of the hook
///   command ("Run a named hook from ~/.config/omarchy/hooks/<name> and
///   ~/.config/omarchy/hooks/<name>.d/"); `docs/omarchy-quickstart.md` (`theme-set.d`
///   hook directory).
/// * `.local/bin`: issue #402 Gap ("scripts on PATH such as ~/.local/bin");
///   `docs/omarchy-inventory.json` summary of a command that installs into ~/.local/bin.
///
/// The hook root is special: its files are `hook` entries and its subdirectories must
/// be `<name>.d` directories, whose files are `hook_d` entries.
pub const ROOTS: &[(&str, EntryKind)] = &[
    (".config/omarchy/hooks", EntryKind::Hook),
    (".local/bin", EntryKind::LocalBin),
];

/// Bounds on one scan. [`DEFAULT_LIMITS`] is what [`scan`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most entries listed. The next file sets the `truncated` flag and stops the walk.
    pub max_entries: usize,
    /// Files larger than this are listed as `unknown` and not read.
    pub max_file_bytes: u64,
    /// Directory levels walked below each root. A root's own entries are level 1.
    pub max_depth: usize,
}

/// Generous defaults: a hook script is seldom more than a few kilobytes.
pub const DEFAULT_LIMITS: Limits = Limits {
    max_entries: 1000,
    max_file_bytes: 1024 * 1024,
    max_depth: 4,
};

/// Where an entry was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A file directly in `~/.config/omarchy/hooks`.
    Hook,
    /// A file inside a `~/.config/omarchy/hooks/<name>.d` directory.
    HookD,
    /// A file in `~/.local/bin`.
    LocalBin,
}

impl EntryKind {
    fn label(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::HookD => "hook_d",
            Self::LocalBin => "local_bin",
        }
    }
}

/// The verdict for one entry. `NotAScript` covers files the lexer never reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Portable,
    Partial,
    Blocked,
    /// The file was listed but not read (too large, or unreadable), so no verdict is given.
    Unknown,
    /// No shell shebang and no `.sh` extension, or a shebang naming a non-shell program.
    NotAScript,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Self::Portable => "portable",
            Self::Partial => "partial",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
            Self::NotAScript => "not_a_script",
        }
    }
}

/// One feature that a script uses, with the category and the need from the Python table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeatureUse {
    pub name: &'static str,
    pub category: &'static str,
    pub need: &'static str,
}

/// One file under a root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Path relative to the home directory, with `/` separators.
    pub path: String,
    pub kind: EntryKind,
    pub verdict: Verdict,
    /// Why the verdict was given, when it is not obvious from the features.
    pub reason: Option<String>,
    /// True when the file had CRLF line endings, which were normalised before lexing.
    pub crlf: bool,
    /// Features that the script uses, one per name.
    pub features: Vec<FeatureUse>,
    /// External commands the script runs, sorted and without duplicates.
    pub external_commands: Vec<String>,
}

impl Entry {
    fn unknown(path: &str, kind: EntryKind, reason: String) -> Self {
        Self {
            path: path.to_string(),
            kind,
            verdict: Verdict::Unknown,
            reason: Some(reason),
            crlf: false,
            features: Vec::new(),
            external_commands: Vec::new(),
        }
    }
}

/// A link that leaves the home, an unreadable file, or a limit that was reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// One of `link_leaves_home`, `link_not_followed`, `unreadable`, `not_walked`,
    /// `depth_limit`, `truncated`.
    pub kind: &'static str,
    pub path: String,
    pub detail: String,
}

/// Counts of entries by verdict.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub entries: usize,
    pub portable: usize,
    pub partial: usize,
    pub blocked: usize,
    pub unknown: usize,
    pub not_a_script: usize,
}

/// The whole result of one scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanReport {
    pub protocol: &'static str,
    /// The home directory as given to the scan.
    pub home: String,
    pub totals: Totals,
    /// True when the entry cap stopped the walk.
    pub truncated: bool,
    pub entries: Vec<Entry>,
    pub findings: Vec<Finding>,
}

/// Scan `home` with [`DEFAULT_LIMITS`].
pub fn scan(home: &Path) -> Result<ScanReport, String> {
    scan_with_limits(home, DEFAULT_LIMITS)
}

/// Scan `home` with explicit limits. Fails only when `home` is not a directory.
pub fn scan_with_limits(home: &Path, limits: Limits) -> Result<ScanReport, String> {
    let canonical = fs::canonicalize(home)
        .map_err(|error| format!("cannot read home {}: {error}", home.display()))?;
    if !canonical.is_dir() {
        return Err(format!("home {} is not a directory", home.display()));
    }
    let mut scanner = Scanner {
        home: canonical,
        limits,
        entries: Vec::new(),
        findings: Vec::new(),
        truncated: false,
    };
    for &(rel, kind) in ROOTS {
        scanner.scan_root(home, rel, kind);
    }
    let totals = totals(&scanner.entries);
    Ok(ScanReport {
        protocol: PROTOCOL,
        home: home.display().to_string(),
        totals,
        truncated: scanner.truncated,
        entries: scanner.entries,
        findings: scanner.findings,
    })
}

fn totals(entries: &[Entry]) -> Totals {
    let mut totals = Totals {
        entries: entries.len(),
        ..Totals::default()
    };
    for entry in entries {
        match entry.verdict {
            Verdict::Portable => totals.portable += 1,
            Verdict::Partial => totals.partial += 1,
            Verdict::Blocked => totals.blocked += 1,
            Verdict::Unknown => totals.unknown += 1,
            Verdict::NotAScript => totals.not_a_script += 1,
        }
    }
    totals
}

struct Scanner {
    /// The home directory, canonicalised, for the "leaves the home" comparison.
    home: PathBuf,
    limits: Limits,
    entries: Vec<Entry>,
    findings: Vec<Finding>,
    truncated: bool,
}

impl Scanner {
    fn finding(&mut self, kind: &'static str, path: &str, detail: impl Into<String>) {
        self.findings.push(Finding {
            kind,
            path: path.to_string(),
            detail: detail.into(),
        });
    }

    fn unreadable(&mut self, rel: &str, error: &io::Error) {
        self.finding("unreadable", rel, error.to_string());
    }

    fn scan_root(&mut self, home: &Path, rel: &str, kind: EntryKind) {
        let root = home.join(rel);
        let target = match fs::canonicalize(&root) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.unreadable(rel, &error);
                return;
            }
        };
        if !target.starts_with(&self.home) {
            let detail = format!("resolves to {}; not read", target.display());
            self.finding("link_leaves_home", rel, detail);
            return;
        }
        if !target.is_dir() {
            self.finding("unreadable", rel, "not a directory");
            return;
        }
        self.visit_dir(&root, rel, 1, kind, kind == EntryKind::Hook);
    }

    /// Lists one directory in name order. `hook_root` is true for the hooks root, whose
    /// subdirectories must be `<name>.d` directories.
    fn visit_dir(&mut self, dir: &Path, rel: &str, depth: usize, kind: EntryKind, hook_root: bool) {
        let read = match fs::read_dir(dir) {
            Ok(read) => read,
            Err(error) => {
                self.unreadable(rel, &error);
                return;
            }
        };
        let mut names: Vec<OsString> = Vec::new();
        for item in read {
            match item {
                Ok(item) => names.push(item.file_name()),
                Err(error) => self.unreadable(rel, &error),
            }
        }
        names.sort();
        for name in names {
            if self.truncated {
                return;
            }
            let name = name.to_string_lossy().into_owned();
            let child_rel = format!("{rel}/{name}");
            let path = dir.join(&name);
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) => {
                    self.unreadable(&child_rel, &error);
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                self.symlink(&path, &child_rel, kind);
            } else if meta.is_dir() {
                if hook_root && !name.ends_with(".d") {
                    self.finding(
                        "not_walked",
                        &child_rel,
                        "not a hook directory: the name does not end in .d",
                    );
                    continue;
                }
                if depth >= self.limits.max_depth {
                    let detail =
                        format!("not walked: deeper than {} levels", self.limits.max_depth);
                    self.finding("depth_limit", &child_rel, detail);
                    continue;
                }
                let child_kind = if hook_root { EntryKind::HookD } else { kind };
                self.visit_dir(&path, &child_rel, depth + 1, child_kind, false);
            } else if meta.is_file() {
                self.file(&path, &child_rel, kind);
            } else {
                self.finding("not_walked", &child_rel, "not a regular file or directory");
            }
        }
    }

    /// A symbolic link is resolved and compared with the home. A link that resolves
    /// outside the home is a finding and is never read. A link to a file inside the
    /// home is read at its target. A link to a directory is not walked.
    fn symlink(&mut self, path: &Path, rel: &str, kind: EntryKind) {
        match fs::canonicalize(path) {
            Ok(target) if target.starts_with(&self.home) => {
                if target.is_file() {
                    self.file(&target, rel, kind);
                } else {
                    self.finding(
                        "link_not_followed",
                        rel,
                        "symbolic link to a directory; not walked",
                    );
                }
            }
            Ok(target) => {
                let detail = format!("symbolic link resolves to {}; not read", target.display());
                self.finding("link_leaves_home", rel, detail);
            }
            Err(error) => self.unreadable(rel, &error),
        }
    }

    fn file(&mut self, read_path: &Path, rel: &str, kind: EntryKind) {
        let max_entries = self.limits.max_entries;
        if self.entries.len() >= max_entries {
            self.truncated = true;
            let detail = format!("more than {max_entries} entries; the rest are not listed");
            self.finding("truncated", rel, detail);
            return;
        }
        let size = match fs::metadata(read_path) {
            Ok(meta) => meta.len(),
            Err(error) => {
                self.unreadable(rel, &error);
                let reason = format!("could not be read: {error}");
                self.entries.push(Entry::unknown(rel, kind, reason));
                return;
            }
        };
        let max_bytes = self.limits.max_file_bytes;
        if size > max_bytes {
            let reason = format!("larger than {max_bytes} bytes; not read");
            self.entries.push(Entry::unknown(rel, kind, reason));
            return;
        }
        match fs::read(read_path) {
            Ok(bytes) => self.entries.push(classify_bytes(rel, kind, &bytes)),
            Err(error) => {
                self.unreadable(rel, &error);
                let reason = format!("could not be read: {error}");
                self.entries.push(Entry::unknown(rel, kind, reason));
            }
        }
    }
}

/// Classify one file from its bytes. Never executes it.
fn classify_bytes(path: &str, kind: EntryKind, bytes: &[u8]) -> Entry {
    let decoded = String::from_utf8_lossy(bytes);
    // Python tool lines 611-614: CRLF is normalised before lexing and reported.
    let crlf = decoded.contains("\r\n");
    let source = decoded.replace("\r\n", "\n");
    let mut entry = Entry {
        path: path.to_string(),
        kind,
        verdict: Verdict::Portable,
        reason: None,
        crlf,
        features: Vec::new(),
        external_commands: Vec::new(),
    };
    if let Some(reason) = not_script_reason(path, &source) {
        entry.verdict = Verdict::NotAScript;
        entry.reason = Some(reason);
        return entry;
    }
    match analyze_source(&source) {
        Ok(analysis) => {
            entry.verdict = verdict_for(&analysis.features);
            entry.features = analysis
                .features
                .iter()
                .filter_map(|name| {
                    feature_info(name).map(|(category, need)| FeatureUse {
                        name,
                        category,
                        need,
                    })
                })
                .collect();
            entry.external_commands = analysis.external_commands;
        }
        Err(error) => {
            entry.verdict = Verdict::Blocked;
            entry.reason = Some(format!("lex error: {error}"));
        }
    }
    entry
}

/// Scripts are the files whose shebang names a shell, or that have a `.sh` extension
/// and no shebang. Python tool lines 621-634 decide the same way for a shebang.
/// Deviation: `zsh` is accepted here. The Python set is `bash, sh, dash, ash`.
fn not_script_reason(path: &str, source: &str) -> Option<String> {
    match shebang_interpreter(source) {
        Some(interpreter) if SHELL_INTERPRETERS.contains(&interpreter.as_str()) => None,
        Some(interpreter) => Some(format!(
            "shebang names `{interpreter}`, which is not a shell"
        )),
        None if path.ends_with(".sh") => None,
        None => Some("no shell shebang and no .sh extension".to_string()),
    }
}

/// Python tool lines 574-585: the interpreter named on the first line, with `env` resolved.
fn shebang_interpreter(source: &str) -> Option<String> {
    let first = source.split('\n').next().unwrap_or("");
    let words: Vec<&str> = first.strip_prefix("#!")?.split_whitespace().collect();
    let mut interpreter = basename(words.first()?);
    if interpreter == "env" {
        if let Some(next) = words.get(1) {
            interpreter = basename(next);
        }
    }
    Some(interpreter.to_string())
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Python tool lines 555-563: any unsupported feature blocks a direct port, any partial
/// feature makes it partial, and otherwise it is portable.
fn verdict_for(features: &BTreeSet<&'static str>) -> Verdict {
    let category_of = |name: &str| feature_info(name).map(|(category, _)| category);
    if features
        .iter()
        .any(|name| category_of(name) == Some("unsupported"))
    {
        Verdict::Blocked
    } else if features
        .iter()
        .any(|name| category_of(name) == Some("partial"))
    {
        Verdict::Partial
    } else {
        Verdict::Portable
    }
}

/// Feature name -> (category, what Execute offers instead). Python tool lines 44-72.
const FEATURES: &[(&str, &str, &str)] = &[
    ("heredoc", "partial", "a quoted body maps to a triple-quoted raw block with use value, then the command; an expanding body needs its values as bindings"),
    ("process_substitution", "unsupported", "no process substitution; use separate programs"),
    ("trap", "partial", "on exit and on interrupt cover EXIT and INT cleanup; other signals have no equivalent"),
    ("eval", "unsupported", "no string evaluation; write the command as a program"),
    ("source", "partial", "run program file runs another file in process with explicit inputs; shared variables do not cross, and KEY=VALUE env files load with env.load"),
    ("exec_builtin", "partial", "exec replaces the shell; a final exec ports to hand over to, which must be the last statement"),
    ("coproc", "unsupported", "no coprocesses; use listen or stream forms"),
    ("background_job", "partial", "spawn runs owned named jobs; launch detached starts an unowned process; & has no direct equivalent"),
    ("associative_array", "unsupported", "declare -A has no equivalent"),
    ("regex_match", "partial", "needs a text regex applet; Execute compares text exactly"),
    ("read_builtin", "partial", "needs accept or listen input parsing"),
    ("getopts", "partial", "needs an argument parser; use accept with arguments"),
    ("array_assign", "partial", "needs list values: accept as list of text or a list literal"),
    ("array_expansion", "partial", "needs list values: accept as list of text"),
    ("parameter_expansion", "partial", "needs text applets (default, strip, replace)"),
    ("arithmetic", "partial", "needs number applets"),
    ("local_variable", "partial", "locals are per reusable program, not per shell function"),
    ("ansi_quote", "partial", "needs literal text; ANSI escape rules differ"),
    ("command_substitution", "supported", "capture with as name"),
    ("function_definition", "supported", "reusable programs"),
    ("loop", "supported", "for each, repeat or while"),
    ("conditional", "supported", "if and otherwise"),
    ("pipeline", "supported", "pipe to or then"),
    ("redirection", "supported", "then save, discard stderr or direct argv"),
    ("positional_arguments", "supported", "accept arguments as list of text from arguments"),
    ("environment_variable", "supported", "accept from environment"),
    ("privileged_command", "supported", "declare effects and capabilities in a manifest"),
];

/// Python tool lines 74-77.
const PRIVILEGED: &[&str] = &[
    "sudo",
    "pkexec",
    "doas",
    "systemctl",
    "pacman",
    "yay",
    "paru",
    "apt",
    "apt-get",
    "dnf",
    "flatpak",
    "snap",
    "mount",
    "umount",
    "chown",
    "chmod",
];

/// Python tool lines 78-88.
const SHELL_BUILTIN_FEATURES: &[(&str, &str)] = &[
    ("read", "read_builtin"),
    ("getopts", "getopts"),
    ("local", "local_variable"),
    ("trap", "trap"),
    ("eval", "eval"),
    ("source", "source"),
    (".", "source"),
    ("exec", "exec_builtin"),
    ("coproc", "coproc"),
];

/// Python tool lines 89-93.
const NON_EXTERNAL: &[&str] = &[
    "declare", "typeset", "printf", "echo", "test", "[", "true", "false", "cd", "set", "shift",
    "export", "unset", "return", "exit", "break", "continue", "wait", "jobs", "kill", "pwd", "let",
    "umask", "alias", "unalias", "readonly", "shopt",
];

/// Python tool lines 95-98. Longest first, so `<<<` and `<<-` are not read as `<<`.
const OPERATORS: &[&str] = &[
    "&>>", "<<<", "<<-", ";;", "&&", "||", "|&", ">>", "<<", "<&", ">&", "<>", ">|", "&>", "|",
    "&", ";", "(", ")", "<", ">",
];

/// Python tool line 99.
const REDIRECTIONS: &[&str] = &[
    "&>>", "<<<", "<<-", ">>", "<<", "<&", ">&", "<>", ">|", "&>", "<", ">",
];

/// Python tool line 100.
const COMMAND_SEPARATORS: &[&str] = &[";", ";;", "&&", "||", "|", "|&", "&", "(", ")"];

/// Python tool line 101.
const COMMAND_OPENERS: &[&str] = &[
    "then", "do", "else", "elif", "if", "while", "until", "!", "{", "time",
];

/// Python tool line 104, plus `zsh` (see [`not_script_reason`]).
const SHELL_INTERPRETERS: &[&str] = &["bash", "sh", "dash", "ash", "zsh"];

fn feature_info(name: &str) -> Option<(&'static str, &'static str)> {
    FEATURES
        .iter()
        .find(|entry| entry.0 == name)
        .map(|entry| (entry.1, entry.2))
}

fn shell_builtin_feature(word: &str) -> Option<&'static str> {
    SHELL_BUILTIN_FEATURES
        .iter()
        .find(|entry| entry.0 == word)
        .map(|entry| entry.1)
}

/// Python tool lines 107-120, as a Rust error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokKind {
    Word,
    Op,
    Newline,
    ProcSub,
    Arith,
    Eof,
}

/// Python tool lines 113-119. Only the text and the features are kept: the verdict does
/// not depend on line numbers.
#[derive(Debug, Clone)]
struct Token {
    kind: TokKind,
    text: String,
    features: BTreeSet<&'static str>,
}

fn token(kind: TokKind, text: String) -> Token {
    Token {
        kind,
        text,
        features: BTreeSet::new(),
    }
}

/// Python tool line 36 limits nesting only through the Python recursion limit. This
/// port keeps the nesting bound explicit (see [`MAX_NESTING`]).
struct Lexer {
    s: Vec<char>,
    i: usize,
    tokens: Vec<Token>,
    /// Pending here-documents: (delimiter, strip leading tabs). Their bodies are skipped
    /// at the next newline.
    pending: Vec<(String, bool)>,
    newlines: Vec<usize>,
    depth: usize,
}

impl Lexer {
    fn new(source: &str) -> Self {
        let s: Vec<char> = source.chars().collect();
        let newlines = s
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == '\n')
            .map(|(index, _)| index)
            .collect();
        Self {
            s,
            i: 0,
            tokens: Vec::new(),
            pending: Vec::new(),
            newlines,
            depth: 0,
        }
    }

    fn len(&self) -> usize {
        self.s.len()
    }

    fn ch(&self, at: usize) -> Option<char> {
        self.s.get(at).copied()
    }

    /// Python slicing: both ends clamp to the input.
    fn text(&self, start: usize, end: usize) -> String {
        let end = end.min(self.len());
        let start = start.min(end);
        self.s
            .get(start..end)
            .map(|part| part.iter().collect())
            .unwrap_or_default()
    }

    fn starts_with(&self, pattern: &str, at: usize) -> bool {
        pattern
            .chars()
            .enumerate()
            .all(|(k, c)| self.ch(at + k) == Some(c))
    }

    fn find(&self, target: char, from: usize) -> Option<usize> {
        (from..self.len()).find(|&k| self.ch(k) == Some(target))
    }

    /// Python tool lines 130-132.
    fn line_of(&self, pos: usize) -> usize {
        self.newlines.partition_point(|&n| n < pos) + 1
    }

    fn err(&self, at: usize, message: impl Into<String>) -> LexError {
        LexError {
            line: self.line_of(at),
            message: message.into(),
        }
    }

    fn enter(&mut self, at: usize) -> Result<(), LexError> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return Err(self.err(at, "nesting is too deep"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Python tool lines 134-167.
    fn run(mut self) -> Result<Vec<Token>, LexError> {
        loop {
            self.skip_blanks();
            let Some(c) = self.ch(self.i) else { break };
            if c == '\n' {
                self.newline();
                continue;
            }
            if c == '#' {
                self.comment();
                continue;
            }
            if (c == '<' || c == '>') && self.ch(self.i + 1) == Some('(') {
                self.procsub()?;
                continue;
            }
            if self.starts_with("((", self.i) && self.command_start() {
                let start = self.i;
                let end = self.balanced(start, '(', ')')?;
                let text = self.text(start, end);
                self.tokens.push(Token {
                    kind: TokKind::Arith,
                    text,
                    features: BTreeSet::from(["arithmetic"]),
                });
                self.i = end;
                continue;
            }
            if let Some(op) = self.match_operator() {
                self.tokens.push(token(TokKind::Op, op.to_string()));
                self.i += op.len();
                continue;
            }
            self.word()?;
        }
        self.tokens.push(token(TokKind::Eof, String::new()));
        Ok(self.tokens)
    }

    /// Python tool lines 169-179.
    fn command_start(&self) -> bool {
        match self.tokens.last() {
            None => true,
            Some(last) => match last.kind {
                TokKind::Newline | TokKind::Eof => true,
                TokKind::Op => COMMAND_SEPARATORS.contains(&last.text.as_str()),
                TokKind::Word => matches!(
                    last.text.as_str(),
                    "if" | "elif" | "while" | "until" | "then" | "do" | "else"
                ),
                TokKind::ProcSub | TokKind::Arith => false,
            },
        }
    }

    /// Python tool lines 181-189.
    fn skip_blanks(&mut self) {
        loop {
            match self.ch(self.i) {
                Some(' ') | Some('\t') => self.i += 1,
                Some('\\') if self.ch(self.i + 1) == Some('\n') => self.i += 2,
                _ => break,
            }
        }
    }

    /// Python tool lines 191-193.
    fn comment(&mut self) {
        self.i = self.find('\n', self.i).unwrap_or(self.len());
    }

    /// Python tool lines 195-199.
    fn match_operator(&self) -> Option<&'static str> {
        OPERATORS
            .iter()
            .find(|op| self.starts_with(op, self.i))
            .copied()
    }

    /// Python tool lines 201-218. Here-document bodies end at a line equal to the
    /// delimiter (after tab stripping for `<<-`).
    fn newline(&mut self) {
        self.tokens.push(token(TokKind::Newline, "\n".to_string()));
        self.i += 1;
        let pending = std::mem::take(&mut self.pending);
        for (delimiter, strip_tabs) in pending {
            while self.i < self.len() {
                let end = self.find('\n', self.i).unwrap_or(self.len());
                let text = self.text(self.i, end);
                self.i = (end + 1).min(self.len());
                let candidate = if strip_tabs {
                    text.trim_start_matches('\t')
                } else {
                    text.as_str()
                };
                if candidate == delimiter.as_str() {
                    break;
                }
            }
        }
    }

    /// Python tool lines 220-226.
    fn procsub(&mut self) -> Result<(), LexError> {
        let start = self.i;
        let end = self.balanced(start + 1, '(', ')')?;
        let text = self.text(start, end);
        self.tokens.push(Token {
            kind: TokKind::ProcSub,
            text,
            features: BTreeSet::from(["process_substitution"]),
        });
        self.i = end;
        Ok(())
    }

    /// Python tool lines 228-261.
    fn balanced(&mut self, start: usize, open: char, close: char) -> Result<usize, LexError> {
        self.enter(start)?;
        let result = self.balanced_inner(start, open, close);
        self.leave();
        result
    }

    fn balanced_inner(&mut self, start: usize, open: char, close: char) -> Result<usize, LexError> {
        let len = self.len();
        let mut i = start;
        while i < len && self.ch(i) != Some(open) {
            i += 1;
        }
        if i >= len {
            return Err(self.err(start, format!("expected {open:?}")));
        }
        let mut depth: i64 = 0;
        while let Some(c) = self.ch(i) {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '\'' {
                let end = self
                    .find('\'', i + 1)
                    .ok_or_else(|| self.err(i, "unterminated single quote"))?;
                i = end + 1;
                continue;
            }
            if c == '"' {
                i = self.double_end(i)?;
                continue;
            }
            if c == '#' && (i == 0 || matches!(self.ch(i - 1), Some(' ' | '\t' | '\n' | ';'))) {
                i = self.find('\n', i).unwrap_or(len);
                continue;
            }
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Ok(i + 1);
                }
            }
            i += 1;
        }
        Err(self.err(start, format!("unterminated {open:?} group")))
    }

    /// Python tool lines 263-279.
    fn double_end(&mut self, start: usize) -> Result<usize, LexError> {
        self.enter(start)?;
        let result = self.double_end_inner(start);
        self.leave();
        result
    }

    fn double_end_inner(&mut self, start: usize) -> Result<usize, LexError> {
        let mut i = start + 1;
        while let Some(c) = self.ch(i) {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                return Ok(i + 1);
            }
            if c == '$' && self.ch(i + 1) == Some('(') {
                i = self.balanced(i + 1, '(', ')')?;
                continue;
            }
            if c == '`' {
                i = self.backtick_end(i)?;
                continue;
            }
            i += 1;
        }
        Err(self.err(start, "unterminated double quote"))
    }

    /// Python tool lines 281-290.
    fn backtick_end(&self, start: usize) -> Result<usize, LexError> {
        let mut i = start + 1;
        while let Some(c) = self.ch(i) {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '`' {
                return Ok(i + 1);
            }
            i += 1;
        }
        Err(self.err(start, "unterminated backtick"))
    }

    /// Python tool lines 292-356.
    fn word(&mut self) -> Result<(), LexError> {
        let start = self.i;
        let mut features: BTreeSet<&'static str> = BTreeSet::new();
        let mut text = String::new();
        while let Some(c) = self.ch(self.i) {
            if matches!(
                c,
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>'
            ) {
                break;
            }
            if c == '\\' {
                if self.ch(self.i + 1) == Some('\n') {
                    self.i += 2;
                    continue;
                }
                text.push_str(&self.text(self.i, self.i + 2));
                self.i += 2;
                continue;
            }
            if c == '\'' {
                let end = self
                    .find('\'', self.i + 1)
                    .ok_or_else(|| self.err(self.i, "unterminated single quote"))?;
                text.push_str(&self.text(self.i, end + 1));
                self.i = end + 1;
                continue;
            }
            if c == '"' {
                let end = self.double_end(self.i)?;
                self.scan_dollars(self.i + 1, end.saturating_sub(1), &mut features)?;
                text.push_str(&self.text(self.i, end));
                self.i = end;
                continue;
            }
            if c == '`' {
                let end = self.backtick_end(self.i)?;
                features.insert("command_substitution");
                text.push_str(&self.text(self.i, end));
                self.i = end;
                continue;
            }
            if c == '$' {
                let end = self.dollar_end(self.i, &mut features)?;
                text.push_str(&self.text(self.i, end));
                self.i = end;
                continue;
            }
            if c == '=' && self.ch(self.i + 1) == Some('(') {
                // Python tool lines 336-343: an array assignment such as `files=(a b)`.
                let mut with_eq: Vec<char> = text.chars().collect();
                with_eq.push('=');
                if assignment_len(&with_eq) == Some(with_eq.len()) && !text.ends_with('=') {
                    let end = self.balanced(self.i + 1, '(', ')')?;
                    features.insert("array_assign");
                    text.push_str(&self.text(self.i, end));
                    self.i = end;
                    break;
                }
            }
            text.push(c);
            self.i += 1;
        }
        if self.i == start && start < self.len() {
            // Python tool lines 346-349: never loop on a character the rules do not consume.
            text = self.text(start, start + 1);
            self.i = start + 1;
        }
        // Python tool lines 351-355: the word after `<<` or `<<-` is a here-document delimiter.
        let previous_op = self
            .tokens
            .last()
            .filter(|prev| prev.kind == TokKind::Op)
            .map(|prev| prev.text.clone());
        if matches!(previous_op.as_deref(), Some("<<") | Some("<<-")) {
            let delimiter: String = text
                .chars()
                .filter(|c| !matches!(c, '"' | '\'' | '\\'))
                .collect();
            self.pending
                .push((delimiter, previous_op.as_deref() == Some("<<-")));
            features.insert("heredoc");
        }
        self.tokens.push(Token {
            kind: TokKind::Word,
            text,
            features,
        });
        Ok(())
    }

    /// Python tool lines 358-365: records expansions inside a double-quoted segment.
    fn scan_dollars(
        &mut self,
        start: usize,
        stop: usize,
        features: &mut BTreeSet<&'static str>,
    ) -> Result<(), LexError> {
        let mut i = start;
        while i < stop {
            if self.ch(i) == Some('$') {
                i = self.dollar_end(i, features)?.min(stop);
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    /// Python tool lines 367-408: the end of a `$` expansion that begins at `start`.
    fn dollar_end(
        &mut self,
        start: usize,
        features: &mut BTreeSet<&'static str>,
    ) -> Result<usize, LexError> {
        self.enter(start)?;
        let result = self.dollar_end_inner(start, features);
        self.leave();
        result
    }

    fn dollar_end_inner(
        &mut self,
        start: usize,
        features: &mut BTreeSet<&'static str>,
    ) -> Result<usize, LexError> {
        let next = self.ch(start + 1);
        match next {
            // Python tool lines 370-378.
            Some('(') => {
                let arithmetic = self.ch(start + 2) == Some('(');
                features.insert(if arithmetic {
                    "arithmetic"
                } else {
                    "command_substitution"
                });
                let end = self.balanced(start + 1, '(', ')')?;
                self.scan_dollars(start + 2, end.saturating_sub(1), features)?;
                Ok(end)
            }
            // Python tool lines 379-384.
            Some('{') => {
                features.insert("parameter_expansion");
                let end = self.balanced(start + 1, '{', '}')?;
                let body = self.text(start + 2, end.saturating_sub(1));
                if body.contains("[@]") || body.contains("[*]") {
                    features.insert("array_expansion");
                }
                Ok(end)
            }
            // Python tool lines 385-390: ANSI-C quoting.
            Some('\'') => {
                let mut end = start + 2;
                while end < self.len() && self.ch(end) != Some('\'') {
                    end += if self.ch(end) == Some('\\') { 2 } else { 1 };
                }
                features.insert("ansi_quote");
                Ok((end + 1).min(self.len()))
            }
            // Python tool lines 391-392.
            Some('"') => self.double_end(start + 1),
            _ => {
                // Python tool lines 393-399: a name; upper-case names are environment variables.
                if let Some(name_end) = self.name_end(start + 1) {
                    let name = self.text(start + 1, name_end);
                    if name.chars().any(|c| c.is_ascii_uppercase()) {
                        features.insert("environment_variable");
                    }
                    return Ok(name_end);
                }
                // Python tool lines 400-407: digits, `@`, `*` and `#` are positional.
                match next {
                    Some(c) if c.is_numeric() || matches!(c, '@' | '*' | '#') => {
                        features.insert("positional_arguments");
                        Ok(start + 2)
                    }
                    Some(_) => Ok(start + 2),
                    None => Ok(start + 1),
                }
            }
        }
    }

    /// `[A-Za-z_][A-Za-z0-9_]*` at `at`, as Python `NAME.match`: the end index.
    fn name_end(&self, at: usize) -> Option<usize> {
        if !self
            .ch(at)
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return None;
        }
        let mut end = at + 1;
        while self
            .ch(end)
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            end += 1;
        }
        Some(end)
    }
}

/// The length of a Bash assignment prefix at the start of `s`, matching the Python
/// `ASSIGNMENT` regex `[A-Za-z_][A-Za-z0-9_]*(\[[^\]]*\])?\+?=` (Python tool line 103).
/// The pattern is deterministic, so no backtracking is needed.
fn assignment_len(s: &[char]) -> Option<usize> {
    if !s
        .first()
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
    {
        return None;
    }
    let mut i = 1;
    while s
        .get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
    {
        i += 1;
    }
    if s.get(i) == Some(&'[') {
        let close = s.get(i + 1..)?.iter().position(|c| *c == ']')?;
        i = i + 1 + close + 1;
    }
    if s.get(i) == Some(&'+') {
        i += 1;
    }
    if s.get(i) == Some(&'=') {
        Some(i + 1)
    } else {
        None
    }
}

/// What the command parser found in one script.
#[derive(Debug, Default)]
struct Analysis {
    features: BTreeSet<&'static str>,
    external_commands: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaseState {
    Subject,
    Pattern,
    Body,
}

/// Python tool lines 415-571: features and external commands of one shell script.
fn analyze_source(source: &str) -> Result<Analysis, LexError> {
    let tokens = Lexer::new(source).run()?;
    let mut features: BTreeSet<&'static str> = BTreeSet::new();

    // Python tool lines 423-431: token features, redirections, background jobs and pipes.
    for tok in &tokens {
        features.extend(tok.features.iter().copied());
        if tok.kind == TokKind::Op {
            if REDIRECTIONS.contains(&tok.text.as_str()) {
                features.insert("redirection");
            }
            if tok.text == "&" {
                features.insert("background_job");
            }
            if tok.text == "|" {
                features.insert("pipeline");
            }
        }
    }

    // Python tool lines 433-550: the command parser.
    let mut commands: Vec<String> = Vec::new();
    let mut functions: BTreeSet<String> = BTreeSet::new();
    // One entry per open `case`, as in Python: subject before `in`, labels, then bodies.
    let mut case_states: Vec<CaseState> = Vec::new();
    let mut command_start = true;
    let mut index = 0usize;
    while let Some(tok) = tokens.get(index) {
        let word = tok.text.as_str();
        match tok.kind {
            TokKind::Newline | TokKind::Eof => {
                command_start = true;
                index += 1;
                continue;
            }
            TokKind::Op => {
                // Python tool lines 446-453.
                match (case_states.last().copied(), word) {
                    (Some(CaseState::Pattern), ")") => {
                        if let Some(top) = case_states.last_mut() {
                            *top = CaseState::Body;
                        }
                    }
                    (Some(CaseState::Body), ";;") => {
                        if let Some(top) = case_states.last_mut() {
                            *top = CaseState::Pattern;
                        }
                    }
                    _ => {}
                }
                command_start = COMMAND_SEPARATORS.contains(&word);
                index += 1;
                continue;
            }
            TokKind::ProcSub | TokKind::Arith => {
                command_start = false;
                index += 1;
                continue;
            }
            TokKind::Word => {}
        }

        // Python tool lines 459-463: `in` closes a case subject.
        if case_states.last() == Some(&CaseState::Subject) && word == "in" {
            if let Some(top) = case_states.last_mut() {
                *top = CaseState::Pattern;
            }
            command_start = true;
            index += 1;
            continue;
        }
        // Python tool lines 464-467: a case label such as `start|on)` is a pattern, not a command.
        if case_states.last() == Some(&CaseState::Pattern) && command_start && word != "esac" {
            index += 1;
            continue;
        }
        // Python tool lines 468-474: conditionals and loops that open with a keyword.
        if command_start && matches!(word, "if" | "elif" | "while" | "until" | "case") {
            let category = if matches!(word, "if" | "elif" | "case") {
                "conditional"
            } else {
                "loop"
            };
            features.insert(category);
            if word == "case" {
                case_states.push(CaseState::Subject);
            }
            command_start = word != "case";
            index += 1;
            continue;
        }
        // Python tool lines 475-479.
        if command_start && matches!(word, "for" | "select") {
            features.insert("loop");
            command_start = false;
            index += 1;
            continue;
        }
        // Python tool lines 480-489: `[[ ... ]]` conditions, scanned for `=~`.
        if command_start && word == "[[" {
            features.insert("conditional");
            let mut j = index + 1;
            while let Some(next) = tokens.get(j) {
                if next.kind == TokKind::Eof || (next.kind == TokKind::Word && next.text == "]]") {
                    break;
                }
                if next.kind == TokKind::Word && next.text == "=~" {
                    features.insert("regex_match");
                }
                j += 1;
            }
            index = j + 1;
            command_start = false;
            continue;
        }
        // Python tool lines 490-496: `function name`.
        if command_start && word == "function" {
            features.insert("function_definition");
            if let Some(name) = tokens.get(index + 1).filter(|t| t.kind == TokKind::Word) {
                functions.insert(name.text.clone());
            }
            command_start = false;
            index += 1;
            continue;
        }
        // Python tool lines 497-500.
        if command_start && COMMAND_OPENERS.contains(&word) {
            command_start = true;
            index += 1;
            continue;
        }
        // Python tool lines 501-506: `exec CMD` replaces the shell with CMD.
        if command_start && word == "exec" {
            features.insert("exec_builtin");
            command_start = true;
            index += 1;
            continue;
        }
        // Python tool lines 507-512.
        if command_start && matches!(word, "esac" | "in" | "fi" | "done" | "}") {
            if word == "esac" {
                case_states.pop();
            }
            command_start = matches!(word, "in" | "}");
            index += 1;
            continue;
        }
        // Python tool lines 513-526: `name ( )` defines a function.
        let defines_function = command_start
            && tokens
                .get(index + 1)
                .is_some_and(|t| t.kind == TokKind::Op && t.text == "(")
            && tokens
                .get(index + 2)
                .is_some_and(|t| t.kind == TokKind::Op && t.text == ")")
            && is_name(word);
        if defines_function {
            features.insert("function_definition");
            functions.insert(word.to_string());
            index += 3;
            command_start = true;
            continue;
        }
        // Python tool lines 527-529: an assignment before a command is skipped.
        if command_start && assignment_len(&word.chars().collect::<Vec<char>>()).is_some() {
            index += 1;
            continue;
        }
        // Python tool lines 530-548: a command word.
        if command_start {
            commands.push(word.to_string());
            if PRIVILEGED.contains(&word) {
                features.insert("privileged_command");
            }
            if let Some(feature) = shell_builtin_feature(word) {
                features.insert(feature);
            }
            if word == "declare" || word == "typeset" {
                let mut j = index + 1;
                let mut flags: Vec<&str> = Vec::new();
                while let Some(flag) = tokens.get(j) {
                    if flag.kind != TokKind::Word || !flag.text.starts_with('-') {
                        break;
                    }
                    flags.push(flag.text.as_str());
                    j += 1;
                }
                if flags.iter().any(|flag| flag.contains('A')) {
                    features.insert("associative_array");
                }
                if flags.iter().any(|flag| flag.contains('a')) {
                    features.insert("array_assign");
                }
            }
            command_start = false;
            index += 1;
            continue;
        }
        command_start = false;
        index += 1;
    }

    // Python tool lines 552-554: external commands exclude builtins, non-external names and functions.
    let mut external_commands: BTreeSet<String> = BTreeSet::new();
    for name in &commands {
        let builtin = shell_builtin_feature(name).is_some();
        if !builtin && !NON_EXTERNAL.contains(&name.as_str()) && !functions.contains(name) {
            external_commands.insert(name.clone());
        }
    }
    Ok(Analysis {
        features,
        external_commands: external_commands.into_iter().collect(),
    })
}

/// True for a bare shell name `[A-Za-z_][A-Za-z0-9_]*`, as Python `NAME.fullmatch`.
fn is_name(word: &str) -> bool {
    let mut chars = word.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The stable `--json` rendering, protocol [`PROTOCOL`].
pub fn render_json(report: &ScanReport) -> Value {
    serde_json::to_value(report).unwrap_or(Value::Null)
}

/// The plain text rendering: one block per entry, then the findings and the totals.
/// Only the features that are not `supported` are shown, since those drove the verdict.
pub fn render_text(report: &ScanReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Omarchy customisation scan of {}", report.home);
    if report.entries.is_empty() {
        let _ = writeln!(out, "no customisation entry points found");
    }
    for entry in &report.entries {
        let _ = writeln!(
            out,
            "{:<13} {}  [{}]",
            entry.verdict.label(),
            entry.path,
            entry.kind.label()
        );
        if let Some(reason) = &entry.reason {
            let _ = writeln!(out, "    reason: {reason}");
        }
        for feature in entry.features.iter().filter(|f| f.category != "supported") {
            let _ = writeln!(
                out,
                "    {} ({}): {}",
                feature.name, feature.category, feature.need
            );
        }
        if !entry.external_commands.is_empty() {
            let _ = writeln!(out, "    commands: {}", entry.external_commands.join(", "));
        }
    }
    for finding in &report.findings {
        let _ = writeln!(
            out,
            "finding {}: {}: {}",
            finding.kind, finding.path, finding.detail
        );
    }
    let totals = &report.totals;
    let _ = writeln!(
        out,
        "{} entries: {} portable, {} partial, {} blocked, {} unknown, {} not a script",
        totals.entries,
        totals.portable,
        totals.partial,
        totals.blocked,
        totals.unknown,
        totals.not_a_script
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(home: &Path, rel: &str, body: &[u8]) {
        let path = home.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture directory");
        }
        fs::write(&path, body).expect("write fixture file");
    }

    fn entry<'a>(report: &'a ScanReport, path: &str) -> &'a Entry {
        report
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("no entry for {path}"))
    }

    fn features_of(source: &str) -> BTreeSet<&'static str> {
        analyze_source(source).expect("lexes").features
    }

    #[test]
    fn heredoc_is_partial_and_cat_is_external_but_echo_is_not() {
        let result =
            analyze_source("cat <<EOF\nline one $(not_run)\nEOF\necho done\n").expect("lexes");
        assert!(result.features.contains("heredoc"));
        assert_eq!(
            classify_bytes(
                "x",
                EntryKind::LocalBin,
                b"#!/bin/bash\ncat <<EOF\nx\nEOF\necho done\n"
            )
            .verdict,
            Verdict::Partial
        );
        assert_eq!(result.external_commands, vec!["cat".to_string()]);
    }

    #[test]
    fn double_angle_inside_quotes_or_comments_is_not_a_here_document() {
        assert!(!features_of("echo \"a << b\"\n").contains("heredoc"));
        assert!(!features_of("# cat <<EOF\necho ok\n").contains("heredoc"));
    }

    #[test]
    fn arithmetic_condition_after_if_is_not_a_command() {
        let result = analyze_source("if (($# == 0)); then exit 1; fi\n").expect("lexes");
        assert!(result.features.contains("arithmetic"));
        assert!(result.external_commands.is_empty());
    }

    #[test]
    fn trap_and_eval_block_a_direct_port() {
        let entry = classify_bytes(
            "x",
            EntryKind::LocalBin,
            b"#!/bin/bash\ntrap 'rm -f x' EXIT\neval \"$cmd\"\n",
        );
        assert_eq!(entry.verdict, Verdict::Blocked);
        let names: Vec<&str> = entry.features.iter().map(|f| f.name).collect();
        assert!(names.contains(&"trap"));
        assert!(names.contains(&"eval"));
    }

    #[test]
    fn exec_names_the_following_command_and_is_partial() {
        let result = analyze_source("exec setsid uwsm-app -- nautilus\n").expect("lexes");
        assert!(result.external_commands.contains(&"setsid".to_string()));
        assert!(result.features.contains("exec_builtin"));
    }

    #[test]
    fn verdict_levels_match_the_python_tests() {
        assert_eq!(
            classify_bytes("a", EntryKind::LocalBin, b"#!/bin/sh\necho hi\n").verdict,
            Verdict::Portable
        );
        assert_eq!(
            classify_bytes("a", EntryKind::LocalBin, b"#!/bin/sh\nx=$((1 + 2))\n").verdict,
            Verdict::Partial
        );
    }

    #[test]
    fn crlf_is_normalised_and_reported() {
        let entry = classify_bytes(
            "x",
            EntryKind::LocalBin,
            b"#!/bin/bash\r\nif (($# == 0)); then\r\n  exit 1\r\nfi\r\n",
        );
        assert!(entry.crlf);
        assert_eq!(entry.verdict, Verdict::Partial);
        assert!(!entry.external_commands.contains(&"$#".to_string()));
    }

    #[test]
    fn function_call_is_not_an_external_command() {
        let result =
            analyze_source("usage() {\n  ls\n}\nusage\nfunction helper {\n  true\n}\nhelper\n")
                .expect("lexes");
        assert_eq!(result.external_commands, vec!["ls".to_string()]);
    }

    #[test]
    fn case_labels_are_not_external_commands() {
        let source = "case \"$1\" in\n  start|on) systemctl start x ;;\n  --clear) rm -f y ;;\n  *) cat z ;;\nesac\n";
        let result = analyze_source(source).expect("lexes");
        assert_eq!(
            result.external_commands,
            vec!["cat".to_string(), "rm".to_string(), "systemctl".to_string()]
        );
    }

    #[test]
    fn python_shebang_is_not_a_script() {
        let entry = classify_bytes(
            "helper",
            EntryKind::LocalBin,
            b"#!/usr/bin/python3\nprint(1)\n",
        );
        assert_eq!(entry.verdict, Verdict::NotAScript);
        assert!(entry.features.is_empty());
    }

    #[test]
    fn nesting_beyond_the_bound_is_a_lex_error() {
        let source = format!("echo {}", "\"$(".repeat(100));
        let error = analyze_source(&source).expect_err("too deep");
        assert!(error.message.contains("nesting"));
    }

    #[test]
    fn assignment_prefix_matches_the_python_regex() {
        let chars = |s: &str| s.chars().collect::<Vec<char>>();
        assert_eq!(assignment_len(&chars("a=")), Some(2));
        assert_eq!(assignment_len(&chars("a+=")), Some(3));
        assert_eq!(assignment_len(&chars("a[1]=")), Some(5));
        assert_eq!(assignment_len(&chars("a+")), None);
        assert_eq!(assignment_len(&chars("1a=")), None);
    }

    #[test]
    fn portable_script_in_local_bin() {
        let home = tempfile::tempdir().expect("tempdir");
        write(home.path(), ".local/bin/tidy", b"#!/bin/sh\necho done\n");
        let report = scan(home.path()).expect("scan");
        let found = entry(&report, ".local/bin/tidy");
        assert_eq!(found.verdict, Verdict::Portable);
        assert_eq!(found.kind, EntryKind::LocalBin);
        assert!(found.external_commands.is_empty());
        assert_eq!(report.totals.portable, 1);
    }

    #[test]
    fn hook_files_and_hook_d_scripts_are_listed_with_their_kinds() {
        let home = tempfile::tempdir().expect("tempdir");
        write(
            home.path(),
            ".config/omarchy/hooks/theme-set",
            b"#!/bin/bash\ncat <<EOF\nx\nEOF\n",
        );
        write(
            home.path(),
            ".config/omarchy/hooks/theme-set.d/10-colors",
            b"#!/bin/bash\nx=$((1 + 2))\n",
        );
        let report = scan(home.path()).expect("scan");
        let hook = entry(&report, ".config/omarchy/hooks/theme-set");
        assert_eq!(hook.kind, EntryKind::Hook);
        assert_eq!(hook.verdict, Verdict::Partial);
        assert_eq!(hook.external_commands, vec!["cat".to_string()]);
        let hook_d = entry(&report, ".config/omarchy/hooks/theme-set.d/10-colors");
        assert_eq!(hook_d.kind, EntryKind::HookD);
        assert_eq!(hook_d.verdict, Verdict::Partial);
        assert_eq!(report.totals.entries, 2);
    }

    #[test]
    fn blocked_partial_and_not_a_script_files_are_told_apart() {
        let home = tempfile::tempdir().expect("tempdir");
        write(
            home.path(),
            ".local/bin/run-cmd",
            b"#!/bin/bash\neval \"$cmd\"\n",
        );
        write(
            home.path(),
            ".local/bin/notes",
            b"plain words, no shebang\n",
        );
        write(home.path(), ".local/bin/checker.sh", b"echo hi\n");
        let report = scan(home.path()).expect("scan");
        assert_eq!(
            entry(&report, ".local/bin/run-cmd").verdict,
            Verdict::Blocked
        );
        assert_eq!(
            entry(&report, ".local/bin/notes").verdict,
            Verdict::NotAScript
        );
        assert_eq!(
            entry(&report, ".local/bin/checker.sh").verdict,
            Verdict::Portable
        );
        assert_eq!(report.totals.blocked, 1);
        assert_eq!(report.totals.not_a_script, 1);
    }

    #[test]
    fn oversized_file_is_unknown_and_not_read() {
        let home = tempfile::tempdir().expect("tempdir");
        write(home.path(), ".local/bin/big", &[b'#'; 64]);
        let limits = Limits {
            max_file_bytes: 8,
            ..DEFAULT_LIMITS
        };
        let report = scan_with_limits(home.path(), limits).expect("scan");
        let found = entry(&report, ".local/bin/big");
        assert_eq!(found.verdict, Verdict::Unknown);
        assert!(found
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("not read")));
        assert_eq!(report.totals.unknown, 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_leaving_home_is_a_finding_and_is_not_read() {
        let home = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        write(outside.path(), "secret.sh", b"#!/bin/sh\necho secret\n");
        fs::create_dir_all(home.path().join(".local/bin")).expect("create bin");
        std::os::unix::fs::symlink(
            outside.path().join("secret.sh"),
            home.path().join(".local/bin/linked"),
        )
        .expect("create link");
        let report = scan(home.path()).expect("scan");
        assert!(report.entries.iter().all(|e| e.path != ".local/bin/linked"));
        let finding = report
            .findings
            .iter()
            .find(|f| f.kind == "link_leaves_home")
            .expect("finding for the leaving link");
        assert_eq!(finding.path, ".local/bin/linked");
    }

    #[test]
    fn empty_home_has_no_entries_and_no_findings() {
        let home = tempfile::tempdir().expect("tempdir");
        let report = scan(home.path()).expect("scan");
        assert!(report.entries.is_empty());
        assert!(report.findings.is_empty());
        assert!(!report.truncated);
        assert_eq!(report.totals, Totals::default());
        assert!(render_text(&report).contains("no customisation entry points found"));
    }

    #[test]
    fn missing_home_is_an_error() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(scan(&home.path().join("missing")).is_err());
    }

    #[test]
    fn entry_cap_truncates_and_reports_it() {
        let home = tempfile::tempdir().expect("tempdir");
        for name in ["a", "b", "c"] {
            write(
                home.path(),
                &format!(".local/bin/{name}"),
                b"#!/bin/sh\necho x\n",
            );
        }
        let limits = Limits {
            max_entries: 2,
            ..DEFAULT_LIMITS
        };
        let report = scan_with_limits(home.path(), limits).expect("scan");
        assert_eq!(report.entries.len(), 2);
        assert!(report.truncated);
        assert!(report.findings.iter().any(|f| f.kind == "truncated"));
    }

    #[test]
    fn json_rendering_carries_the_protocol_and_totals() {
        let home = tempfile::tempdir().expect("tempdir");
        write(home.path(), ".local/bin/tidy", b"#!/bin/sh\necho done\n");
        let report = scan(home.path()).expect("scan");
        let json = render_json(&report);
        assert_eq!(json["protocol"], PROTOCOL);
        assert_eq!(json["totals"]["portable"], 1);
        assert_eq!(json["entries"][0]["verdict"], "portable");
        assert_eq!(json["entries"][0]["kind"], "local_bin");
    }
}
