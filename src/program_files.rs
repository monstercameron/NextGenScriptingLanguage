//! Program files: `run program file "PATH"` calls and program-location inputs.
//!
//! The parser never reads files, so this loader resolves both before any step
//! runs. A called file runs in-process with its explicit inputs and its own
//! bindings. Called files cannot declare triggers, local programs, or environment
//! and argument sources.
//!
//! Reading a called file is a host read: it is authorized through the capability
//! policy exactly as other path reads are. Called files are capped at
//! `MAX_PROGRAM_FILE_BYTES` and must be regular files. Load errors name the file
//! and line and never quote its text.
use crate::ast::{
    InputDeclaration, InputSource, Invocation, LoopKind, Program, ReusableProgram, Step, StepKind,
    Value,
};
use crate::manifest::Registry;
use crate::parser::{self, RawProgram, PROGRAM_FILE_PREFIX};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_FILE_CALL_DEPTH: usize = 16;
const MAX_CALLED_FILES: usize = 256;
/// The largest program file a `run program file` call may read.
///
/// A path that uses a Windows spelling: a verbatim (`\\?\`, `\\.\`) or NT
/// (`\??\`) prefix, or a drive-letter prefix (`C:\`, `C:/`). Detection is pure
/// byte logic so identity behaves the same on every host; genuine Unix paths
/// never match (absolute Unix paths start with `/`).
fn is_windows_spelling(bytes: &[u8]) -> bool {
    bytes.starts_with(br"\\?\")
        || bytes.starts_with(br"\\.\")
        || bytes.starts_with(br"\??\")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/'))
}

/// Fold a Windows-spelled path for filesystem use: turn `/` into `\` without
/// touching anything else. Win32 converts separators only for non-verbatim
/// paths; a `\\?\`-prefixed path reaches CreateFileW unchanged, and a `/`
/// there fails with ERROR_INVALID_NAME. That is exactly what joining a called
/// program-file literal such as `sub/child.program` onto an already-verbatim
/// canonical parent produces, so the join must be folded before
/// `std::fs::canonicalize`. Non-Windows spellings pass through untouched.
pub(crate) fn filesystem_spelling(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_encoded_bytes();
    if !is_windows_spelling(bytes) {
        return path.to_path_buf();
    }
    let folded: Vec<u8> = bytes
        .iter()
        .map(|&unit| if unit == b'/' { b'\\' } else { unit })
        .collect();
    // SAFETY: only ASCII `/` became ASCII `\`; byte validity under the
    // platform encoding is unchanged, so the borrowed `OsStr` is valid.
    PathBuf::from(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&folded) })
}

/// Spelling-insensitive identity key for called-file tracking. Strips the
/// verbatim/NT prefix (mapping `\\?\UNC\` to plain UNC form), folds `/` to
/// `\`, and uppercases a drive letter, so `\\?\C:\dir\file`, `C:\dir/file`,
/// and `c:\dir\file` compare as one identity. Only Windows spellings fold;
/// every other path keeps its exact bytes. Keys alone fold: stored canonical
/// values and every emitted display path keep the exact canonical spelling,
/// so diagnostic file, line, span, and message content never changes.
pub(crate) fn identity_key(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_encoded_bytes();
    if !is_windows_spelling(bytes) {
        return path.to_path_buf();
    }
    let mut body: &[u8] = bytes;
    let mut unc = false;
    if let Some(rest) = body.strip_prefix(br"\\?\UNC\") {
        body = rest;
        unc = true;
    } else if let Some(rest) = body
        .strip_prefix(br"\\?\")
        .or_else(|| body.strip_prefix(br"\\.\"))
        .or_else(|| body.strip_prefix(br"\??\"))
    {
        body = rest;
    }
    let mut folded: Vec<u8> = Vec::with_capacity(body.len() + 2);
    if unc {
        folded.extend_from_slice(br"\\");
    }
    folded.extend(
        body.iter()
            .map(|&unit| if unit == b'/' { b'\\' } else { unit }),
    );
    if folded.len() > 2 && folded[1] == b':' && folded[0].is_ascii_lowercase() {
        folded[0] -= b'a' - b'A';
    }
    // SAFETY: only ASCII `/` became ASCII `\` and an ASCII lowercase drive
    // letter became uppercase; byte validity under the platform encoding is
    // unchanged, so the borrowed `OsStr` is valid.
    PathBuf::from(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&folded) })
}

/// The largest program file a `run program file` call may read.
pub const MAX_PROGRAM_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FailedProgramFile {
    pub(crate) path: PathBuf,
    pub(crate) source: String,
    pub(crate) call_chain: Vec<(PathBuf, usize)>,
}

#[derive(Clone, Default)]
pub(crate) struct SourceLoadSnapshot {
    sources: std::sync::Arc<BTreeMap<PathBuf, String>>,
    requests: std::sync::Arc<BTreeMap<PathBuf, PathBuf>>,
    call_paths: std::sync::Arc<BTreeMap<(PathBuf, usize), PathBuf>>,
    root_request: Option<PathBuf>,
    root_path: Option<PathBuf>,
    failed_file: Option<PathBuf>,
    failed_call_chain: Vec<(PathBuf, usize)>,
    replay_only: bool,
}

impl SourceLoadSnapshot {
    pub(crate) fn replay(&self) -> Self {
        let mut snapshot = self.clone();
        snapshot.replay_only = true;
        snapshot
    }

    /// Return captured source by canonical path or an exact requested path.
    /// Both spellings compare by [`identity_key`], so a verbatim canonical
    /// path and a display-alias request resolve to the same captured source.
    pub(crate) fn loaded_source(&self, path: &Path) -> Option<String> {
        let key = identity_key(path);
        self.sources.get(&key).cloned().or_else(|| {
            self.requests
                .get(&key)
                .and_then(|canonical| self.sources.get(&identity_key(canonical)))
                .cloned()
        })
    }

    /// Whether a captured source exists for `path` under any spelling.
    fn has_source(&self, canonical: &Path) -> bool {
        self.sources.contains_key(&identity_key(canonical))
    }

    /// Record a captured source under its spelling-insensitive identity. The
    /// stored value keeps the exact canonical path; only the key folds.
    fn note_source(&mut self, canonical: &Path, source: String) {
        std::sync::Arc::make_mut(&mut self.sources).insert(identity_key(canonical), source);
    }

    pub(crate) fn canonical_path_for_call(&self, parent: &Path, line: usize) -> Option<PathBuf> {
        self.call_paths.get(&(identity_key(parent), line)).cloned()
    }

    pub(crate) fn canonical_root_path(&self, requested: &Path) -> Option<PathBuf> {
        let requested = identity_key(requested);
        if self
            .root_request
            .as_deref()
            .is_some_and(|root| identity_key(root) == requested)
            || self
                .root_path
                .as_deref()
                .is_some_and(|root| identity_key(root) == requested)
        {
            self.root_path.clone()
        } else {
            None
        }
    }

    pub(crate) fn failed_file(&self) -> Option<FailedProgramFile> {
        let path = self.failed_file.as_ref()?;
        Some(FailedProgramFile {
            path: path.clone(),
            source: self.sources.get(&identity_key(path))?.clone(),
            call_chain: self.failed_call_chain.clone(),
        })
    }

    fn remember_request(&mut self, requested: &Path, canonical: &Path) {
        let requests = std::sync::Arc::make_mut(&mut self.requests);
        requests.insert(identity_key(requested), identity_key(canonical));
    }

    fn remember_call(&mut self, parent: &Path, line: usize, canonical: &Path) {
        std::sync::Arc::make_mut(&mut self.call_paths)
            .insert((identity_key(parent), line), canonical.to_path_buf());
    }
}

pub(crate) fn file_key_display_path(key: &str) -> Option<&str> {
    key.strip_prefix(PROGRAM_FILE_PREFIX)
}

fn remember_called_path(
    paths: &mut BTreeMap<String, PathBuf>,
    key: &str,
    canonical: &Path,
) -> bool {
    // Compare by spelling-insensitive identity: the same file reached through
    // a verbatim path and a display-alias request must not read as a
    // collision, while two genuinely different files still collide loudly.
    match paths.entry(key.to_owned()) {
        std::collections::btree_map::Entry::Occupied(entry) => {
            identity_key(entry.get()) != identity_key(canonical)
        }
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(canonical.to_path_buf());
            false
        }
    }
}

#[cfg(feature = "lsp")]
fn canonicalize_called_path(
    path: &Path,
    scope: Option<&SourceScope<'_>>,
) -> std::io::Result<PathBuf> {
    // Fold Windows separators before resolving: a verbatim parent joined with
    // a forward-slash literal would otherwise fail with ERROR_INVALID_NAME.
    match fs::canonicalize(filesystem_spelling(path)) {
        Ok(canonical) => Ok(canonical),
        Err(error) => {
            // A dirty editor buffer remains a valid source after its backing file is
            // removed. Recover only a captured open-document path or symlink alias
            // whose parent still resolves; workspace containment is checked afterwards.
            let Some(scope) = scope else {
                return Err(error);
            };
            let Some(name) = path.file_name() else {
                return Err(error);
            };
            let Some(parent) = path
                .parent()
                .and_then(|parent| fs::canonicalize(filesystem_spelling(parent)).ok())
            else {
                return Err(error);
            };
            let candidate = parent.join(name);
            if scope.open_documents.contains_key(&candidate) {
                Ok(candidate)
            } else if let Some(canonical) = scope
                .open_document_aliases
                .and_then(|aliases| aliases.get(&candidate))
                .filter(|canonical| scope.open_documents.contains_key(*canonical))
            {
                Ok(canonical.clone())
            } else {
                Err(error)
            }
        }
    }
}

/// What a load may consult: the registry, which carries the capability policy, and
/// the working directory that relative policy grants resolve against.
#[derive(Clone, Copy)]
pub struct Access<'a> {
    pub registry: &'a Registry,
    pub cwd: &'a Path,
}

/// Parse a program file from disk, resolving its file calls and location inputs.
pub fn parse_file(path: &Path, access: Option<Access<'_>>) -> Result<Program> {
    let source = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_source(path, &source, access)
}

/// Parse program text read from `path`, so relative file calls and location inputs
/// resolve from that program's own directory. Without an `access`, any file call is
/// refused, because its read could not be authorized.
pub fn parse_source(path: &Path, source: &str, access: Option<Access<'_>>) -> Result<Program> {
    let canonical =
        fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    parse_source_resolved(&canonical, source, access)
}

/// Resolve a program source after its canonical path has already been computed.
/// The caller may retain that path for checkpoints or other startup work.
pub fn parse_source_resolved(
    canonical: &Path,
    source: &str,
    access: Option<Access<'_>>,
) -> Result<Program> {
    parse_source_resolved_inner(
        canonical,
        source,
        access,
        #[cfg(feature = "lsp")]
        None,
        None,
    )
}

#[cfg(feature = "lsp")]
pub(crate) struct SourceScope<'a> {
    pub root: &'a Path,
    pub open_documents: &'a BTreeMap<PathBuf, &'a str>,
    open_document_aliases: Option<&'a BTreeMap<PathBuf, PathBuf>>,
    snapshot: std::rc::Rc<std::cell::RefCell<SourceLoadSnapshot>>,
    capture_sources: bool,
    replay_only: bool,
}

#[cfg(feature = "lsp")]
impl<'a> SourceScope<'a> {
    pub(crate) fn new(root: &'a Path, open_documents: &'a BTreeMap<PathBuf, &'a str>) -> Self {
        Self {
            root,
            open_documents,
            open_document_aliases: None,
            snapshot: std::rc::Rc::new(std::cell::RefCell::new(SourceLoadSnapshot::default())),
            capture_sources: false,
            replay_only: false,
        }
    }

    pub(crate) fn with_open_document_aliases(
        mut self,
        aliases: &'a BTreeMap<PathBuf, PathBuf>,
    ) -> Self {
        self.open_document_aliases = Some(aliases);
        self
    }

    /// Enable source capture for diagnostics that need callee provenance. Kept opt-in
    /// so ordinary LSP checks do not retain a second copy of every called file.
    #[allow(dead_code)]
    pub(crate) fn with_source_snapshots(mut self) -> Self {
        self.capture_sources = true;
        self
    }

    /// Exact source bytes authorized and used by the loader, including editor overlays.
    /// Consumed by callee-provenance diagnostics (#474) rather than rereading files.
    #[allow(dead_code)]
    pub(crate) fn loaded_source(&self, path: &Path) -> Option<String> {
        self.snapshot.borrow().loaded_source(path)
    }

    pub(crate) fn canonical_path_for_call(&self, parent: &Path, line: usize) -> Option<PathBuf> {
        self.snapshot.borrow().canonical_path_for_call(parent, line)
    }

    pub(crate) fn failed_file(&self) -> Option<FailedProgramFile> {
        self.snapshot.borrow().failed_file()
    }

    pub(crate) fn replay(&self) -> Self {
        Self {
            root: self.root,
            open_documents: self.open_documents,
            open_document_aliases: self.open_document_aliases,
            snapshot: self.snapshot.clone(),
            capture_sources: false,
            replay_only: true,
        }
    }
}

fn parse_source_resolved_inner<'a>(
    canonical: &Path,
    source: &str,
    access: Option<Access<'a>>,
    #[cfg(feature = "lsp")] scope: Option<&'a SourceScope<'a>>,
    snapshot: Option<&'a std::cell::RefCell<SourceLoadSnapshot>>,
) -> Result<Program> {
    #[cfg(feature = "lsp")]
    if scope.is_some_and(|scope| !canonical.starts_with(scope.root)) {
        bail!("program file is outside the language server workspace");
    }
    let mut loader = Loader {
        access,
        called: BTreeMap::new(),
        called_paths: BTreeMap::new(),
        stack: Vec::new(),
        call_chain: Vec::new(),
        #[cfg(feature = "lsp")]
        scope,
        snapshot,
    };
    let mut raw = parser::parse_raw(source)?;
    loader.stack.push(canonical.to_path_buf());
    loader.resolve(&mut raw, canonical)?;
    loader.stack.pop();
    // File keys start with `file:`, which no program name can be, so the maps merge.
    raw.definitions.extend(loader.called);
    parser::lower(raw)
}

