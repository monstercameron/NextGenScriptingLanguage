//! Program files: `run program file "PATH"` calls and program-location inputs.
//!
//! The parser never reads files, so this loader resolves both before any step
//! runs. A called file runs in-process with its explicit inputs and its own
//! bindings. Called files cannot declare triggers, local programs, or environment
//! and argument sources.
use crate::ast::{InputDeclaration, InputSource, Program, ReusableProgram, Step, StepKind, Value};
use crate::parser::{self, RawProgram, PROGRAM_FILE_PREFIX};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_FILE_CALL_DEPTH: usize = 16;
const MAX_CALLED_FILES: usize = 256;

/// Parse a program file from disk, resolving its file calls and location inputs.
pub fn parse_file(path: &Path) -> Result<Program> {
    let source = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_source(path, &source)
}

/// Parse program text read from `path`, so relative file calls and location inputs
/// resolve from that program's own directory.
pub fn parse_source(path: &Path, source: &str) -> Result<Program> {
    let canonical =
        fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let mut loader = Loader {
        called: BTreeMap::new(),
        stack: Vec::new(),
    };
    let mut raw = parser::parse_raw(source)?;
    loader.stack.push(canonical.clone());
    loader.resolve(&mut raw, &canonical)?;
    loader.stack.pop();
    // File keys start with `file:`, which no program name can be, so the maps merge.
    raw.definitions.extend(loader.called);
    parser::lower(raw)
}

/// Parse text whose path is known when there is one; without a path, file calls
/// are an error.
pub fn parse_at(source: &str, path: Option<&Path>) -> Result<Program> {
    match path {
        Some(path) => parse_source(path, source),
        None => parser::parse(source),
    }
}

struct Loader {
    /// Called files, keyed by `file:` plus their canonical path.
    called: BTreeMap<String, ReusableProgram>,
    /// Canonical paths of the files being resolved, outermost first.
    stack: Vec<PathBuf>,
}

impl Loader {
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
        let canonical = fs::canonicalize(path)
            .with_context(|| format!("line {line}: program file {} not found", path.display()))?;
        let shown = display_path(&canonical);
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
        let source = fs::read_to_string(&canonical)
            .with_context(|| format!("line {line}: reading program file {shown}"))?;
        let mut raw =
            parser::parse_raw(&source).with_context(|| format!("program file {shown}"))?;
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
