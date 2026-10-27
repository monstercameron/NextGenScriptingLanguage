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
pub const MAX_PROGRAM_FILE_BYTES: u64 = 1024 * 1024;

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
    )
}

#[cfg(feature = "lsp")]
pub(crate) struct SourceScope<'a> {
    pub root: &'a Path,
    pub open_documents: &'a BTreeMap<PathBuf, &'a str>,
    loaded_sources: std::cell::RefCell<BTreeMap<PathBuf, String>>,
    capture_sources: bool,
}

#[cfg(feature = "lsp")]
impl<'a> SourceScope<'a> {
    pub(crate) fn new(root: &'a Path, open_documents: &'a BTreeMap<PathBuf, &'a str>) -> Self {
        Self {
            root,
            open_documents,
            loaded_sources: std::cell::RefCell::new(BTreeMap::new()),
            capture_sources: false,
        }
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
    pub(crate) fn loaded_source(&self, canonical: &Path) -> Option<String> {
        self.loaded_sources.borrow().get(canonical).cloned()
    }
}

fn parse_source_resolved_inner<'a>(
    canonical: &Path,
    source: &str,
    access: Option<Access<'a>>,
    #[cfg(feature = "lsp")] scope: Option<&'a SourceScope<'a>>,
) -> Result<Program> {
    #[cfg(feature = "lsp")]
    if scope.is_some_and(|scope| !canonical.starts_with(scope.root)) {
        bail!("program file is outside the language server workspace");
    }
    let mut loader = Loader {
        access,
        called: BTreeMap::new(),
        stack: Vec::new(),
        #[cfg(feature = "lsp")]
        scope,
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

#[cfg(feature = "lsp")]
pub(crate) fn parse_at_scoped<'a>(
    source: &str,
    path: Option<&Path>,
    access: Option<Access<'a>>,
    scope: Option<&'a SourceScope<'a>>,
) -> Result<Program> {
    match path {
        Some(path) => {
            let canonical =
                fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
            parse_source_resolved_inner(&canonical, source, access, scope)
        }
        None => parser::parse(source),
    }
}

struct Loader<'a> {
    access: Option<Access<'a>>,
    #[cfg(feature = "lsp")]
    scope: Option<&'a SourceScope<'a>>,
    /// Called files, keyed by `file:` plus their canonical path.
    called: BTreeMap<String, ReusableProgram>,
    /// Canonical paths of the files being resolved, outermost first.
    stack: Vec<PathBuf>,
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
        let canonical = fs::canonicalize(path)
            .with_context(|| format!("line {line}: program file {} not found", path.display()))?;
        let shown = display_path(&canonical);
        #[cfg(feature = "lsp")]
        if self
            .scope
            .is_some_and(|scope| !canonical.starts_with(scope.root))
        {
            bail!("line {line}: called program file is outside the language server workspace");
        }
        let key = format!("{PROGRAM_FILE_PREFIX}{shown}");
        if self.called.contains_key(&key) {
            return Ok(key);
        }
        if let Some(start) = self.stack.iter().position(|open| open == &canonical) {
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
        authorize_read(access, &canonical, &shown, line)?;
        let disk_source = read_program_file(&canonical, &shown, line)?;
        #[cfg(not(feature = "lsp"))]
        let source = disk_source.as_str();
        #[cfg(feature = "lsp")]
        let source = if let Some(edited) = self
            .scope
            .and_then(|scope| scope.open_documents.get(&canonical))
        {
            if edited.len() as u64 > MAX_PROGRAM_FILE_BYTES {
                bail!("line {line}: program file {shown} is larger than 1 MiB");
            }
            *edited
        } else {
            disk_source.as_str()
        };
        #[cfg(feature = "lsp")]
        if let Some(scope) = self.scope.filter(|scope| scope.capture_sources) {
            scope
                .loaded_sources
                .borrow_mut()
                .insert(canonical.clone(), source.to_owned());
        }
        let mut raw = parser::parse_raw(source).map_err(|error| {
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
        let resolved = self.resolve(&mut raw, &canonical);
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
    let (Some(target), Some(base)) = (key.strip_prefix(PROGRAM_FILE_PREFIX), base) else {
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