/// Parse text whose path is known when there is one; without a path, file calls
/// are an error.
pub fn parse_at(source: &str, path: Option<&Path>, access: Option<Access<'_>>) -> Result<Program> {
    match path {
        Some(path) => parse_source(path, source, access),
        None => parser::parse(source),
    }
}

/// Parse a source while retaining the exact authorized call graph for static
/// diagnostic passes. Supplying a replay snapshot forbids filesystem resolution
/// and reads for called files.
pub(crate) fn parse_at_with_snapshot(
    source: &str,
    path: Option<&Path>,
    access: Option<Access<'_>>,
    replay: Option<&SourceLoadSnapshot>,
) -> (Result<Program>, SourceLoadSnapshot) {
    let mut captured = replay.cloned().unwrap_or_default();
    captured.failed_file = None;
    captured.failed_call_chain.clear();
    let snapshot = std::cell::RefCell::new(captured);
    let parsed = match path {
        Some(path) if snapshot.borrow().replay_only => {
            // Reuse the root identity captured before any aliases can change,
            // while still avoiding filesystem reads of callees.
            let canonical = snapshot
                .borrow()
                .canonical_root_path(path)
                .unwrap_or_else(|| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
            parse_source_resolved_inner(
                &canonical,
                source,
                access,
                #[cfg(feature = "lsp")]
                None,
                Some(&snapshot),
            )
        }
        Some(path) => {
            match fs::canonicalize(path).with_context(|| format!("resolving {}", path.display())) {
                Ok(canonical) => {
                    {
                        let mut captured = snapshot.borrow_mut();
                        captured.root_request = Some(path.to_path_buf());
                        captured.root_path = Some(canonical.clone());
                    }
                    parse_source_resolved_inner(
                        &canonical,
                        source,
                        access,
                        #[cfg(feature = "lsp")]
                        None,
                        Some(&snapshot),
                    )
                }
                Err(error) => Err(error),
            }
        }
        None => parser::parse(source),
    };
    (parsed, snapshot.into_inner())
}

#[cfg(feature = "lsp")]
pub(crate) fn parse_at_scoped<'a>(
    source: &str,
    path: Option<&Path>,
    access: Option<Access<'a>>,
    scope: Option<&'a SourceScope<'a>>,
) -> Result<Program> {
    if let Some(scope) = scope {
        let mut snapshot = scope.snapshot.borrow_mut();
        snapshot.failed_file = None;
        snapshot.failed_call_chain.clear();
    }
    match path {
        Some(path) => {
            #[cfg(feature = "lsp")]
            if scope.is_some_and(|scope| scope.replay_only) {
                return parse_source_resolved_inner(
                    path,
                    source,
                    access,
                    scope,
                    scope.map(|scope| &*scope.snapshot),
                );
            }
            let canonical =
                fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
            let snapshot = scope
                .filter(|scope| scope.capture_sources || scope.replay_only)
                .map(|scope| &*scope.snapshot);
            parse_source_resolved_inner(&canonical, source, access, scope, snapshot)
        }
        None => parser::parse(source),
    }
}

struct Loader<'a> {
    access: Option<Access<'a>>,
    #[cfg(feature = "lsp")]
    scope: Option<&'a SourceScope<'a>>,
    snapshot: Option<&'a std::cell::RefCell<SourceLoadSnapshot>>,
    /// Called files, keyed by their public `file:` display key.
    called: BTreeMap<String, ReusableProgram>,
    /// Canonical file identity for each public display key.
    called_paths: BTreeMap<String, PathBuf>,
    /// Canonical paths of the files being resolved, outermost first.
    stack: Vec<PathBuf>,
    /// Callsite edges from the root program to the current file.
    call_chain: Vec<(PathBuf, usize)>,
}

impl Loader<'_> {
    fn resolve(&mut self, raw: &mut RawProgram, file: &Path) -> Result<()> {
        let directory = file
            .parent()
            .context("program file has no directory")?
            .to_path_buf();
        set_location_defaults(&mut raw.inputs, file)?;
        self.resolve_steps(&mut raw.steps, &directory)?;
        for definition in raw.definitions.values_mut() {
            self.resolve_steps(&mut definition.steps, &directory)?;
        }
        Ok(())
    }

    fn resolve_steps(&mut self, steps: &mut [Step], directory: &Path) -> Result<()> {
        for step in steps {
            let StepKind::CallProgram { call, .. } = &mut step.kind else {
                continue;
            };
            let Some(literal) = call.operation.strip_prefix(PROGRAM_FILE_PREFIX) else {
                continue;
            };
            let literal = literal.to_owned();
            let key = self.load(&directory.join(&literal), step.line)?;
            call.operation = key;
        }
        Ok(())
    }

    /// Load one called file, returning its definition key.
    fn load(&mut self, path: &Path, line: usize) -> Result<String> {
        let access = self.access.context(
            "run program file needs the program's capability policy to authorize its read; this entry point cannot load it",
        )?;
        let (replay_only, replayed) = self
            .snapshot
            .map(|snapshot| {
                let snapshot = snapshot.borrow();
                let replayed = snapshot
                    .requests
                    .get(&identity_key(path))
                    .cloned()
                    .and_then(|canonical| {
                        snapshot
                            .sources
                            .get(&identity_key(&canonical))
                            .cloned()
                            .map(|source| (canonical, source))
                    });
                (snapshot.replay_only, replayed)
            })
            .unwrap_or((false, None));
        #[cfg(feature = "lsp")]
        let replay_only = replay_only || self.scope.is_some_and(|scope| scope.replay_only);
        if replay_only && replayed.is_none() {
            bail!(
                "line {line}: no captured source snapshot exists for called program file {}",
                path.display()
            );
        }
        let canonical = match &replayed {
            Some((canonical, _)) => canonical.clone(),
            None => {
                // Fold Windows separators first: `path` joins a program-file
                // literal onto an already-verbatim canonical parent, and a
                // verbatim path with `/` fails canonicalization on Windows.
                #[cfg(feature = "lsp")]
                let resolved = canonicalize_called_path(&filesystem_spelling(path), self.scope);
                #[cfg(not(feature = "lsp"))]
                let resolved = fs::canonicalize(filesystem_spelling(path));
                resolved.with_context(|| {
                    format!("line {line}: program file {} not found", path.display())
                })?
            }
        };
        let shown = display_path(&canonical);
        #[cfg(feature = "lsp")]
        if self
            .scope
            .is_some_and(|scope| !canonical.starts_with(scope.root))
        {
            bail!("line {line}: called program file is outside the language server workspace");
        }
        let key = format!("{PROGRAM_FILE_PREFIX}{shown}");
        if remember_called_path(&mut self.called_paths, &key, &canonical) {
            bail!(
                "line {line}: called program files have the same display path but different canonical identities"
            );
        }
        let parent = self.stack.last().cloned();
        if let (Some(snapshot), Some(parent)) = (self.snapshot, parent.as_deref()) {
            let mut snapshot = snapshot.borrow_mut();
            #[cfg(feature = "lsp")]
            let replaying = replay_only;
            #[cfg(not(feature = "lsp"))]
            let replaying = snapshot.replay_only;
            if !replaying {
                snapshot.remember_call(parent, line, &canonical);
            }
        }
        if self.called.contains_key(&key) {
            if let Some(snapshot) = self.snapshot {
                let mut snapshot = snapshot.borrow_mut();
                #[cfg(feature = "lsp")]
                let replaying = replay_only;
                #[cfg(not(feature = "lsp"))]
                let replaying = snapshot.replay_only;
                if !replaying && snapshot.has_source(&canonical) {
                    snapshot.remember_request(path, &canonical);
                }
            }
            return Ok(key);
        }
        // Compare open files by spelling-insensitive identity so a cycle
        // through a display alias still reads as the same file.
        if let Some(start) = self
            .stack
            .iter()
            .position(|open| identity_key(open) == identity_key(&canonical))
        {
            let chain: Vec<String> = self.stack[start..]
                .iter()
                .chain(std::iter::once(&canonical))
                .map(|open| display_path(open))
                .collect();
            bail!(
                "line {line}: program file calls form a cycle: {}",
                chain.join(" -> ")
            );
        }
        if self.stack.len() >= MAX_FILE_CALL_DEPTH {
            bail!("line {line}: program file calls exceed {MAX_FILE_CALL_DEPTH} levels");
        }
        if self.called.len() >= MAX_CALLED_FILES {
            bail!("line {line}: a program may call at most {MAX_CALLED_FILES} program files");
        }
        let overlay: Option<&&str> = {
            #[cfg(feature = "lsp")]
            {
                self.scope
                    .and_then(|scope| scope.open_documents.get(&canonical))
            }
            #[cfg(not(feature = "lsp"))]
            {
                None
            }
        };
        if let Some(edited) = overlay {
            if edited.len() as u64 > MAX_PROGRAM_FILE_BYTES {
                bail!("line {line}: program file {shown} is larger than 1 MiB");
            }
        }
        if replayed.is_none() {
            authorize_read(access, &canonical, &shown, line)?;
        }
        let disk_source = if replayed.is_some() || overlay.is_some() {
            None
        } else {
            Some(read_program_file(&canonical, &shown, line)?)
        };
        let source = replayed
            .as_ref()
            .map(|(_, source)| source.as_str())
            .or(overlay.copied())
            .or(disk_source.as_deref())
            .expect("a source is available from replay, overlay, or disk");
        if let Some(snapshot) = self.snapshot {
            let mut snapshot = snapshot.borrow_mut();
            #[cfg(feature = "lsp")]
            let replaying = replay_only;
            #[cfg(not(feature = "lsp"))]
            let replaying = snapshot.replay_only;
            if !replaying {
                snapshot.note_source(&canonical, source.to_owned());
                snapshot.remember_request(path, &canonical);
            }
        }
        let mut raw = parser::parse_raw(source).map_err(|error| {
            if let Some(snapshot) = self.snapshot {
                let mut snapshot = snapshot.borrow_mut();
                snapshot.failed_file = Some(canonical.clone());
                snapshot.failed_call_chain = self.call_chain.clone();
                if let Some(parent) = self.stack.last() {
                    snapshot.failed_call_chain.push((parent.clone(), line));
                }
            }
            anyhow::anyhow!(
                "line {}: called program file {shown} is not valid program text",
                error_line(&error).unwrap_or(1)
            )
        })?;
        if raw.trigger.is_some() {
            bail!("line {line}: called program file {shown} cannot declare a trigger");
        }
        if !raw.definitions.is_empty() {
            bail!(
                "line {line}: called program file {shown} cannot define programs; call that file by path"
            );
        }
        if let Some(input) = raw.inputs.iter().find(|input| {
            matches!(
                input.source,
                Some(
                    InputSource::Environment(_) | InputSource::Argument(_) | InputSource::Arguments
                )
            )
        }) {
            bail!(
                "line {line}: input {:?} of called program file {shown} must be passed explicitly; read environment or arguments in the caller",
                input.name
            );
        }
        // Applet names come from the file's text, so they are checked here and reported
        // by position only. A later diagnostic would otherwise quote them.
        for step in &raw.steps {
            if calls_of(step)
                .into_iter()
                .any(|call| undeclared(access.registry, call))
            {
                bail!(
                    "line {}: called program file {shown} calls an applet that is not declared",
                    step.line
                );
            }
        }
        self.stack.push(canonical.clone());
        let pushed_callsite = if let Some(parent) = parent {
            self.call_chain.push((parent, line));
            true
        } else {
            false
        };
        let resolved = self.resolve(&mut raw, &canonical);
        if pushed_callsite {
            self.call_chain.pop();
        }
        self.stack.pop();
        resolved?;
        self.called.insert(
            key.clone(),
            ReusableProgram {
                inputs: raw.inputs,
                steps: raw.steps,
                line: 1,
            },
        );
        Ok(key)
    }
}

/// The read of a called file is a host read, authorized exactly as other path reads
/// are, through the capability policy.
#[cfg(not(target_arch = "wasm32"))]
fn authorize_read(access: Access<'_>, canonical: &Path, shown: &str, line: usize) -> Result<()> {
    crate::capabilities::authorize_host_path(
        access.registry,
        canonical,
        crate::capabilities::HostAccess::Read,
        access.cwd,
    )
    .with_context(|| {
        format!("line {line}: reading program file {shown} is not allowed by the capability policy")
    })
}

/// A build without a host filesystem cannot read a called file at all.
#[cfg(target_arch = "wasm32")]
fn authorize_read(_access: Access<'_>, _canonical: &Path, shown: &str, line: usize) -> Result<()> {
    bail!("line {line}: program file {shown} cannot be read: this build has no host filesystem")
}

/// Read a called file. Only regular files are read, and the read is capped, so a
/// device, a pipe, or a huge file cannot hang or exhaust the process. The errors name
/// the file and never contain its text.
fn read_program_file(canonical: &Path, shown: &str, line: usize) -> Result<String> {
    let metadata = fs::metadata(canonical)
        .with_context(|| format!("line {line}: program file {shown} cannot be read"))?;
    if !metadata.is_file() {
        bail!("line {line}: program file {shown} is not a regular file");
    }
    if metadata.len() > MAX_PROGRAM_FILE_BYTES {
        bail!("line {line}: program file {shown} is larger than 1 MiB");
    }
    let file = File::open(canonical)
        .with_context(|| format!("line {line}: program file {shown} cannot be read"))?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        bail!("line {line}: program file {shown} is not a regular file");
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROGRAM_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("line {line}: program file {shown} cannot be read"))?;
    if bytes.len() as u64 > MAX_PROGRAM_FILE_BYTES {
        bail!("line {line}: program file {shown} is larger than 1 MiB");
    }
    String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("line {line}: program file {shown} is not UTF-8 text"))
}

/// The line a parse error names, from its leading `line N:`, if it has one.
fn error_line(error: &anyhow::Error) -> Option<usize> {
    let text = format!("{error:#}");
    let rest = text.strip_prefix("line ")?;
    rest.split(':').next()?.trim().parse().ok()
}

/// The invocations a statement runs as commands.
fn calls_of(step: &Step) -> Vec<&Invocation> {
    match &step.kind {
        StepKind::Invoke { call, .. }
        | StepKind::IfCommand { call, .. }
        | StepKind::Loop {
            kind: LoopKind::WhileCommand { call, .. },
            ..
        } => vec![call],
        StepKind::TaskGroup { tasks, .. } => tasks.iter().collect(),
        StepKind::SpawnJob { stages, .. } => stages.iter().map(|stage| &stage.call).collect(),
        _ => Vec::new(),
    }
}

/// A catalog command that the registry does not declare. Direct executables name
/// their own binary and are not looked up.
fn undeclared(registry: &Registry, call: &Invocation) -> bool {
    !call.direct && registry.resolve_call(call).is_err()
}

/// Program-location inputs take the canonical location of their own program file
/// as their default. An explicit `--input` or `with` value still wins.
fn set_location_defaults(inputs: &mut [InputDeclaration], file: &Path) -> Result<()> {
    for input in inputs {
        let location = match input.source {
            Some(InputSource::ProgramDirectory) => {
                display_path(file.parent().unwrap_or_else(|| Path::new("")))
            }
            Some(InputSource::ProgramPath) => display_path(file),
            _ => continue,
        };
        let value = Value::String(location);
        input.validate_value(&value)?;
        input.default = Some(value);
    }
    Ok(())
}

/// A path as text, without the Windows verbatim prefix that canonicalization adds.
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    let stripped = text.strip_prefix(r"\\?\").map(str::to_owned);
    stripped.unwrap_or(text)
}

/// The canonical directory of a program file, for showing its calls relative to it.
pub fn program_directory(path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    canonical
        .parent()
        .map(|parent| PathBuf::from(display_path(parent)))
}

/// Shows a `file:` definition key as its path relative to `base`, the calling
/// program's directory. Other keys, and keys without a base, are shown unchanged.
pub fn display_key(key: &str, base: Option<&Path>) -> String {
    let (Some(target), Some(base)) = (file_key_display_path(key), base) else {
        return key.to_owned();
    };
    let target = Path::new(target);
    let base_parts: Vec<_> = base.components().collect();
    let target_parts: Vec<_> = target.components().collect();
    let common = base_parts
        .iter()
        .zip(&target_parts)
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return key.to_owned();
    }
    let mut parts: Vec<String> = Vec::new();
    parts.extend(std::iter::repeat_n(
        "..".to_owned(),
        base_parts.len() - common,
    ));
    parts.extend(
        target_parts[common..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()),
    );
    format!("{PROGRAM_FILE_PREFIX}{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_key_unifies_windows_spellings() {
        // The same file under every Windows spelling folds to one identity.
        // Pure byte logic, so this runs identically on every host.
        let spellings = [
            PathBuf::from(r"\\?\C:\workspace\callee.program"),
            PathBuf::from(r"C:\workspace\callee.program"),
            PathBuf::from(r"C:\workspace/callee.program"),
            PathBuf::from(r"c:\workspace\callee.program"),
            PathBuf::from(r"c:\workspace/callee.program"),
        ];
        let first = identity_key(&spellings[0]);
        for spelling in &spellings[1..] {
            assert_eq!(identity_key(spelling), first, "{spelling:?}");
        }
        // A verbatim UNC path unifies with its plain UNC spelling.
        assert_eq!(
            identity_key(&PathBuf::from(r"\\?\UNC\server\share\callee.program")),
            identity_key(&PathBuf::from(r"\\server\share\callee.program"))
        );
        // Different files keep different identities, including Windows
        // spellings that differ only past the prefix.
        assert_ne!(
            identity_key(&PathBuf::from(r"\\?\C:\workspace\callee.program")),
            identity_key(&PathBuf::from(r"C:\workspace\other.program"))
        );
        assert_ne!(
            identity_key(&PathBuf::from(r"\\?\C:\workspace\callee.program.")),
            identity_key(&PathBuf::from(r"\\?\C:\workspace\callee.program "))
        );
        // Non-Windows spellings keep their exact bytes: absolute Unix paths,
        // plain relatives, and Unix names that merely contain a backslash.
        for plain in [
            PathBuf::from("/tmp/workspace/callee.program"),
            PathBuf::from("sub/child.program"),
            PathBuf::from("main.program"),
        ] {
            assert_eq!(identity_key(&plain), plain, "{plain:?}");
        }
        let backslash_name = PathBuf::from("dir\\file");
        assert_eq!(identity_key(&backslash_name), backslash_name);
    }

    #[test]
    fn filesystem_spelling_folds_only_windows_separators() {
        // A verbatim parent joined with a forward-slash literal is exactly
        // what the loader canonicalizes; the fold keeps the prefix so the
        // path still resolves, including past MAX_PATH.
        assert_eq!(
            filesystem_spelling(&PathBuf::from(r"\\?\C:\workspace\sub/child.program")),
            PathBuf::from(r"\\?\C:\workspace\sub\child.program")
        );
        assert_eq!(
            filesystem_spelling(&PathBuf::from(r"C:\workspace\sub/child.program")),
            PathBuf::from(r"C:\workspace\sub\child.program")
        );
        // Unix paths pass through untouched.
        for plain in [
            PathBuf::from("/tmp/workspace/sub/child.program"),
            PathBuf::from("sub/child.program"),
        ] {
            assert_eq!(filesystem_spelling(&plain), plain, "{plain:?}");
        }
    }

    #[test]
    fn source_snapshot_unifies_verbatim_identity_with_display_alias() {
        let requested = PathBuf::from(r"\\?\C:\workspace\sub\..\callee.program");
        let canonical = PathBuf::from(r"\\?\C:\workspace\callee.program");
        let shown = display_path(&canonical);
        let key = format!("{PROGRAM_FILE_PREFIX}{shown}");
        let mut snapshot = SourceLoadSnapshot::default();
        snapshot.note_source(&canonical, "run\n".into());
        snapshot.remember_request(&requested, &canonical);
        let caller = PathBuf::from(r"\\?\C:\workspace\main.program");
        snapshot.remember_call(&caller, 3, &canonical);

        assert_eq!(snapshot.loaded_source(&canonical).as_deref(), Some("run\n"));
        assert_eq!(snapshot.loaded_source(&requested).as_deref(), Some("run\n"));
        // A display-alias request (verbatim prefix stripped, as written in a
        // `run program file` literal) resolves to the same captured source.
        assert_eq!(
            snapshot.loaded_source(&PathBuf::from(&shown)).as_deref(),
            Some("run\n")
        );
        // The stored callsite keeps the exact canonical path for spans.
        assert_eq!(
            snapshot.canonical_path_for_call(&caller, 3),
            Some(canonical.clone())
        );
        assert_eq!(file_key_display_path(&key), Some(shown.as_str()));
        assert_eq!(
            display_key(&key, None),
            format!("{PROGRAM_FILE_PREFIX}{shown}")
        );
    }

    #[test]
    fn snapshot_maps_each_canonical_path_by_callsite() {
        let first = PathBuf::from(r"\\?\C:\workspace\callee.program.");
        let second = PathBuf::from(r"\\?\C:\workspace\callee.program ");
        let caller = PathBuf::from(r"\\?\C:\workspace\main.program");
        let first_display_key = format!("{PROGRAM_FILE_PREFIX}C:\\workspace\\callee.program");
        let second_display_key = format!("{PROGRAM_FILE_PREFIX}C:\\workspace\\callee.program");
        assert_eq!(first_display_key, second_display_key);

        let mut snapshot = SourceLoadSnapshot::default();
        // Store through the same spelling-insensitive keys the loader uses.
        snapshot.note_source(&first, "first\n".into());
        snapshot.note_source(&second, "second\n".into());
        snapshot.remember_call(&caller, 1, &first);
        snapshot.remember_call(&caller, 2, &second);

        assert_eq!(
            snapshot.canonical_path_for_call(&caller, 1),
            Some(first.clone())
        );
        assert_eq!(
            snapshot.canonical_path_for_call(&caller, 2),
            Some(second.clone())
        );
        snapshot.failed_file = Some(second.clone());
        snapshot.failed_call_chain = vec![(caller, 2)];
        assert_eq!(
            snapshot.failed_file(),
            Some(FailedProgramFile {
                path: second,
                source: "second\n".into(),
                call_chain: vec![(PathBuf::from(r"\\?\C:\workspace\main.program"), 2)],
            })
        );
    }

    #[test]
    fn a_display_key_collision_with_distinct_canonical_files_is_rejected() {
        let first = PathBuf::from(r"\\?\C:\workspace\callee.program.");
        let second = PathBuf::from(r"\\?\C:\workspace\callee.program ");
        let mut paths = BTreeMap::new();

        assert!(!remember_called_path(
            &mut paths,
            "file:C:\\workspace\\callee.program",
            &first
        ));
        assert!(remember_called_path(
            &mut paths,
            "file:C:\\workspace\\callee.program",
            &second
        ));
    }

    #[test]
    fn public_file_definition_keys_remain_plain_serializable_paths() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main.program");
        let callee = root.path().join("callee.program");
        std::fs::write(&main, "run program file \"callee.program\"\n").unwrap();
        std::fs::write(&callee, "use value \"ok\"\n").unwrap();
        let registry = Registry::default();
        let access = Access {
            registry: &registry,
            cwd: root.path(),
        };
        let program = parse_at(
            "run program file \"callee.program\"\n",
            Some(&main),
            Some(access),
        )
        .unwrap();
        let expected = format!(
            "{PROGRAM_FILE_PREFIX}{}",
            display_path(&fs::canonicalize(&callee).unwrap())
        );
        let call = match &program.steps[0].kind {
            StepKind::CallProgram { call, .. } => call,
            other => panic!("expected a file call, got {other:?}"),
        };
        assert_eq!(call.operation, expected);
        assert!(program.definitions.contains_key(&expected));
        let serialized = serde_json::to_value(&program).unwrap();
        assert!(serialized["definitions"]
            .as_object()
            .unwrap()
            .contains_key(&expected));
        assert!(!serialized.to_string().contains("\\u0000"));
        fn contains_string(value: &serde_json::Value, expected: &str) -> bool {
            match value {
                serde_json::Value::String(value) => value == expected,
                serde_json::Value::Array(values) => {
                    values.iter().any(|value| contains_string(value, expected))
                }
                serde_json::Value::Object(values) => values
                    .values()
                    .any(|value| contains_string(value, expected)),
                _ => false,
            }
        }
        assert!(contains_string(&serialized, &expected));
    }
}
