//! A small stdio Language Server Protocol implementation for `.program` files.
//!
//! The server implements incremental document sync, diagnostics, completion,
//! hover, reusable-program definition/references, and validated rename edits.
//! It uses JSON-RPC framing from the LSP specification and writes no logs to
//! stdout, keeping the channel safe for editor clients.

use crate::diagnostics::{analyze_source, analyze_source_scoped, Diagnostic, DiagnosticSeverity};
use crate::manifest::Registry;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOTAL_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_OPEN_DOCUMENTS: usize = 256;
const MAX_CONTENT_CHANGES: usize = 128;
const MAX_RENAME_EDITS: usize = 10_000;

#[derive(Debug, Clone)]
struct Document {
    text: String,
    version: Option<i64>,
    /// File identity captured at didOpen, used only if the live path disappears.
    canonical_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    line: usize,
    character: usize,
}

#[derive(Debug, Clone, Copy)]
struct Word {
    text_start: usize,
    text_end: usize,
    line: usize,
    quoted: bool,
}

/// Start the LSP server on stdin/stdout using the default manifest directory.
pub fn run_stdio() -> Result<()> {
    let registry = load_default_registry()?;
    run_with_io(std::io::stdin().lock(), std::io::stdout().lock(), registry)
}

fn load_default_registry() -> Result<Registry> {
    if let Some(path) = env::var_os("EXECUTE_MANIFEST_DIR") {
        return Registry::load_optional(Path::new(&path)).with_context(|| {
            format!(
                "could not load manifests from {}",
                Path::new(&path).display()
            )
        });
    }
    // The working directory is never searched, matching the CLI: a workspace
    // could otherwise shadow bundled applets. EXECUTE_MANIFEST_DIR selects
    // other manifests explicitly.
    if let Ok(executable) = env::current_exe() {
        let executable = fs::canonicalize(&executable).unwrap_or(executable);
        if let Some(parent) = executable.parent() {
            for candidate in [
                parent.join("../manifests"),
                parent.join("../share/execute/manifests"),
            ] {
                if candidate.is_dir() {
                    return Registry::load_optional(&candidate).with_context(|| {
                        format!("could not load manifests from {}", candidate.display())
                    });
                }
            }
        }
    }
    Ok(Registry::default())
}

/// Run the stdio protocol over supplied streams. This is public so embedders
/// and protocol tests can use the same concrete server loop as `execute lsp`.
pub fn run_with_io<R: Read, W: Write>(reader: R, writer: W, registry: Registry) -> Result<()> {
    let mut reader = BufReader::new(reader);
    let mut writer = writer;
    let mut documents = BTreeMap::<String, Document>::new();
    let mut diagnostic_targets = BTreeMap::<String, BTreeSet<String>>::new();
    let mut workspace = Workspace::default();
    let mut watch_dynamic_registration = false;
    while let Some(message) = read_message(&mut reader)? {
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                write_error(&mut writer, &JsonValue::Null, -32700, &error)?;
                continue;
            }
        };
        let Some(method) = message.get("method").and_then(JsonValue::as_str) else {
            continue;
        };
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(JsonValue::Null);
        let result = match method {
            "initialize" => {
                workspace = Workspace::from_initialize(&params);
                watch_dynamic_registration = params
                    .pointer("/capabilities/workspace/didChangeWatchedFiles/dynamicRegistration")
                    .and_then(JsonValue::as_bool)
                    == Some(true);
                Some(json!({
                    "capabilities": {
                        "textDocumentSync": { "openClose": true, "change": 2, "save": { "includeText": false } },
                        "positionEncoding": "utf-16",
                        "completionProvider": { "triggerCharacters": [" ", "."] },
                        "hoverProvider": true,
                        "definitionProvider": true,
                        "referencesProvider": true,
                        "renameProvider": { "prepareProvider": true },
                        "codeActionProvider": { "codeActionKinds": ["quickfix"] },
                    "workspace": { "workspaceFolders": { "supported": true, "changeNotifications": true } }
                    },
                    "serverInfo": { "name": "execute-language-server", "version": env!("CARGO_PKG_VERSION") }
                }))
            }
            "initialized" => {
                if watch_dynamic_registration {
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0", "id": "execute-watch-program-files",
                            "method": "client/registerCapability",
                            "params": { "registrations": [{ "id": "execute-watch-program-files", "method": "workspace/didChangeWatchedFiles", "registerOptions": { "watchers": [{ "globPattern": "**/*", "kind": 7 }] } }] }
                        }),
                    )?;
                }
                None
            }
            "$/cancelRequest" => None,
            "shutdown" => Some(JsonValue::Null),
            "exit" => break,
            "textDocument/didOpen" => {
                if let Some(document) = params.get("textDocument") {
                    if let (Some(uri), Some(text)) = (
                        document.get("uri").and_then(JsonValue::as_str),
                        document.get("text").and_then(JsonValue::as_str),
                    ) {
                        let Some(version) = document.get("version").and_then(JsonValue::as_i64)
                        else {
                            show_message(
                                &mut writer,
                                "Ignored document without an integer version",
                            )?;
                            continue;
                        };
                        if text.len() > MAX_DOCUMENT_BYTES {
                            show_message(
                                &mut writer,
                                &format!("Document exceeds the {MAX_DOCUMENT_BYTES}-byte limit"),
                            )?;
                            continue;
                        }
                        let other_bytes = document_bytes_except(&documents, uri);
                        if other_bytes.saturating_add(text.len()) > MAX_TOTAL_DOCUMENT_BYTES {
                            show_message(
                                &mut writer,
                                &format!(
                                    "Open documents exceed the {MAX_TOTAL_DOCUMENT_BYTES}-byte aggregate limit"
                                ),
                            )?;
                            continue;
                        }
                        if !documents.contains_key(uri) && documents.len() >= MAX_OPEN_DOCUMENTS {
                            show_message(
                                &mut writer,
                                &format!(
                                    "The language server supports at most {MAX_OPEN_DOCUMENTS} open documents"
                                ),
                            )?;
                            continue;
                        }
                        documents.insert(
                            uri.to_owned(),
                            Document {
                                text: text.to_owned(),
                                version: Some(version),
                                canonical_path: local_file_path(uri)
                                    .and_then(|path| fs::canonicalize(path).ok()),
                            },
                        );
                        publish_affected_diagnostics(
                            &mut writer,
                            &documents,
                            &workspace,
                            &registry,
                            &mut diagnostic_targets,
                            Some(uri),
                        )?;
                    }
                }
                None
            }
            "textDocument/didChange" => {
                if let Some(document_identifier) = params.get("textDocument") {
                    if let Some(uri) = document_identifier.get("uri").and_then(JsonValue::as_str) {
                        let other_bytes = document_bytes_except(&documents, uri);
                        if let Some(document) = documents.get_mut(uri) {
                            let mut candidate = document.clone();
                            match apply_content_changes(&mut candidate, &params) {
                                Ok(()) => {
                                    if other_bytes.saturating_add(candidate.text.len())
                                        > MAX_TOTAL_DOCUMENT_BYTES
                                    {
                                        show_message(
                                            &mut writer,
                                            &format!(
                                                "Open documents exceed the {MAX_TOTAL_DOCUMENT_BYTES}-byte aggregate limit"
                                            ),
                                        )?;
                                        continue;
                                    }
                                    *document = candidate;
                                    publish_affected_diagnostics(
                                        &mut writer,
                                        &documents,
                                        &workspace,
                                        &registry,
                                        &mut diagnostic_targets,
                                        Some(uri),
                                    )?;
                                }
                                Err(error) => {
                                    show_message(
                                        &mut writer,
                                        &format!("Ignored invalid document update: {error}"),
                                    )?;
                                }
                            }
                        }
                    }
                }
                None
            }
            "textDocument/didClose" => {
                if let Some(uri) = params
                    .pointer("/textDocument/uri")
                    .and_then(JsonValue::as_str)
                {
                    documents.remove(uri);
                    write_notification(
                        &mut writer,
                        "textDocument/publishDiagnostics",
                        json!({ "uri": uri, "diagnostics": [] }),
                    )?;
                    publish_affected_diagnostics(
                        &mut writer,
                        &documents,
                        &workspace,
                        &registry,
                        &mut diagnostic_targets,
                        Some(uri),
                    )?;
                }
                None
            }
            "workspace/didChangeWorkspaceFolders" => {
                workspace.apply_folder_changes(&params);
                publish_affected_diagnostics(
                    &mut writer,
                    &documents,
                    &workspace,
                    &registry,
                    &mut diagnostic_targets,
                    None,
                )?;
                None
            }
            "textDocument/didSave" | "workspace/didChangeWatchedFiles" => {
                let changed = params
                    .pointer("/textDocument/uri")
                    .and_then(JsonValue::as_str);
                publish_affected_diagnostics(
                    &mut writer,
                    &documents,
                    &workspace,
                    &registry,
                    &mut diagnostic_targets,
                    changed,
                )?;
                None
            }
            "textDocument/completion" => {
                let manifests = registry.manifests()?;
                Some(completion(&params, &documents, &registry, &manifests))
            }
            "textDocument/hover" => {
                let manifests = registry.manifests()?;
                Some(hover(&params, &documents, &manifests))
            }
            "textDocument/definition" => Some(definition(&params, &documents)),
            "textDocument/references" => Some(references(&params, &documents)),
            "textDocument/prepareRename" => Some(prepare_rename(&params, &documents)),
            "textDocument/rename" => Some(rename(&params, &documents)),
            "textDocument/codeAction" => Some(code_actions(&params, &documents, &registry)),
            _ if id.is_some() => {
                write_error(
                    &mut writer,
                    id.as_ref().unwrap(),
                    -32601,
                    "method not found",
                )?;
                None
            }
            _ => None,
        };
        if let (Some(id), Some(result)) = (id, result) {
            write_response(&mut writer, id, result)?;
        }
    }
    Ok(())
}

fn read_message<R: BufRead>(
    reader: &mut R,
) -> Result<Option<std::result::Result<JsonValue, String>>> {
    let mut content_length = None;
    let mut header_bytes = 0usize;
    loop {
        let Some(line) = read_header_line(reader, MAX_HEADER_BYTES - header_bytes)? else {
            if header_bytes == 0 {
                return Ok(None);
            }
            bail!("unexpected EOF in LSP headers");
        };
        header_bytes += line.len();
        let line = std::str::from_utf8(&line).context("LSP header is not ASCII/UTF-8")?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end_matches(['\r', '\n']).split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                if content_length.is_some() {
                    bail!("LSP message has duplicate Content-Length headers");
                }
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .context("invalid LSP Content-Length")?,
                );
            }
        }
    }
    let Some(length) = content_length else {
        return Ok(Some(Err("LSP message is missing Content-Length".into())));
    };
    if length > MAX_MESSAGE_BYTES {
        bail!("LSP message exceeds {MAX_MESSAGE_BYTES} bytes");
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .context("reading LSP message body")?;
    Ok(Some(serde_json::from_slice(&body).map_err(|error| {
        format!("decoding LSP JSON-RPC message: {error}")
    })))
}

/// Read a header line without allowing `read_line` to grow an unbounded
/// allocation when a peer sends a header that never terminates.
fn read_header_line<R: BufRead>(reader: &mut R, max_bytes: usize) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            bail!("unexpected EOF in LSP header line");
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(buffer.len(), |position| position + 1);
        if line.len().saturating_add(count) > max_bytes {
            bail!("LSP headers exceed {MAX_HEADER_BYTES} bytes");
        }
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

fn write_message<W: Write>(writer: &mut W, message: &JsonValue) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn write_response<W: Write>(writer: &mut W, id: JsonValue, result: JsonValue) -> Result<()> {
    write_message(
        writer,
        &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
}

fn write_error<W: Write>(writer: &mut W, id: &JsonValue, code: i64, message: &str) -> Result<()> {
    write_message(
        writer,
        &json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message }
        }),
    )
}

fn write_notification<W: Write>(writer: &mut W, method: &str, params: JsonValue) -> Result<()> {
    write_message(
        writer,
        &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    )
}

fn show_message<W: Write>(writer: &mut W, message: &str) -> Result<()> {
    write_notification(
        writer,
        "window/showMessage",
        json!({ "type": 2, "message": message }),
    )
}

fn document_bytes_except(documents: &BTreeMap<String, Document>, excluded_uri: &str) -> usize {
    documents
        .iter()
        .filter(|(uri, _)| uri.as_str() != excluded_uri)
        .map(|(_, document)| document.text.len())
        .sum()
}

#[derive(Default)]
struct Workspace {
    roots: Vec<PathBuf>,
    explicitly_configured: bool,
}

fn local_file_path(uri: &str) -> Option<PathBuf> {
    let uri = url::Url::parse(uri).ok()?;
    if uri.scheme() != "file"
        || uri.query().is_some()
        || uri.fragment().is_some()
        || uri
            .host_str()
            .is_some_and(|host| !host.eq_ignore_ascii_case("localhost"))
    {
        return None;
    }
    uri.to_file_path().ok()
}

impl Workspace {
    fn from_initialize(params: &JsonValue) -> Self {
        let mut workspace = Self::default();
        if let Some(folders) = params.get("workspaceFolders").and_then(JsonValue::as_array) {
            workspace.explicitly_configured = !folders.is_empty();
            for folder in folders {
                if let Some(root) = folder
                    .get("uri")
                    .and_then(JsonValue::as_str)
                    .and_then(local_file_path)
                    .and_then(|path| fs::canonicalize(path).ok())
                    .filter(|path| path.is_dir())
                {
                    workspace.roots.push(root);
                }
            }
        }
        if !workspace.explicitly_configured {
            if let Some(uri) = params.get("rootUri").and_then(JsonValue::as_str) {
                workspace.explicitly_configured = true;
                if let Some(root) = local_file_path(uri)
                    .and_then(|path| fs::canonicalize(path).ok())
                    .filter(|path| path.is_dir())
                {
                    workspace.roots.push(root);
                }
            }
        }
        if !workspace.explicitly_configured {
            if let Some(path) = params.get("rootPath").and_then(JsonValue::as_str) {
                workspace.explicitly_configured = true;
                let path = Path::new(path);
                if path.is_absolute() {
                    if let Ok(root) = fs::canonicalize(path) {
                        if root.is_dir() {
                            workspace.roots.push(root);
                        }
                    }
                }
            }
        }
        workspace
    }

    fn apply_folder_changes(&mut self, params: &JsonValue) {
        self.explicitly_configured = true;
        let Some(event) = params.get("event") else {
            self.roots.clear();
            return;
        };
        let Some(removed) = event.get("removed").and_then(JsonValue::as_array) else {
            self.roots.clear();
            return;
        };
        for folder in removed {
            let root = folder
                .get("uri")
                .and_then(JsonValue::as_str)
                .and_then(local_file_path)
                .and_then(|path| fs::canonicalize(path).ok());
            match root {
                Some(root) => self.roots.retain(|existing| existing != &root),
                // If a removed path no longer resolves, do not retain any stale grant.
                None => self.roots.clear(),
            }
        }
        let Some(added) = event.get("added").and_then(JsonValue::as_array) else {
            self.roots.clear();
            return;
        };
        for folder in added {
            if let Some(root) = folder
                .get("uri")
                .and_then(JsonValue::as_str)
                .and_then(local_file_path)
                .and_then(|path| fs::canonicalize(path).ok())
                .filter(|path| path.is_dir())
            {
                if !self.roots.contains(&root) {
                    self.roots.push(root);
                }
            }
        }
    }

    fn root_for(&self, path: &Path) -> Option<PathBuf> {
        if self.explicitly_configured {
            self.roots
                .iter()
                .filter(|root| path.starts_with(root))
                .max_by_key(|root| root.components().count())
                .cloned()
        } else {
            path.parent().map(Path::to_path_buf)
        }
    }
}

fn open_document_overlays(
    documents: &BTreeMap<String, Document>,
) -> (BTreeMap<PathBuf, &str>, BTreeMap<PathBuf, PathBuf>) {
    let mut sources = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    for (uri, document) in documents {
        let Some(requested_path) = local_file_path(uri) else {
            continue;
        };
        let canonical = fs::canonicalize(&requested_path)
            .ok()
            .or_else(|| document.canonical_path.clone());
        let Some(canonical) = canonical else {
            continue;
        };
        sources.insert(canonical.clone(), document.text.as_str());
        if requested_path != canonical {
            aliases.insert(requested_path, canonical);
        }
    }
    (sources, aliases)
}

#[cfg(test)]
fn diagnostics_for_document(
    uri: &str,
    documents: &BTreeMap<String, Document>,
    workspace: &Workspace,
    registry: &Registry,
) -> Vec<Diagnostic> {
    let (open_documents, aliases) = open_document_overlays(documents);
    diagnostics_with_overlays(
        uri,
        documents,
        workspace,
        registry,
        &open_documents,
        &aliases,
    )
}

fn diagnostics_with_overlays<'a>(
    uri: &str,
    documents: &BTreeMap<String, Document>,
    workspace: &Workspace,
    registry: &'a Registry,
    open_documents: &'a BTreeMap<PathBuf, &'a str>,
    aliases: &'a BTreeMap<PathBuf, PathBuf>,
) -> Vec<Diagnostic> {
    let document = &documents[uri];
    let path = local_file_path(uri)
        .and_then(|path| fs::canonicalize(path).ok())
        .or_else(|| document.canonical_path.clone());
    let root = path.as_deref().and_then(|path| workspace.root_for(path));
    let scope = root.as_ref().map(|root| {
        crate::program_files::SourceScope::new(root, open_documents)
            .with_open_document_aliases(aliases)
            .with_source_snapshots()
    });
    let usable_path = if scope.is_some() {
        path.as_deref()
    } else {
        None
    };
    let mut diagnostics =
        analyze_source_scoped(&document.text, usable_path, registry, scope.as_ref());
    if usable_path.is_none() {
        for diagnostic in &mut diagnostics {
            if diagnostic
                .message
                .contains("needs the program loaded from its file")
            {
                diagnostic.severity = DiagnosticSeverity::Information;
                diagnostic.message = if path.is_some() {
                    "Called program files cannot be resolved outside the language server workspace"
                        .to_owned()
                } else {
                    "Called program files need a saved local file location to resolve".to_owned()
                };
            }
        }
    }
    diagnostics
}

fn publish_affected_diagnostics<W: Write>(
    writer: &mut W,
    documents: &BTreeMap<String, Document>,
    workspace: &Workspace,
    registry: &Registry,
    diagnostic_targets: &mut BTreeMap<String, BTreeSet<String>>,
    changed_uri: Option<&str>,
) -> Result<()> {
    let (open_documents, aliases) = open_document_overlays(documents);
    let mut outgoing = BTreeMap::<String, Vec<Diagnostic>>::new();
    let mut sources = BTreeSet::<String>::new();
    for (uri, document) in documents {
        // Any file call, including a transitive one, contains these literal words.
        // This is a conservative filter; comments can cause harmless extra checks.
        if changed_uri != Some(uri.as_str())
            && !(document.text.contains("program") && document.text.contains("file"))
        {
            continue;
        }
        sources.insert(uri.clone());
    }
    if let Some(changed_uri) = changed_uri {
        if let Some(previous_targets) = diagnostic_targets.get(changed_uri) {
            sources.extend(
                previous_targets
                    .iter()
                    .filter(|uri| documents.contains_key(*uri))
                    .cloned(),
            );
        }
        if !documents.contains_key(changed_uri) {
            diagnostic_targets.remove(changed_uri);
        }
    }
    let prior_targets = sources
        .iter()
        .map(|uri| {
            (
                uri.clone(),
                diagnostic_targets.get(uri).cloned().unwrap_or_default(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for uri in sources {
        let Some(_document) = documents.get(&uri) else {
            continue;
        };
        outgoing.entry(uri.clone()).or_default();
        let mut current_targets = BTreeSet::new();
        for mut diagnostic in diagnostics_with_overlays(
            &uri,
            documents,
            workspace,
            registry,
            &open_documents,
            &aliases,
        ) {
            let diagnostic_path = diagnostic.span.path.as_deref().map(PathBuf::from);
            let destination = diagnostic_path
                .as_deref()
                .and_then(|path| uri_for_source_path(path, documents))
                .cloned();
            let destination = if let Some(destination) = destination {
                destination
            } else {
                if diagnostic_path.is_some() {
                    if let Some(callsite) = diagnostic.related.iter().find(|related| {
                        related
                            .span
                            .path
                            .as_deref()
                            .and_then(|path| uri_for_source_path(Path::new(path), documents))
                            .is_some_and(|source_uri| source_uri.as_str() == uri)
                    }) {
                        diagnostic.span = callsite.span.clone();
                    }
                }
                uri.clone()
            };
            if destination != uri {
                current_targets.insert(destination.clone());
            }
            outgoing.entry(destination).or_default().push(diagnostic);
        }
        diagnostic_targets.insert(uri.clone(), current_targets);
    }
    for targets in prior_targets.values() {
        for uri in targets {
            if documents.contains_key(uri) {
                outgoing.entry(uri.clone()).or_default();
            }
        }
    }
    for (uri, diagnostics) in outgoing {
        let Some(document) = documents.get(&uri) else {
            continue;
        };
        let positions = SourcePositions::new(&document.text);
        let mut merged = Vec::<Diagnostic>::new();
        for diagnostic in diagnostics {
            if let Some(existing) = merged.iter_mut().find(|existing| {
                existing.code == diagnostic.code
                    && existing.message == diagnostic.message
                    && existing.span.path == diagnostic.span.path
                    && existing.span.start_byte == diagnostic.span.start_byte
                    && existing.span.end_byte == diagnostic.span.end_byte
            }) {
                // The same leaf diagnostic can arrive through multiple open
                // callers. Preserve the full call chain, even when a shorter
                // route was encountered first, while keeping repeated callsites
                // out of the LSP payload.
                for related in diagnostic.related {
                    if !existing.related.contains(&related) {
                        existing.related.push(related);
                    }
                }
            } else {
                merged.push(diagnostic);
            }
        }
        let diagnostics = merged
            .into_iter()
            .map(|diagnostic| {
                lsp_diagnostic_with_documents(&document.text, &positions, diagnostic, documents)
            })
            .collect::<Vec<_>>();
        let mut params = json!({ "uri": uri, "diagnostics": diagnostics });
        if let Some(version) = document.version {
            params["version"] = version.into();
        }
        write_notification(writer, "textDocument/publishDiagnostics", params)?;
    }
    Ok(())
}

fn uri_for_source_path<'a>(
    path: &Path,
    documents: &'a BTreeMap<String, Document>,
) -> Option<&'a String> {
    documents.keys().find(|uri| {
        let Some(document_path) = local_file_path(uri) else {
            return false;
        };
        let candidate = fs::canonicalize(&document_path)
            .ok()
            .or_else(|| {
                documents
                    .get(*uri)
                    .and_then(|document| document.canonical_path.clone())
            })
            .unwrap_or(document_path);
        candidate.as_path() == path
    })
}

fn lsp_diagnostic(source: &str, positions: &SourcePositions, diagnostic: Diagnostic) -> JsonValue {
    lsp_diagnostic_with_documents(source, positions, diagnostic, &BTreeMap::new())
}

fn lsp_diagnostic_with_documents(
    source: &str,
    positions: &SourcePositions,
    diagnostic: Diagnostic,
    documents: &BTreeMap<String, Document>,
) -> JsonValue {
    let related = diagnostic
        .related
        .iter()
        .filter_map(|related| {
            let path = related.span.path.as_deref()?;
            let path = Path::new(path);
            let (uri, range) = if let Some(uri) = uri_for_source_path(path, documents) {
                let related_source = &documents.get(uri)?.text;
                let related_positions = SourcePositions::new(related_source);
                (
                    uri.clone(),
                    related_positions.range(
                        related_source,
                        related.span.start_byte,
                        related.span.end_byte,
                    ),
                )
            } else {
                // Closed caller files are never reopened to publish related
                // information. Use the URI and UTF-16 range captured while the
                // authorized source snapshot was already in memory.
                if related.span.path_bytes.as_deref() != Some(path.as_os_str().as_encoded_bytes()) {
                    return None;
                }
                let uri = url::Url::from_file_path(path).ok()?.to_string();
                let range = related.lsp_range?;
                (
                    uri,
                    json!({
                        "start": {
                            "line": range.start_line,
                            "character": range.start_character
                        },
                        "end": {
                            "line": range.end_line,
                            "character": range.end_character
                        }
                    }),
                )
            };
            Some(json!({
                "location": {
                    "uri": uri,
                    "range": range
                },
                "message": related.message
            }))
        })
        .collect::<Vec<_>>();
    let mut result = json!({
        "range": positions.range(source, diagnostic.span.start_byte, diagnostic.span.end_byte),
        "severity": match diagnostic.severity {
            crate::diagnostics::DiagnosticSeverity::Error => 1,
            crate::diagnostics::DiagnosticSeverity::Warning => 2,
            crate::diagnostics::DiagnosticSeverity::Information => 3,
            crate::diagnostics::DiagnosticSeverity::Hint => 4,
        },
        "code": diagnostic.code,
        "source": "execute",
        "message": diagnostic.message,
        "data": { "span_precision": diagnostic.span.precision }
    });
    if !related.is_empty() {
        result["relatedInformation"] = json!(related);
    }
    result
}

fn apply_content_changes(document: &mut Document, params: &JsonValue) -> Result<()> {
    let changes = params
        .get("contentChanges")
        .and_then(JsonValue::as_array)
        .context("didChange requires contentChanges")?;
    if changes.is_empty() {
        bail!("didChange requires at least one content change");
    }
    if changes.len() > MAX_CONTENT_CHANGES {
        bail!("didChange exceeds the {MAX_CONTENT_CHANGES}-change limit");
    }
    let version = params
        .pointer("/textDocument/version")
        .and_then(JsonValue::as_i64)
        .context("didChange requires an integer document version")?;
    if document.version.is_some_and(|current| version <= current) {
        bail!("didChange version {version} is stale or nonincreasing");
    }
    // Stage every incremental edit. A malformed later change must not leave
    // the document half-updated after an earlier valid change.
    let mut candidate = document.text.clone();
    for change in changes {
        let text = change
            .get("text")
            .and_then(JsonValue::as_str)
            .context("content change requires text")?;
        let Some(range) = change.get("range") else {
            candidate.clear();
            candidate.push_str(text);
            if candidate.len() > MAX_DOCUMENT_BYTES {
                bail!("document exceeds the {MAX_DOCUMENT_BYTES}-byte limit");
            }
            continue;
        };
        let start = read_position(range.get("start"))?;
        let end = read_position(range.get("end"))?;
        let start_byte = position_to_byte(&candidate, start)
            .ok_or_else(|| anyhow!("didChange start position is outside the document"))?;
        let end_byte = position_to_byte(&candidate, end)
            .ok_or_else(|| anyhow!("didChange end position is outside the document"))?;
        if start_byte > end_byte {
            bail!("didChange range start is after range end");
        }
        candidate.replace_range(start_byte..end_byte, text);
        if candidate.len() > MAX_DOCUMENT_BYTES {
            bail!("document exceeds the {MAX_DOCUMENT_BYTES}-byte limit");
        }
    }
    document.text = candidate;
    document.version = Some(version);
    Ok(())
}

fn read_position(value: Option<&JsonValue>) -> Result<Position> {
    let value = value.context("LSP range position is missing")?;
    let line = value
        .get("line")
        .and_then(JsonValue::as_u64)
        .context("position line is missing")?;
    let character = value
        .get("character")
        .and_then(JsonValue::as_u64)
        .context("position character is missing")?;
    Ok(Position {
        line: usize::try_from(line).context("position line is out of range")?,
        character: usize::try_from(character).context("position character is out of range")?,
    })
}

/// Quick fixes from the static diagnostics whose span touches the requested range.
/// Each suggested fix becomes one action that edits only the document's text.
fn code_actions(
    params: &JsonValue,
    documents: &BTreeMap<String, Document>,
    registry: &Registry,
) -> JsonValue {
    let Some(uri) = params
        .pointer("/textDocument/uri")
        .and_then(JsonValue::as_str)
    else {
        return json!([]);
    };
    let Some(document) = documents.get(uri) else {
        return json!([]);
    };
    let source = document.text.as_str();
    let positions = SourcePositions::new(source);
    let requested = params.get("range").and_then(|range| {
        let start = position_to_byte(source, read_position(range.get("start")).ok()?)?;
        let end = position_to_byte(source, read_position(range.get("end")).ok()?)?;
        Some((start, end))
    });
    let mut actions = Vec::new();
    // Recompute fixes for this document's own text; a callee fix is therefore
    // translated against the callee document rather than a caller's offsets.
    for diagnostic in analyze_source(source, registry) {
        if diagnostic.fixes.is_empty() {
            continue;
        }
        if let Some((start, end)) = requested {
            if diagnostic.span.end_byte < start || diagnostic.span.start_byte > end {
                continue;
            }
        }
        let lsp = lsp_diagnostic(source, &positions, diagnostic.clone());
        for (index, fix) in diagnostic.fixes.iter().enumerate() {
            let edits = fix
                .edits
                .iter()
                .map(|edit| {
                    json!({
                        "range": positions.range(source, edit.span.start_byte, edit.span.end_byte),
                        "newText": edit.replacement,
                    })
                })
                .collect::<Vec<_>>();
            actions.push(json!({
                "title": fix.message,
                "kind": "quickfix",
                "diagnostics": [lsp.clone()],
                "isPreferred": index == 0,
                "edit": { "changes": { uri: edits } },
            }));
        }
    }
    JsonValue::Array(actions)
}

fn text_document_position(params: &JsonValue) -> Option<(&str, Position)> {
    Some((
        params.pointer("/textDocument/uri")?.as_str()?,
        read_position(params.get("position")).ok()?,
    ))
}

fn completion(
    params: &JsonValue,
    documents: &BTreeMap<String, Document>,
    registry: &Registry,
    manifests: &[&crate::manifest::Manifest],
) -> JsonValue {
    let Some((uri, position)) = text_document_position(params) else {
        return json!({ "isIncomplete": false, "items": [] });
    };
    let Some(document) = documents.get(uri) else {
        return json!({ "isIncomplete": false, "items": [] });
    };
    let Some(offset) = position_to_byte(&document.text, position) else {
        return json!({ "isIncomplete": false, "items": [] });
    };
    let (line_start, _) = line_bounds(&document.text, position.line).unwrap_or((0, 0));
    let prefix = document.text.get(line_start..offset).unwrap_or("");
    // A cursor inside a trailing comment completes nothing.
    if crate::parser::strip_comment(prefix).len() != prefix.len() {
        return json!({ "isIncomplete": false, "items": [] });
    }
    let trimmed = prefix.trim_start();
    // Word tokens of the line prefix; quoted spans stay intact so values with
    // spaces compare as one token.
    let tokens = completion_words(prefix);
    let ends_with_space = prefix.chars().last().is_some_and(char::is_whitespace);

    let mut items = Vec::<JsonValue>::new();
    let indent = prefix.len() - trimmed.len();
    complete_at_cursor(
        &mut items,
        trimmed,
        &tokens,
        ends_with_space,
        &document.text,
        position.line,
        indent,
        registry,
        manifests,
    );

    let prefix_word = if ends_with_space {
        ""
    } else {
        completion_word_text(&tokens).unwrap_or("")
    };
    if !prefix_word.is_empty() {
        let needle = prefix_word.to_ascii_lowercase();
        items.retain(|item| {
            item.get("filterText")
                .or_else(|| item.get("label"))
                .and_then(JsonValue::as_str)
                .is_some_and(|label| label.to_ascii_lowercase().contains(&needle))
        });
    }
    json!({ "isIncomplete": false, "items": items })
}

/// One word of a completion line prefix. `quoted` marks a `"..."` span so
/// keyword matching skips string literals.
struct CompletionWord {
    text: String,
    quoted: bool,
}

/// Split a line prefix into words, keeping `"..."` spans (with `\` escapes)
/// and `[`/`]`/`,` punctuation as separate tokens.
fn completion_words(prefix: &str) -> Vec<CompletionWord> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted: Option<char> = None;
    let mut escaped = false;
    let mut in_word = false;
    let flush = |current: &mut String,
                 words: &mut Vec<CompletionWord>,
                 in_word: &mut bool,
                 quoted_word: bool| {
        if *in_word || quoted_word {
            words.push(CompletionWord {
                text: std::mem::take(current),
                quoted: quoted_word,
            });
            *in_word = false;
        }
    };
    for ch in prefix.chars() {
        if let Some(quote) = quoted {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                quoted = None;
                flush(&mut current, &mut words, &mut in_word, true);
            }
            continue;
        }
        match ch {
            '"' => {
                if in_word {
                    flush(&mut current, &mut words, &mut in_word, false);
                }
                quoted = Some('"');
                current.push(ch);
            }
            ' ' | '\t' | '\r' => {
                flush(&mut current, &mut words, &mut in_word, false);
            }
            '[' | ']' | ',' => {
                flush(&mut current, &mut words, &mut in_word, false);
                words.push(CompletionWord {
                    text: ch.to_string(),
                    quoted: false,
                });
            }
            _ => {
                current.push(ch);
                in_word = true;
            }
        }
    }
    if quoted.is_some() {
        // An unterminated string still completes its inner text.
        words.push(CompletionWord {
            text: current,
            quoted: true,
        });
    } else {
        flush(&mut current, &mut words, &mut in_word, false);
    }
    words
}

/// Text of the trailing partial word, without its opening quote when the
/// cursor is inside a string literal.
fn completion_word_text(words: &[CompletionWord]) -> Option<&str> {
    let word = words.last()?;
    if word.quoted {
        Some(word.text.strip_prefix('"').unwrap_or(&word.text))
    } else {
        Some(&word.text)
    }
}

/// Unquoted word texts of a line prefix, for keyword matching.
fn bare_words(tokens: &[CompletionWord]) -> Vec<&str> {
    tokens
        .iter()
        .filter(|word| !word.quoted)
        .map(|word| word.text.as_str())
        .collect()
}

/// Dispatch completion by cursor context: statement start, invocation target,
/// parameter name, parameter value, condition operand/operator, or a binding
/// definition (which completes nothing).
#[allow(clippy::too_many_arguments)]
fn complete_at_cursor(
    items: &mut Vec<JsonValue>,
    trimmed: &str,
    tokens: &[CompletionWord],
    ends_with_space: bool,
    source: &str,
    line: usize,
    indent: usize,
    registry: &Registry,
    manifests: &[&crate::manifest::Manifest],
) {
    let words = bare_words(tokens);
    if words.is_empty() {
        push_statement_forms_scoped(items, source, line, indent);
        return;
    }
    // Declaration options after `accept name as <type> ...`.
    if words[0] == "accept" {
        complete_accept_line(items, &words, ends_with_space);
        return;
    }
    // `take <type> at <pointer> from <value> as <name>`: types, keywords and
    // values in turn, never invocation suffixes.
    if words[0] == "take" {
        complete_take(items, source, line, tokens, &words, ends_with_space);
        return;
    }
    // Conditions: `if`, `otherwise if`, `while`.
    if words[0] == "if" || words[0] == "while" || trimmed.starts_with("otherwise if ") {
        complete_condition(items, source, line, tokens, &words, ends_with_space, manifests);
        return;
    }
    if trimmed.starts_with("otherwise") || words[0] == "else" {
        push_statement_forms_scoped(items, source, line, indent);
        return;
    }
    // Iteration and repetition tails.
    if words[0] == "repeat" && words.len() == 2 && ends_with_space {
        push_keyword(items, "times", "Repeat count unit");
        return;
    }
    if words[0] == "for" && words.get(1).is_some_and(|word| *word == "each") {
        complete_for_each(items, source, line, &words, ends_with_space);
        return;
    }
    // Task groups and background jobs take no invocation suffixes.
    if matches!(words[0], "parallel" | "race") {
        complete_parallel(items, source, line, indent, tokens, &words, ends_with_space);
        return;
    }
    if words[0] == "spawn" {
        complete_spawn(items, source, line, indent, &words, ends_with_space);
        return;
    }
    // Scoped temporary directories and files take no invocation suffixes.
    if words[0] == "with" {
        complete_with_temporary(items, source, line, indent, &words, ends_with_space);
        return;
    }
    // Single-value statements.
    if matches!(words[0], "use") {
        complete_use(items, source, line, tokens, &words, ends_with_space);
        return;
    }
    if matches!(words[0], "read") {
        complete_read(items, source, line, tokens, &words, ends_with_space);
        return;
    }
    if matches!(words[0], "exit" | "fail" | "return") {
        complete_value_statement(items, source, line, tokens, &words, ends_with_space);
        return;
    }
    if matches!(words[0], "await" | "inspect" | "cancel") {
        complete_job_reference(items, source, line, indent, tokens, &words, ends_with_space);
        return;
    }
    // Listener sources name applets without a `run` prefix.
    if words == ["listen", "to"] || (words.len() == 3 && words[0] == "listen" && words[1] == "to") {
        push_applet_names(items, manifests);
        if let Ok(program) = crate::parser::parse(source) {
            push_program_names(items, &program);
        }
        return;
    }
    // `then` transfer forms (`then parse json as`, `then encode ... as json`)
    // are complete statements, not invocations with suffixes.
    if words[0] == "then"
        && matches!(words.get(1), Some(word) if matches!(*word, "parse" | "encode" | "save" | "append" | "capture"))
    {
        complete_then_statement(items, source, line, tokens, &words, ends_with_space);
        return;
    }
    // Invocations and their suffixes.
    if is_invocation_head(&words) {
        complete_invocation(
            items,
            source,
            line,
            indent,
            tokens,
            &words,
            ends_with_space,
            registry,
            manifests,
        );
        return;
    }
    // Bare applet phrases (`count lines with ...`) carry no connection head.
    // Statement heads that merely contain `with`/`and`/`as` (`take ... as`,
    // `spawn as`, ...) are complete statements, not invocations.
    if trimmed.contains(" with ") || trimmed.contains(" and ") || trimmed.contains(" as ") {
        if words.first().is_some_and(|word| is_statement_head_word(word)) {
            push_statement_forms_scoped(items, source, line, indent);
        } else {
            complete_invocation(
                items,
                source,
                line,
                indent,
                tokens,
                &words,
                ends_with_space,
                registry,
                manifests,
            );
        }
        return;
    }
    // Anything else (clause heads, triggers, unknown prefixes) falls back to
    // statement forms so people discover the grammar from anywhere.
    push_statement_forms_scoped(items, source, line, indent);
}

/// Whether the line starts an invocation (`run`/`then`/`pipe to`, including
/// handover and detached-launch variants that wrap one native command).
fn is_invocation_head(words: &[&str]) -> bool {
    matches!(words.first(), Some(first) if matches!(*first, "run" | "then" | "interactively"))
        || (words.len() >= 2 && words[0] == "pipe" && words[1] == "to")
        || (words.len() >= 3 && words[0] == "hand" && words[1] == "over" && words[2] == "to")
        || (words.len() >= 2 && words[0] == "launch" && words[1] == "detached")
}

/// Whether `word` starts a parser statement form. Lines starting here are
/// complete statements, so the bare-phrase `with`/`and`/`as` sniff must not
/// route them to invocation completion (which would offer invocation
/// suffixes that `spawn`, `take`, `parallel`, `with temporary ...` and the
/// `then` transfer forms never accept).
fn is_statement_head_word(word: &str) -> bool {
    crate::parser::statement_completion_forms()
        .iter()
        .any(|form| form.label.split(' ').next() == Some(word))
}

/// Complete `accept` declaration lines: types after `as`, declaration options
/// after a complete type, nothing inside choice lists.
fn complete_accept_line(items: &mut Vec<JsonValue>, words: &[&str], ends_with_space: bool) {
    let Some(as_index) = words.iter().position(|word| *word == "as") else {
        return;
    };
    let after = &words[as_index + 1..];
    // Still typing the type, or the cursor sits right after `as `.
    if after.is_empty() || (after.len() == 1 && !ends_with_space) {
        for form in ["text", "integer", "boolean", "json", "list of text", "list of integer", "list of json"] {
            push_keyword(items, form, "Input value type");
        }
        return;
    }
    if !ends_with_space {
        return;
    }
    // A complete type precedes option keywords.
    push_keyword(items, "one of", "Allowed text values");
    push_keyword(items, "from", "Input source");
    push_keyword(items, "with default", "Default value");
    push_keyword(items, "described as", "Human-readable description");
}

/// Push the `take` value types from the parser's own table.
fn push_take_types(items: &mut Vec<JsonValue>) {
    for form in crate::parser::take_type_forms() {
        push_keyword(items, form, "Take value type");
    }
}

/// Complete `take <type> at <pointer> from <value> as <name>`: value types
/// first, then each keyword, then values in the pointer/source positions, and
/// nothing once the `as` binding starts. `take` takes no invocation
/// suffixes, so a complete `take ... as <name>` completes nothing at all.
fn complete_take(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
) {
    debug_assert!(words.first() == Some(&"take"));
    // Bare (unquoted) word at a token index; quoted literals never match a
    // keyword, so they only ever count as values.
    let bare = |index: usize| -> Option<&str> {
        tokens
            .get(index)
            .filter(|token| !token.quoted)
            .map(|token| token.text.as_str())
    };
    // --- Value type: `text|integer|boolean|json` or `list of text`. ---
    let mut cursor = 1;
    if bare(cursor) == Some("list") {
        if bare(cursor + 1) != Some("of") {
            if !ends_with_space || tokens.len() == cursor + 1 {
                push_keyword(items, "of", "Take list element type");
            }
            return;
        }
        if bare(cursor + 2) != Some("text") {
            if !ends_with_space || tokens.len() == cursor + 2 {
                push_keyword(items, "text", "Take list of text");
            }
            return;
        }
        cursor += 3;
    } else if matches!(
        bare(cursor),
        Some("text" | "integer" | "boolean" | "json")
    ) {
        cursor += 1;
    } else {
        // Missing (`take `) or partial (`take te...`) type.
        push_take_types(items);
        return;
    }
    // --- `at <pointer>` ---
    if bare(cursor) != Some("at") {
        if !ends_with_space || tokens.len() == cursor {
            push_keyword(items, "at", "JSON pointer keyword");
        }
        return;
    }
    cursor += 1;
    if tokens.len() == cursor {
        if ends_with_space {
            push_value_items(items, source, line, None, None);
        } else {
            push_keyword(items, "at", "JSON pointer keyword");
        }
        return;
    }
    if tokens.len() == cursor + 1 && !ends_with_space {
        // A pointer being typed (bare partial or an open quote).
        push_value_items(items, source, line, None, None);
        return;
    }
    // Pointer complete (`... at <pointer> `, quoted or bare): expect `from`.
    cursor += 1;
    if bare(cursor) != Some("from") {
        if !ends_with_space || tokens.len() == cursor {
            push_keyword(items, "from", "JSON source keyword");
        }
        return;
    }
    cursor += 1;
    // --- `from <value>` ---
    if tokens.len() == cursor {
        if ends_with_space {
            push_value_items(items, source, line, None, None);
        } else {
            push_keyword(items, "from", "JSON source keyword");
        }
        return;
    }
    if tokens.len() == cursor + 1 && !ends_with_space {
        push_value_items(items, source, line, None, None);
        return;
    }
    // Source complete (`... from <value> `): expect `as`.
    cursor += 1;
    if bare(cursor) != Some("as") {
        if !ends_with_space || tokens.len() == cursor {
            push_keyword(items, "as", "Bind the JSON value to a name");
        }
        return;
    }
    cursor += 1;
    // Past `as`: a new binding is being named, or the statement is complete.
    // Either way nothing follows: `take` accepts no invocation suffixes.
    if tokens.len() == cursor && !ends_with_space {
        push_keyword(items, "as", "Bind the JSON value to a name");
    }
}

/// Complete a value condition: operators after the left operand, values
/// (with `one of` choices when the left side names such an input) after the
/// operator, and values for a bare left operand.
fn complete_condition(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
    manifests: &[&crate::manifest::Manifest],
) {
    let head_len = if words[0] == "otherwise" { 2 } else { 1 };
    let rest = if words.len() > head_len {
        &words[head_len..]
    } else {
        &[][..]
    };
    // A trailing quoted span is a value being typed (quoted spans never reach
    // `words`), so complete values filtered by its inner text.
    let quoted_value = !ends_with_space && tokens.last().is_some_and(|token| token.quoted);
    // `if run ...` is a command condition: complete the applet name.
    if rest.first() == Some(&"run") {
        if rest.len() <= 2 && !ends_with_space || rest.len() == 1 {
            push_applet_names(items, manifests);
        }
        if rest.len() >= 2 {
            push_keyword(items, "succeeds", "Branch when the command succeeds");
            push_keyword(items, "fails", "Branch when the command fails");
        }
        return;
    }
    let operator_at = rest.iter().position(|word| {
        matches!(*word, "is" | "contains" | "does" | "succeeds" | "fails")
    });
    match operator_at {
        None => {
            if rest.is_empty() || (rest.len() == 1 && !ends_with_space) {
                push_value_items(items, source, line, None, None);
            } else if ends_with_space {
                for operator in crate::parser::condition_operator_forms() {
                    // `succeeds`/`fails` only follow `run`; value conditions
                    // use the comparison operators.
                    if matches!(*operator, "succeeds" | "fails") {
                        continue;
                    }
                    push_keyword(items, operator, "Condition operator");
                }
            }
        }
        Some(index) => {
            let operator = rest[index];
            let after = &rest[index + 1..];
            // A trailing space (or a quoted value being typed) completes the
            // operator: `if mode is ` already expects the right operand.
            let complete = matches!(operator, "succeeds" | "fails")
                || ((operator == "is" || operator == "contains")
                    && (ends_with_space || quoted_value || !after.is_empty()))
                || (operator == "does" && after.first() == Some(&"not"));
            if !complete {
                // `if mode does ` still needs `not` for `does not contain`.
                if operator == "does" && after.is_empty() && ends_with_space {
                    push_keyword(items, "not", "Negate the contains operator");
                }
                return;
            }
            // The left operand may name an input with `one of` choices.
            let left = rest.first().copied();
            push_value_items(items, source, line, left, None);
        }
    }
}

/// Complete `for each <name> in <list>`: `in` after the iteration binding,
/// then values after `in`.
fn complete_for_each(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    words: &[&str],
    ends_with_space: bool,
) {
    let Some(in_at) = words.iter().position(|word| *word == "in") else {
        // No `in` yet: `for each <name> ` expects it; anything shorter is
        // still the new iteration binding, which completes nothing.
        if ends_with_space && words.len() >= 3 {
            push_keyword(items, "in", "List value to iterate");
        } else if !ends_with_space && words.len() == 4 {
            push_keyword(items, "in", "List value to iterate");
        }
        return;
    };
    if words.get(in_at + 1).is_none() && (ends_with_space || words.len() == in_at + 1) {
        push_value_items(items, source, line, None, None);
    }
}

/// Complete `parallel ...` / `race ...` task-group headers: `at most` (and
/// `for each` for `parallel`) first, then the limit, `as`, and nothing once
/// the result binding starts. Task groups take no invocation suffixes.
fn complete_parallel(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    indent: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
) {
    debug_assert!(matches!(words.first(), Some(first) if matches!(*first, "parallel" | "race")));
    if words.len() == 1 {
        if ends_with_space {
            push_keyword(items, "at most", "Bound the task group size");
            if words[0] == "parallel" {
                push_keyword(items, "for each", "One task per list element");
            }
        } else {
            push_statement_forms_scoped(items, source, line, indent);
        }
        return;
    }
    if words[0] == "parallel" && words[1] == "for" {
        complete_parallel_for_each(items, source, line, tokens, words, ends_with_space);
        return;
    }
    complete_task_limit_tail(items, words, ends_with_space, 1);
}

/// Complete `parallel for each <name> in <list> at most <limit> as <result>`.
fn complete_parallel_for_each(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
) {
    if words.get(2) != Some(&"each") {
        // `parallel for ` expects `each`; a partial third word narrows to it.
        if words.len() == 3 && !ends_with_space {
            push_keyword(items, "each", "One task per list element");
        }
        return;
    }
    if words.len() == 3 {
        // `parallel for each` (with or without a trailing space): the
        // iteration binding is new, so nothing completes.
        return;
    }
    if words.len() == 4 && !ends_with_space {
        // Still typing the iteration binding.
        return;
    }
    // The binding is complete (`parallel for each <name> `): expect `in`.
    let mut cursor = 4;
    if words.get(cursor) != Some(&"in") {
        if (ends_with_space && words.len() == cursor)
            || (!ends_with_space && words.len() == cursor + 1)
        {
            push_keyword(items, "in", "List value to iterate");
        }
        return;
    }
    cursor += 1;
    if tokens.len() == cursor {
        if ends_with_space {
            push_value_items(items, source, line, None, None);
        } else {
            push_keyword(items, "in", "List value to iterate");
        }
        return;
    }
    if tokens.len() == cursor + 1 && !ends_with_space {
        push_value_items(items, source, line, None, None);
        return;
    }
    // List complete: the group limit follows as `at most <limit>`.
    complete_task_limit_tail_tokens(items, tokens, ends_with_space, cursor + 1);
}

/// Complete the shared `at most <limit> as <result>` tail of `parallel`,
/// `race` and `parallel for each` headers, starting at the `at` word index
/// in bare-word space.
fn complete_task_limit_tail(
    items: &mut Vec<JsonValue>,
    words: &[&str],
    ends_with_space: bool,
    base: usize,
) {
    if words.get(base) != Some(&"at") {
        if (ends_with_space && words.len() == base)
            || (!ends_with_space && words.len() == base + 1)
        {
            push_keyword(items, "at most", "Bound the task group size");
        }
        return;
    }
    if words.get(base + 1) != Some(&"most") {
        if (ends_with_space && words.len() == base + 1)
            || (!ends_with_space && words.len() == base + 2)
        {
            push_keyword(items, "most", "Bound the task group size");
        }
        return;
    }
    let limit_at = base + 2;
    if words.len() == limit_at && !ends_with_space {
        // `... most` just typed: confirm the keyword.
        push_keyword(items, "most", "Bound the task group size");
        return;
    }
    if words.len() <= limit_at {
        // `... at most ` : an integer literal follows, which completes nothing.
        return;
    }
    if words.len() == limit_at + 1 && !ends_with_space {
        // Still typing the integer limit.
        return;
    }
    // Limit complete (`... at most <limit> `): expect `as`.
    let as_at = limit_at + 1;
    if words.get(as_at) != Some(&"as") {
        if (ends_with_space && words.len() == as_at)
            || (!ends_with_space && words.len() == as_at + 1)
        {
            push_keyword(items, "as", "Bind the task group result");
        }
        return;
    }
    // Past `as`: a new result binding is being named, or the header is
    // complete. Either way nothing follows: no invocation suffixes.
    if words.len() == as_at + 1 && !ends_with_space {
        push_keyword(items, "as", "Bind the task group result");
    }
}

/// Complete the `at most <limit> as <result>` tail in token space (for
/// `parallel for each`, where a quoted list value shifts word indexes).
/// `base` is the token index where `at` is expected.
fn complete_task_limit_tail_tokens(
    items: &mut Vec<JsonValue>,
    tokens: &[CompletionWord],
    ends_with_space: bool,
    base: usize,
) {
    let bare = |index: usize| -> Option<&str> {
        tokens
            .get(index)
            .filter(|token| !token.quoted)
            .map(|token| token.text.as_str())
    };
    if bare(base) != Some("at") {
        if !ends_with_space || tokens.len() == base {
            push_keyword(items, "at most", "Bound the task group size");
        }
        return;
    }
    if bare(base + 1) != Some("most") {
        if !ends_with_space || tokens.len() == base + 1 {
            push_keyword(items, "most", "Bound the task group size");
        }
        return;
    }
    let limit_at = base + 2;
    if tokens.len() <= limit_at {
        if !ends_with_space {
            push_keyword(items, "most", "Bound the task group size");
        }
        return;
    }
    if tokens.len() == limit_at + 1 && !ends_with_space {
        return;
    }
    let as_at = limit_at + 1;
    if bare(as_at) != Some("as") {
        if !ends_with_space || tokens.len() == as_at {
            push_keyword(items, "as", "Bind the task group result");
        }
        return;
    }
    if tokens.len() == as_at + 1 && !ends_with_space {
        push_keyword(items, "as", "Bind the task group result");
    }
}

/// Complete `spawn as <job>`: `as` first, then nothing. The job name is a
/// new binding and the pipeline body follows on later lines, so a complete
/// `spawn as <job>` never takes invocation suffixes.
fn complete_spawn(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    indent: usize,
    words: &[&str],
    ends_with_space: bool,
) {
    debug_assert!(words.first() == Some(&"spawn"));
    if words.len() == 1 {
        if ends_with_space {
            push_keyword(items, "as", "Name the background job");
        } else {
            push_statement_forms_scoped(items, source, line, indent);
        }
        return;
    }
    if words.get(1) != Some(&"as") {
        if words.len() == 2 && !ends_with_space {
            push_keyword(items, "as", "Name the background job");
        }
        return;
    }
    if words.len() == 2 && !ends_with_space {
        push_keyword(items, "as", "Name the background job");
    }
    // Past `as`: a new job name is being named, or the header is complete.
}

/// Complete `with temporary file|directory as <name>`: each keyword in turn,
/// then nothing once the `as` binding starts. Temporary scopes never take
/// invocation suffixes.
fn complete_with_temporary(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    indent: usize,
    words: &[&str],
    ends_with_space: bool,
) {
    debug_assert!(words.first() == Some(&"with"));
    if words.len() == 1 {
        if ends_with_space {
            push_keyword(items, "temporary", "Scoped fresh path");
        } else {
            push_statement_forms_scoped(items, source, line, indent);
        }
        return;
    }
    if words.get(1) != Some(&"temporary") {
        if words.len() == 2 && !ends_with_space {
            push_keyword(items, "temporary", "Scoped fresh path");
        }
        return;
    }
    if words.len() == 2 {
        if ends_with_space {
            push_keyword(items, "file", "Scoped fresh private file");
            push_keyword(items, "directory", "Scoped fresh private directory");
        } else {
            push_keyword(items, "temporary", "Scoped fresh path");
        }
        return;
    }
    if !matches!(words.get(2), Some(word) if matches!(*word, "file" | "directory")) {
        if words.len() == 3 && !ends_with_space {
            push_keyword(items, "file", "Scoped fresh private file");
            push_keyword(items, "directory", "Scoped fresh private directory");
        }
        return;
    }
    if words.len() == 3 {
        if ends_with_space {
            push_keyword(items, "as", "Bind the temporary path");
        } else {
            push_keyword(items, "file", "Scoped fresh private file");
            push_keyword(items, "directory", "Scoped fresh private directory");
        }
        return;
    }
    if words.get(3) != Some(&"as") {
        if words.len() == 4 && !ends_with_space {
            push_keyword(items, "as", "Bind the temporary path");
        }
        return;
    }
    if words.len() == 4 && !ends_with_space {
        push_keyword(items, "as", "Bind the temporary path");
    }
    // Past `as`: a new binding is being named, or the header is complete.
}

/// Complete `use ...` statements.
fn complete_use(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
) {
    match words.get(1).copied() {
        None | Some("") => {
            for form in ["use value", "use bytes", "use folder", "use environment", "use without environment"] {
                push_keyword(items, form, "Pipeline input or directory form");
            }
        }
        Some(form @ ("value" | "folder")) => {
            if words.contains(&"as") {
                return;
            }
            // A quoted literal never reaches `words`, so a trailing quote
            // with a space after it is a complete value even when no bare
            // word follows the parameter name.
            let quoted_value =
                ends_with_space && tokens.last().is_some_and(|token| token.quoted);
            let value_present = words.len() > 2 || quoted_value;
            if !value_present || !ends_with_space {
                push_value_items(items, source, line, None, None);
                return;
            }
            // A complete value: `use value <value> ` may bind with `as`,
            // while `use folder <path> ` is already complete.
            if form == "value" {
                push_keyword(items, "as", "Bind the value to a name");
            }
        }
        Some("bytes") => {
            // `use bytes <binding>`: the byte binding is fresh, so no value
            // (or any other) completion applies, matching every other new
            // binding position (`take ... as`, `spawn as`, `read ... as`).
        }
        Some("without") => {
            // `use without environment [...]`: the only follow-up is the
            // `environment` keyword itself.
            if words.get(2).is_none() || (words.len() == 3 && !ends_with_space) {
                push_keyword(items, "environment", "Remove workflow environment variables");
            }
        }
        _ => {}
    }
}

/// Complete `read ...` statements: file values in path position, `as` once
/// the path is complete, and nothing once the `as` binding starts (a new
/// binding is being named).
fn complete_read(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
) {
    if words.contains(&"as") {
        return;
    }
    // A quoted literal never reaches `words`, so a trailing quote with a
    // space after it is a complete path even when `words` shows none.
    let quoted_trailing = ends_with_space && tokens.last().is_some_and(|token| token.quoted);
    // `read from stdin [as <name>]`.
    if words.get(1) == Some(&"from") {
        if words.get(2) != Some(&"stdin") {
            if (ends_with_space && words.len() == 2)
                || (!ends_with_space && words.len() == 3)
            {
                push_keyword(items, "from stdin", "Read standard input");
            }
            return;
        }
        if words.len() == 3 && ends_with_space {
            push_keyword(items, "as", "Bind the input to a name");
        }
        return;
    }
    // Path form: an optional `file` keyword, then one path value.
    let mut cursor = 1;
    if words.get(cursor) == Some(&"file") {
        cursor += 1;
    }
    let path_present = words.len() > cursor || (quoted_trailing && words.len() == cursor);
    if !path_present {
        // `read ` or `read file `: the path is still missing.
        if words.len() == 1 {
            push_keyword(items, "file", "Read a file path");
            push_keyword(items, "from stdin", "Read standard input");
        }
        push_value_items(items, source, line, None, None);
        return;
    }
    if !ends_with_space {
        // A path being typed (bare partial or an open quote).
        push_value_items(items, source, line, None, None);
        if words.len() == 1 {
            push_keyword(items, "file", "Read a file path");
            push_keyword(items, "from stdin", "Read standard input");
            push_keyword(items, "as", "Bind the input to a name");
        }
        return;
    }
    // A complete path (`read "file" `): only `as` may follow.
    push_keyword(items, "as", "Bind the input to a name");
}

/// Complete `exit with status <value>`, `fail with message <text>` and
/// `return`. Only `fail with message` accepts a follow-up (`and status`);
/// `exit` and `return` are complete once their value is present, and bare
/// `return` completes nothing at all.
fn complete_value_statement(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
) {
    if words.contains(&"as") {
        return;
    }
    // A quoted literal never reaches `words`, so a trailing quote with a
    // space after it is a complete value even when `words` shows none.
    let quoted_trailing = ends_with_space && tokens.last().is_some_and(|token| token.quoted);
    match words[0] {
        "return" => {}
        "exit" => {
            if words.get(1) != Some(&"with") || words.get(2) != Some(&"status") {
                if words.len() == 1 && ends_with_space {
                    push_keyword(items, "with status", "Exit with an integer status");
                } else if words == ["exit", "with"] {
                    push_keyword(items, "status", "Exit with an integer status");
                }
                return;
            }
            let complete = ends_with_space && (words.len() > 3 || quoted_trailing);
            if !complete {
                push_value_items(items, source, line, None, None);
            }
        }
        _ => {
            // `fail with message <text> [and status <int>]`.
            if words.get(1) != Some(&"with") || words.get(2) != Some(&"message") {
                if words.len() == 1 && ends_with_space {
                    push_keyword(items, "with message", "Fail with a text message");
                } else if words == ["fail", "with"] {
                    push_keyword(items, "message", "Fail with a text message");
                }
                return;
            }
            if let Some(and_at) = words.iter().position(|word| *word == "and") {
                if words.get(and_at + 1) != Some(&"status") {
                    if (ends_with_space && words.len() == and_at + 1)
                        || (!ends_with_space && words.len() == and_at + 2)
                    {
                        push_keyword(items, "status", "Fail status after a message");
                    }
                    return;
                }
                let complete =
                    ends_with_space && (words.len() > and_at + 2 || quoted_trailing);
                if !complete {
                    push_value_items(items, source, line, None, None);
                }
                return;
            }
            let message_complete = ends_with_space && (words.len() > 3 || quoted_trailing);
            if message_complete {
                push_keyword(items, "and status", "Fail status after a message");
            } else {
                push_value_items(items, source, line, None, None);
            }
        }
    }
}

/// Complete the `then` transfer statements (`then parse json as <name>`,
/// `then encode <value> as json`, `then save|append to <path>`,
/// `then capture bytes as <name>`). These are complete statements that never
/// take invocation suffixes.
fn complete_then_statement(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
) {
    debug_assert!(words.first() == Some(&"then"));
    // A quoted literal never reaches `words`, so a trailing quote with a
    // space after it is a complete value even when `words` shows none.
    let quoted_trailing = ends_with_space && tokens.last().is_some_and(|token| token.quoted);
    match words.get(1) {
        Some(word) if matches!(*word, "parse" | "encode" | "save" | "append" | "capture") => {}
        _ => return,
    }
    match words[1] {
        "parse" => {
            // `then parse json as <name>`.
            if words.get(2) != Some(&"json") {
                if (ends_with_space && words.len() == 2)
                    || (!ends_with_space && words.len() == 3)
                {
                    push_keyword(items, "json", "Parse pipeline output as JSON");
                }
                return;
            }
            if words.len() == 3 {
                if ends_with_space {
                    push_keyword(items, "as", "Bind the parsed JSON");
                } else {
                    push_keyword(items, "json", "Parse pipeline output as JSON");
                }
                return;
            }
            if words.get(3) != Some(&"as") {
                if words.len() == 4 && !ends_with_space {
                    push_keyword(items, "as", "Bind the parsed JSON");
                }
                return;
            }
            if words.len() == 4 && !ends_with_space {
                push_keyword(items, "as", "Bind the parsed JSON");
            }
            // Past `as`: a new binding is being named, or the form is complete.
        }
        "encode" => {
            // `then encode <value> as json`.
            if words.len() == 2 {
                if ends_with_space {
                    push_value_items(items, source, line, None, None);
                }
                return;
            }
            let value_complete = ends_with_space && (words.len() > 3 || quoted_trailing);
            if words.len() == 3 && !value_complete {
                push_value_items(items, source, line, None, None);
                return;
            }
            if !value_complete {
                if words.len() == 4 && !ends_with_space {
                    push_keyword(items, "as json", "Encode the value as JSON");
                }
                return;
            }
            if words.get(3) != Some(&"as") || words.get(4) != Some(&"json") {
                if words.len() == 4 && !ends_with_space {
                    push_keyword(items, "as json", "Encode the value as JSON");
                }
                return;
            }
            // `then encode <value> as json` is complete.
        }
        "save" | "append" => {
            // `then save to <value>` (with an optional `file` keyword) and
            // `then append to <value>`.
            if words.get(2) != Some(&"to") {
                if (ends_with_space && words.len() == 2)
                    || (!ends_with_space && words.len() == 3)
                {
                    push_keyword(items, "to", "File to write pipeline output to");
                }
                return;
            }
            let mut cursor = 3;
            if words.get(cursor) == Some(&"file") {
                cursor += 1;
            }
            let value_complete =
                ends_with_space && (words.len() > cursor || quoted_trailing);
            if words.len() == cursor && !ends_with_space {
                push_keyword(items, "to", "File to write pipeline output to");
            } else if !value_complete {
                push_value_items(items, source, line, None, None);
                if words[1] == "save" && words.len() == cursor {
                    push_keyword(items, "file", "Read a file path");
                }
            }
            // A complete path ends the statement: no suffixes follow.
        }
        _ => {
            // `then capture bytes as <name>`.
            if words.get(2) != Some(&"bytes") {
                if (ends_with_space && words.len() == 2)
                    || (!ends_with_space && words.len() == 3)
                {
                    push_keyword(items, "bytes", "Bind raw pipeline bytes");
                }
                return;
            }
            if words.len() == 3 {
                if ends_with_space {
                    push_keyword(items, "as", "Bind the raw bytes");
                } else {
                    push_keyword(items, "bytes", "Bind raw pipeline bytes");
                }
                return;
            }
            if words.get(3) != Some(&"as") {
                if words.len() == 4 && !ends_with_space {
                    push_keyword(items, "as", "Bind the raw bytes");
                }
                return;
            }
            if words.len() == 4 && !ends_with_space {
                push_keyword(items, "as", "Bind the raw bytes");
            }
            // Past `as`: a new binding is being named, or the form is complete.
        }
    }
}

/// Complete job references (`await`, `inspect`, `cancel`) with spawned jobs.
/// Quoted literals never reach `words`, so a trailing quoted span is a job
/// being typed (`await "j`) or a complete job (`await "j" `): the former
/// offers jobs, the latter offers the same follow-ups as a bare job.
fn complete_job_reference(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    indent: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
) {
    let quoted_trailing = ends_with_space && tokens.last().is_some_and(|token| token.quoted);
    let quoted_value = !ends_with_space && tokens.last().is_some_and(|token| token.quoted);
    if words.len() == 1 && !ends_with_space && !quoted_value {
        push_statement_forms_scoped(items, source, line, indent);
        return;
    }
    if quoted_trailing {
        // A complete quoted job: follow-ups, never the job list again.
        if words[0] == "await" {
            push_keyword(items, "and keep status as", "Bind the job exit status");
            push_keyword(items, "and keep result as", "Bind the job result");
        } else if words[0] == "inspect" {
            push_keyword(items, "as", "Bind the snapshot to a name");
        }
        // `cancel <job>` is complete: nothing follows.
        return;
    }
    if quoted_value || words.len() == 1 || (words.len() == 2 && !ends_with_space) {
        for job in collect_jobs(source, line) {
            items.push(json!({
                "label": job.name,
                "kind": 6,
                "detail": format!("Background job · line {}", job.line),
                "insertText": job.name
            }));
        }
        return;
    }
    // `await <job> ...` takes `and keep status/result as` suffixes.
    if words[0] == "await" && ends_with_space {
        push_keyword(items, "and keep status as", "Bind the job exit status");
        push_keyword(items, "and keep result as", "Bind the job result");
        return;
    }
    // `inspect <job> ...` binds its snapshot with `as`.
    if words[0] == "inspect" && ends_with_space && words.len() == 2 {
        push_keyword(items, "as", "Bind the snapshot to a name");
    }
}

/// Complete an invocation line: applet names after `run`/`then`/`pipe to`,
/// parameter names after `with`/`and`, values after a parameter name, and
/// suffixes after a complete invocation.
#[allow(clippy::too_many_arguments)]
fn complete_invocation(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    indent: usize,
    tokens: &[CompletionWord],
    words: &[&str],
    ends_with_space: bool,
    registry: &Registry,
    manifests: &[&crate::manifest::Manifest],
) {
    // Skip the connection head: `run`, `then`, `pipe to`, `hand over to`,
    // `launch detached`, or a bare applet phrase.
    let mut head_len = 0;
    let mut head = InvocationHead::Bare;
    if words.first() == Some(&"pipe") && words.get(1) == Some(&"to") {
        head_len = 2;
        head = InvocationHead::Pipe;
    } else if words.first() == Some(&"hand")
        && words.get(1) == Some(&"over")
        && words.get(2) == Some(&"to")
    {
        head_len = 3;
        head = InvocationHead::Hand;
    } else if words.first() == Some(&"launch") && words.get(1) == Some(&"detached") {
        head_len = 2;
        head = InvocationHead::Launch;
    } else if matches!(words.first(), Some(first) if matches!(*first, "run" | "then" | "interactively")) {
        head_len = 1;
        head = if words[0] == "then" {
            InvocationHead::Then
        } else {
            InvocationHead::Run
        };
    }
    // A quoted token right after the head is a direct executable, which takes
    // `arguments` rather than manifest parameter names.
    let mut token_cursor = 0;
    for _ in 0..head_len {
        while token_cursor < tokens.len() && tokens[token_cursor].quoted {
            token_cursor += 1;
        }
        token_cursor += 1;
    }
    let direct = token_cursor < tokens.len() && tokens[token_cursor].quoted;
    // The operation runs to the first `with`, `as`, or `and`. Quoted direct
    // executables are absent from `words`, so the scan stays in bare-word
    // space and `direct` only changes how names and schemas resolve.
    let mut cursor = head_len;
    while cursor < words.len() && !matches!(words[cursor], "with" | "as" | "and") {
        cursor += 1;
    }
    if cursor >= words.len() || cursor == head_len {
        // Still typing the operation name.
        if matches!(head, InvocationHead::Hand | InvocationHead::Launch) {
            // Handover and detached launch wrap one native command: only a
            // quoted program path (or PATH executables) can follow.
            push_path_executables(items);
        } else {
            if head == InvocationHead::Then {
                // `then` transfer forms are also valid continuations.
                for form in [
                    "parse json as",
                    "encode",
                    "save to",
                    "append to",
                    "capture bytes as",
                ] {
                    push_keyword(items, form, "Then transfer form");
                }
            }
            push_applet_names(items, manifests);
            if let Ok(program) = crate::parser::parse(source) {
                push_program_names(items, &program);
            }
            push_path_executables(items);
        }
        return;
    }
    let rest = &words[cursor..];
    // `as <name>` defines a new binding: a complete one may still take suffixes.
    if rest.contains(&"as") {
        if ends_with_space && rest.len() >= 2 {
            push_suffix_forms_filtered(items, rest, direct, head, indent);
        }
        return;
    }
    // Find the trailing `with`/`and` segment.
    let last_keyword = rest
        .iter()
        .rposition(|word| matches!(*word, "with" | "and"));
    let Some(keyword_at) = last_keyword else {
        // Complete invocation with no dangling keyword: suffixes.
        if ends_with_space {
            push_suffix_forms_filtered(items, rest, direct, head, indent);
        }
        return;
    };
    let keyword = rest[keyword_at];
    let tail = &rest[keyword_at + 1..];
    // Suffix phrases after `and ...` complete from the suffix table.
    if keyword == "and"
        && tail.first().is_some_and(|word| {
            matches!(
                *word,
                "keep" | "retry" | "cache" | "discard" | "stdin" | "stderr" | "stdout"
                    | "environment" | "without" | "output"
            )
        })
    {
        if ends_with_space || tail.len() <= 3 {
            push_suffix_forms_filtered(items, rest, direct, head, indent);
        }
        return;
    }
    // What the operation resolves to: a manifest applet, a reusable program,
    // or a direct executable.
    let operation = words[head_len..cursor].join(" ");
    if keyword == "and" && tail.is_empty() {
        // `... and ` may start another parameter or a suffix.
        push_parameter_names(items, source, registry, &operation, direct);
        push_suffix_forms_filtered(items, rest, direct, head, indent);
        return;
    }
    if tail.is_empty() {
        // `with ` starts a parameter name.
        push_parameter_names(items, source, registry, &operation, direct);
        return;
    }
    // A quoted span never reaches `words`: a trailing quoted literal with a
    // space after it completes the `with <name> <value>` pair even though
    // the bare-word tail still has length one, while a quote with no
    // trailing space is still being typed.
    let quoted_trailing = ends_with_space && tokens.last().is_some_and(|token| token.quoted);
    let quoted_value = !ends_with_space && tokens.last().is_some_and(|token| token.quoted);
    if tail.len() == 1 && !ends_with_space && !quoted_value {
        // A partial parameter name.
        push_parameter_names(items, source, registry, &operation, direct);
        return;
    }
    if tail.len() == 1 && !quoted_trailing {
        // `with <name> ` or `and <name> `: a parameter value. Value positions
        // complete bindings and inputs, never parameter names.
        let schema = parameter_value_schema(source, registry, &operation, tail[0], direct);
        push_value_items(items, source, line, None, schema);
        return;
    }
    if tail.len() >= 2 && !ends_with_space && !quoted_value {
        // `with <name> <partial>` (or `and <name> <partial>`): a bare partial
        // value being typed. Quoted partials never reach `words`, so they
        // stay on the single-word path above.
        let schema = parameter_value_schema(source, registry, &operation, tail[0], direct);
        push_value_items(items, source, line, None, schema);
        return;
    }
    // A complete `with <name> <value>` pair: suffixes may follow.
    if ends_with_space {
        push_suffix_forms_filtered(items, rest, direct, head, indent);
    }
}

/// Push one keyword-style item (CompletionItemKind 14).
fn push_keyword(items: &mut Vec<JsonValue>, label: &str, detail: &str) {
    items.push(json!({
        "label": label,
        "kind": 14,
        "detail": detail,
        "insertText": label,
        "insertTextFormat": 1
    }));
}

/// Push one parser-generated statement-start form.
fn push_statement_form(items: &mut Vec<JsonValue>, form: &crate::parser::StatementCompletionForm) {
    match form.snippet {
        Some(snippet) => items.push(json!({
            "label": form.label,
            "kind": 15,
            "detail": form.detail,
            "insertText": snippet,
            "insertTextFormat": 2,
            "filterText": form.label
        })),
        None => items.push(json!({
            "label": form.label,
            "kind": 14,
            "detail": form.detail,
            "insertText": form.label,
            "insertTextFormat": 1
        })),
    }
}

/// Block context around the cursor for statement-start filtering. The parser
/// rejects `break`/`continue` outside loops, `return` outside reusable
/// programs, `catch`/`finally` outside a `try` clause position, `otherwise`
/// outside an `if` chain, and `whenever ...` outside `listen` bodies, so
/// those forms are hidden where they cannot parse.
struct CompletionScope {
    in_loop: bool,
    in_reusable: bool,
    in_listen: bool,
    allow_catch: bool,
    allow_finally: bool,
    allow_otherwise: bool,
}

/// Derive the enclosing block context from indentation of the lines before
/// `line`. `indent` is the cursor line's own indentation. This is a
/// completion heuristic over physical lines (raw blocks and continuations
/// can confuse it); it only hides forms that would certainly not parse.
fn completion_scope(source: &str, line: usize, indent: usize) -> CompletionScope {
    let mut scope = CompletionScope {
        in_loop: false,
        in_reusable: false,
        in_listen: false,
        allow_catch: false,
        allow_finally: false,
        allow_otherwise: false,
    };
    let previous: Vec<&str> = source.lines().take(line).collect();
    // Enclosing headers: walk upward, descending to each shallower indent.
    // `repeat`/`for`/`while` are the loop headers `parse_loop` handles;
    // `define` opens a reusable program; `whenever ...` lives in `listen`.
    let mut level = indent;
    for raw in previous.iter().rev() {
        let code = crate::parser::strip_comment(raw);
        let trimmed = code.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let depth = code.len() - trimmed.len();
        if depth >= level {
            continue;
        }
        level = depth;
        match trimmed.split_whitespace().next().unwrap_or("") {
            "repeat" | "for" | "while" => scope.in_loop = true,
            "define" => scope.in_reusable = true,
            "listen" => scope.in_listen = true,
            _ => {}
        }
        if level == 0 {
            break;
        }
    }
    // Clause positions attach at the same indent as their header: scan
    // upward past deeper body lines for a `try`/`if` header (or an existing
    // clause, which narrows what may still follow).
    for raw in previous.iter().rev() {
        let code = crate::parser::strip_comment(raw);
        let trimmed = code.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let depth = code.len() - trimmed.len();
        if depth > indent {
            continue;
        }
        if depth < indent {
            break;
        }
        let mut head_words = trimmed.split_whitespace();
        match head_words.next().unwrap_or("") {
            "try" => {
                scope.allow_catch = true;
                scope.allow_finally = true;
                break;
            }
            "catch" => {
                scope.allow_finally = true;
                break;
            }
            "finally" => break,
            "if" => {
                scope.allow_otherwise = true;
                break;
            }
            "otherwise" => {
                // `otherwise if` continues the chain; a bare `otherwise`
                // closes it.
                if head_words.next() == Some("if") {
                    scope.allow_otherwise = true;
                }
                break;
            }
            _ => break,
        }
    }
    scope
}

/// Push statement-start forms hidden where the parser would reject them
/// (loop-only, reusable-only and clause keywords outside their blocks).
fn push_statement_forms_scoped(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    indent: usize,
) {
    let scope = completion_scope(source, line, indent);
    for form in crate::parser::statement_completion_forms() {
        let allowed = match form.label.split(' ').next().unwrap_or("") {
            "break" | "continue" => scope.in_loop,
            "return" => scope.in_reusable,
            "catch" => scope.allow_catch,
            "finally" => scope.allow_finally,
            "otherwise" => scope.allow_otherwise,
            "whenever" => scope.in_listen,
            _ => true,
        };
        if allowed {
            push_statement_form(items, form);
        }
    }
}

/// Which connection head an invocation line uses. Handover and detached
/// launch wrap one native command with almost no suffixes; `then`/`pipe to`
/// cannot be `interactively` relaunched.
#[derive(Clone, Copy, PartialEq)]
enum InvocationHead {
    Run,
    Then,
    Pipe,
    Hand,
    Launch,
    Bare,
}

/// Push one parser-generated invocation suffix form.
fn push_suffix_form(items: &mut Vec<JsonValue>, form: &crate::parser::SuffixCompletionForm) {
    match form.snippet {
        Some(snippet) => items.push(json!({
            "label": form.label,
            "kind": 15,
            "detail": form.detail,
            "insertText": snippet,
            "insertTextFormat": 2,
            "filterText": form.label
        })),
        None => items.push(json!({
            "label": form.label,
            "kind": 14,
            "detail": form.detail,
            "insertText": form.label,
            "insertTextFormat": 1
        })),
    }
}

/// Push invocation suffixes filtered to what the grammar accepts here:
/// families already present are not repeated, suggestions stay in canonical
/// `suffixes` order (so following them never trips `parse.suffix_order`),
/// `and discard stderr` needs a direct command, `and keep secret` needs
/// `as`, `and retry` is withheld beside `and cache`/`and keep status`/`and
/// keep result` (and vice versa), `and output to` is launch-only, and
/// `interactively` is for a top-level `run` with no redirection, stdin,
/// environment or retry suffix (see `parse_interactive_run`).
fn push_suffix_forms_filtered(
    items: &mut Vec<JsonValue>,
    rest: &[&str],
    direct: bool,
    head: InvocationHead,
    indent: usize,
) {
    if head == InvocationHead::Hand {
        // `hand over to` takes no suffixes at all.
        return;
    }
    let allow_output = head == InvocationHead::Launch;
    // `parse_interactive_run` only accepts a leading `run` (never a bare
    // phrase, `then`, `pipe to`, handover or detached launch) at top level.
    let allow_interactive = head == InvocationHead::Run && indent == 0;
    // Bare words only: quoted bindings cannot collide with these families.
    let adjacent = |first: &str, second: &str| {
        rest.windows(2)
            .any(|window| window[0] == first && window[1] == second)
    };
    let has_as = rest.contains(&"as");
    let has_discard = adjacent("discard", "stderr");
    let has_status = adjacent("keep", "status");
    let has_result = adjacent("keep", "result");
    let has_secret = adjacent("keep", "secret");
    let has_cache = adjacent("and", "cache");
    let has_retry = adjacent("and", "retry");
    let has_output = adjacent("and", "output");
    let has_interactive = rest.contains(&"interactively");
    let has_stdin = adjacent("stdin", "from");
    let has_stderr_to = adjacent("stderr", "to");
    let has_stderr_appended = adjacent("stderr", "appended");
    let has_stderr_merged = adjacent("stderr", "merged");
    let has_stdout_to_stderr = adjacent("stdout", "to");
    let has_environment = adjacent("and", "environment");
    let has_without_environment = adjacent("without", "environment");
    // Highest canonical core rank already present; a newly suggested core
    // family must not sort before it.
    let mut max_core: Option<u8> = None;
    for (present, rank) in [
        (has_discard, 1),
        (has_status, 2),
        (has_result, 3),
        (has_secret, 4),
        (has_cache, 5),
        (has_retry, 6),
    ] {
        if present {
            max_core = Some(max_core.map_or(rank, |current: u8| current.max(rank)));
        }
    }
    let ordered = |rank: u8| max_core.map_or(true, |maximum| maximum <= rank);
    for form in crate::parser::invocation_suffix_forms() {
        let offer = match form.label {
            "as" => !has_as && ordered(0),
            "and discard stderr" => !has_discard && direct && ordered(1),
            "and keep status as" => !has_status && !has_retry && ordered(2),
            "and keep result as" => !has_result && !has_retry && ordered(3),
            "and keep secret" => !has_secret && has_as && ordered(4),
            "and cache" => !has_cache && !has_retry && ordered(5),
            "and retry up to" => !has_retry && !has_cache && !has_status && !has_result,
            "and stdin from" => !has_stdin,
            "and stderr to" => !has_stderr_to,
            "and stderr appended to" => !has_stderr_appended,
            "and stderr merged" => !has_stderr_merged,
            "and stdout to stderr" => !has_stdout_to_stderr,
            "and environment" => !has_environment,
            "and without environment" => !has_without_environment,
            "and output to" => allow_output && !has_output,
            "interactively" => {
                allow_interactive
                    && !has_interactive
                    && !has_stdin
                    && !has_environment
                    && !has_without_environment
                    && !has_stderr_to
                    && !has_stderr_appended
                    && !has_stderr_merged
                    && !has_stdout_to_stderr
                    && !has_retry
            }
            _ => true,
        };
        if offer {
            push_suffix_form(items, form);
        }
    }
}

/// Suffix forms are always pushed filtered (see `push_suffix_forms_filtered`);
/// there is no unfiltered push, so following a suggestion never teaches a
/// rejected or misordered combination.

/// Push applet names plus one snippet per applet with required parameters.
fn push_applet_names(items: &mut Vec<JsonValue>, manifests: &[&crate::manifest::Manifest]) {
    for manifest in manifests {
        let description = manifest
            .description
            .as_deref()
            .unwrap_or("Declared Execute applet");
        items.push(json!({
            "label": manifest.name,
            "kind": 3,
            "detail": description,
            "insertText": manifest.name,
            "insertTextFormat": 1,
            "filterText": format!("{} {} {}", manifest.name, manifest.contract()["phrases"].as_array().into_iter().flatten().filter_map(JsonValue::as_str).collect::<Vec<_>>().join(" "), description)
        }));
        let contract = manifest.contract();
        let required: Vec<&str> = contract
            .pointer("/parameters_schema/required")
            .and_then(JsonValue::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if required.is_empty() {
            continue;
        }
        let mut snippet = manifest.name.clone();
        for (index, parameter) in required.iter().enumerate() {
            snippet.push_str(if index == 0 { " with " } else { " and " });
            snippet.push_str(parameter);
            snippet.push_str(&format!(" ${{{}:{}}}", index + 1, parameter));
        }
        items.push(json!({
            "label": format!("{} with {}", manifest.name, required.join(" and ")),
            "kind": 15,
            "detail": "Snippet: call with required parameters",
            "insertText": snippet,
            "insertTextFormat": 2,
            "filterText": format!("{} {}", manifest.name, required.join(" "))
        }));
    }
}

/// Push same-file reusable programs as invocation targets.
fn push_program_names(items: &mut Vec<JsonValue>, program: &crate::ast::Program) {
    for name in program.definitions.keys() {
        items.push(json!({
            "label": name,
            "kind": 3,
            "detail": "Reusable program in this file",
            "insertText": name,
            "insertTextFormat": 1
        }));
    }
}

/// Opt-in quoted executables from `PATH` for direct calls. Bounded, sorted
/// and off by default (`EXECUTE_LSP_PATH_COMPLETION=1` enables it) so PATH
/// scans never surprise clients; entries are names only, never executed.
fn push_path_executables(items: &mut Vec<JsonValue>) {
    if std::env::var("EXECUTE_LSP_PATH_COMPLETION").as_deref() != Ok("1") {
        return;
    }
    const MAX_PATH_COMPLETIONS: usize = 64;
    let Some(paths) = std::env::var_os("PATH") else {
        return;
    };
    let mut names = BTreeSet::new();
    for dir in std::env::split_paths(&paths) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            names.insert(name);
            if names.len() >= MAX_PATH_COMPLETIONS {
                break;
            }
        }
        if names.len() >= MAX_PATH_COMPLETIONS {
            break;
        }
    }
    for name in names {
        items.push(json!({
            "label": name,
            "kind": 12,
            "detail": "Executable on PATH",
            "insertText": name,
            "insertTextFormat": 1,
            "filterText": format!("\"{name}")
        }));
    }
}

/// Push parameter names for an operation: manifest parameters, reusable
/// program inputs, or `arguments` for direct quoted executables.
fn push_parameter_names(
    items: &mut Vec<JsonValue>,
    source: &str,
    registry: &Registry,
    operation: &str,
    direct: bool,
) {
    if direct {
        items.push(json!({
            "label": "arguments",
            "kind": 5,
            "detail": "required parameter · list",
            "insertText": "arguments",
            "insertTextFormat": 1
        }));
        return;
    }
    if let Some(program_name) = operation.strip_prefix("program ") {
        if let Ok(program) = crate::parser::parse(source) {
            if let Some(definition) = program.definitions.get(program_name) {
                for input in &definition.inputs {
                    items.push(json!({
                        "label": input.name,
                        "kind": 5,
                        "detail": format!("program input · {}", input_type_label(&input.kind)),
                        "insertText": input.name,
                        "insertTextFormat": 1
                    }));
                }
            }
        }
        return;
    }
    let Ok(manifest) = registry.resolve(operation) else {
        return;
    };
    let contract = manifest.contract();
    let required: Vec<&str> = contract
        .pointer("/parameters_schema/required")
        .and_then(JsonValue::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(JsonValue::as_str)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let Some(properties) = contract
        .pointer("/parameters_schema/properties")
        .and_then(JsonValue::as_object)
    else {
        return;
    };
    for (name, schema) in properties {
        let is_required = required.iter().any(|value| *value == name);
        let detail = schema
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or("value");
        items.push(json!({
            "label": name,
            "kind": 5,
            "detail": format!("{} parameter · {}", if is_required { "required" } else { "optional" }, detail),
            "documentation": { "kind": "markdown", "value": format!("Applet `{}` parameter schema:\n\n```json\n{}\n```", manifest.name, serde_json::to_string_pretty(schema).unwrap_or_default()) },
            "insertText": name,
            "insertTextFormat": 1
        }));
    }
}

/// Resolve the value schema for a parameter in value position: manifest
/// parameter schemas (including `enum`), reusable program input choices, or
/// nothing when the type is unknown.
fn parameter_value_schema(
    source: &str,
    registry: &Registry,
    operation: &str,
    parameter: &str,
    direct: bool,
) -> Option<JsonValue> {
    if direct {
        return None;
    }
    if let Some(program_name) = operation.strip_prefix("program ") {
        let program = crate::parser::parse(source).ok()?;
        let definition = program.definitions.get(program_name)?;
        let input = definition.inputs.iter().find(|input| input.name == parameter)?;
        let choices = input.choices.clone().unwrap_or_default();
        if choices.is_empty() {
            return None;
        }
        return Some(json!({ "enum": choices }));
    }
    let manifest = registry.resolve(operation).ok()?;
    manifest
        .contract()
        .pointer(&format!(
            "/parameters_schema/properties/{}",
            escape_json_pointer(parameter)
        ))
        .cloned()
}

/// Escape one reference token for a JSON pointer lookup.
fn escape_json_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Push value-position completions: manifest `enum` values, `one of` choices
/// for the left-hand input, bindings in scope, declared inputs, then the
/// `true`/`false` literals.
fn push_value_items(
    items: &mut Vec<JsonValue>,
    source: &str,
    line: usize,
    left: Option<&str>,
    schema: Option<JsonValue>,
) {
    if let Some(values) = schema
        .as_ref()
        .and_then(|schema| schema.get("enum"))
        .and_then(JsonValue::as_array)
    {
        for value in values.iter().filter_map(JsonValue::as_str) {
            let quoted = format!("\"{value}\"");
            items.push(json!({
                "label": quoted,
                "kind": 12,
                "detail": "Allowed value",
                "insertText": quoted,
                "insertTextFormat": 1
            }));
        }
    }
    if let Some(name) = left {
        for input in collect_inputs(source) {
            if input.name == name {
                // Choice literals reuse their exact quoted spans.
                for choice in &input.choices {
                    items.push(json!({
                        "label": choice,
                        "kind": 12,
                        "detail": format!("Choice of input {name}"),
                        "insertText": choice,
                        "insertTextFormat": 1
                    }));
                }
            }
        }
    }
    for binding in collect_bindings(source, line) {
        items.push(json!({
            "label": binding.name,
            "kind": 6,
            "detail": format!("Binding · {} · line {}", binding.kind, binding.line),
            "insertText": binding.name,
            "insertTextFormat": 1
        }));
    }
    for input in collect_inputs(source) {
        items.push(json!({
            "label": input.name,
            "kind": 6,
            "detail": format!("Input · {}", input.kind),
            "insertText": input.name,
            "insertTextFormat": 1
        }));
    }
    for literal in ["true", "false"] {
        items.push(json!({
            "label": literal,
            "kind": 12,
            "detail": "Boolean literal",
            "insertText": literal,
            "insertTextFormat": 1
        }));
    }
}

/// A declared `accept` input with its display type and `one of` choices.
struct InputDecl {
    name: String,
    kind: String,
    choices: Vec<String>,
    line: usize,
}

/// A binding defined before the cursor line, with a guessed static type.
struct BindingDecl {
    name: String,
    kind: String,
    line: usize,
}

/// A `spawn as` job defined before the cursor line.
struct JobDecl {
    name: String,
    line: usize,
}

/// Display label for a declared input type.
fn input_type_label(kind: &crate::ast::InputType) -> &'static str {
    match kind {
        crate::ast::InputType::Text => "text",
        crate::ast::InputType::Integer
        | crate::ast::InputType::UnsignedInteger
        | crate::ast::InputType::Usize => "integer",
        crate::ast::InputType::Boolean => "boolean",
        crate::ast::InputType::TextList => "list of text",
        crate::ast::InputType::IntegerList
        | crate::ast::InputType::UnsignedIntegerList
        | crate::ast::InputType::UsizeList => "list of integer",
        crate::ast::InputType::JsonList => "list of json",
        crate::ast::InputType::Json => "json",
        crate::ast::InputType::Bytes => "bytes",
        crate::ast::InputType::Never => "never",
    }
}

/// Collect `accept` declarations in order. Choice literals reuse their exact
/// quoted spans so escaped text round-trips.
fn collect_inputs(source: &str) -> Vec<InputDecl> {
    let mut inputs = Vec::new();
    for (index, raw) in source.lines().enumerate() {
        let tokens = completion_words(crate::parser::strip_comment(raw));
        let words = bare_words(&tokens);
        if words.first() != Some(&"accept") || words.len() < 4 || words[2] != "as" {
            continue;
        }
        if !is_completion_binding_word(words[1]) {
            continue;
        }
        let mut end = 3;
        while end < words.len()
            && !matches!(words[end], "one" | "from" | "with" | "described")
        {
            end += 1;
        }
        let kind = if end > 3 {
            words[3..end].join(" ")
        } else {
            "value".to_owned()
        };
        inputs.push(InputDecl {
            name: words[1].to_owned(),
            kind,
            choices: parse_choice_literals(crate::parser::strip_comment(raw)),
            line: index + 1,
        });
    }
    inputs
}

/// Collect the quoted literals after `one of [` on one declaration line.
fn parse_choice_literals(code: &str) -> Vec<String> {
    let Some(one) = code.find("one of") else {
        return Vec::new();
    };
    let rest = &code[one + "one of".len()..];
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::new();
    let mut index = open + 1;
    while index < bytes.len() && out.len() < 256 {
        match bytes[index] {
            b'"' => {
                let start = index;
                index += 1;
                let mut escaped = false;
                while index < bytes.len() {
                    if escaped {
                        escaped = false;
                    } else if bytes[index] == b'\\' {
                        escaped = true;
                    } else if bytes[index] == b'"' {
                        break;
                    }
                    index += 1;
                }
                if index < bytes.len() {
                    if let Some(literal) = rest.get(start..=index) {
                        let inner = &literal[1..literal.len() - 1];
                        if !inner.is_empty() {
                            out.push(literal.to_owned());
                        }
                    }
                    index += 1;
                } else {
                    break;
                }
            }
            b']' => break,
            _ => {
                index += 1;
            }
        }
    }
    out
}

/// Whether a word can name a binding. Mirrors the parser's identifier rule
/// plus the reserved literals it rejects for bindings.
fn is_completion_binding_word(word: &str) -> bool {
    crate::parser::is_identifier(word) && !matches!(word, "true" | "false" | "null")
}

/// Words after `as` that name a type or keyword rather than a binding.
fn is_binding_keyword(word: &str) -> bool {
    matches!(word, "json" | "text" | "integer" | "boolean")
}

/// Guess a binding's static type from the defining line's head words.
fn guess_binding_kind(words: &[&str]) -> &'static str {
    if words.first() == Some(&"take") {
        if words.get(1) == Some(&"list") {
            return "list of text";
        }
        return match words.get(1).copied() {
            Some(kind @ ("text" | "integer" | "boolean" | "json")) => kind,
            _ => "binding",
        };
    }
    if words.contains(&"parse") {
        return "json";
    }
    if words.contains(&"capture") {
        return "bytes";
    }
    if words.first() == Some(&"read") {
        return "text";
    }
    if words.first() == Some(&"inspect") {
        return "snapshot";
    }
    if words.first() == Some(&"spawn") {
        return "job";
    }
    if words.first() == Some(&"whenever") {
        return "message";
    }
    if words.contains(&"temporary") {
        return "path";
    }
    if words.contains(&"status") {
        return "integer";
    }
    if words.contains(&"result") {
        return "result";
    }
    if words.first() == Some(&"use") && words.get(1) == Some(&"value") {
        return match words.get(2) {
            Some(literal) if literal.starts_with('"') => "text",
            Some(literal) if matches!(*literal, "true" | "false") => "boolean",
            Some(literal)
                if literal
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_digit() || first == '-') =>
            {
                "integer"
            }
            Some(literal) if literal.starts_with('[') => "list",
            Some(literal) if literal.starts_with('{') => "record",
            _ => "value",
        };
    }
    "binding"
}

/// Collect bindings defined on lines before the cursor line: trailing
/// `as NAME` definitions, `for each` iteration bindings and `catch` names.
fn collect_bindings(source: &str, before_line: usize) -> Vec<BindingDecl> {
    let mut bindings = Vec::new();
    for (index, raw) in source.lines().enumerate() {
        if index >= before_line {
            break;
        }
        let code = crate::parser::strip_comment(raw).trim();
        if code.is_empty() {
            continue;
        }
        let tokens = completion_words(code);
        let words = bare_words(&tokens);
        if words.is_empty() || matches!(words[0], "accept" | "define") {
            continue;
        }
        if words[0] == "for" && words.get(1) == Some(&"each") {
            if let Some(name) = words.get(2) {
                if is_completion_binding_word(name) {
                    bindings.push(BindingDecl {
                        name: (*name).to_owned(),
                        kind: "element".to_owned(),
                        line: index + 1,
                    });
                }
            }
            continue;
        }
        if words[0] == "parallel"
            && words.get(1) == Some(&"for")
            && words.get(2) == Some(&"each")
        {
            if let Some(name) = words.get(3) {
                if is_completion_binding_word(name) {
                    bindings.push(BindingDecl {
                        name: (*name).to_owned(),
                        kind: "element".to_owned(),
                        line: index + 1,
                    });
                }
            }
            continue;
        }
        if words[0] == "catch" {
            if let Some(name) = words.get(1) {
                if is_completion_binding_word(name) {
                    bindings.push(BindingDecl {
                        name: (*name).to_owned(),
                        kind: "error".to_owned(),
                        line: index + 1,
                    });
                }
            }
            continue;
        }
        let Some(as_at) = words.iter().rposition(|word| *word == "as") else {
            continue;
        };
        let Some(name) = words.get(as_at + 1) else {
            continue;
        };
        if !is_completion_binding_word(name) || is_binding_keyword(name) {
            continue;
        }
        let kind = guess_binding_kind(&words);
        bindings.push(BindingDecl {
            name: (*name).to_owned(),
            kind: kind.to_owned(),
            line: index + 1,
        });
    }
    bindings
}

/// Collect `spawn as` jobs defined before the cursor line.
fn collect_jobs(source: &str, before_line: usize) -> Vec<JobDecl> {
    collect_bindings(source, before_line)
        .into_iter()
        .filter(|binding| binding.kind == "job")
        .map(|binding| JobDecl {
            name: binding.name,
            line: binding.line,
        })
        .collect()
}

fn hover(
    params: &JsonValue,
    documents: &BTreeMap<String, Document>,
    manifests: &[&crate::manifest::Manifest],
) -> JsonValue {
    let Some((uri, position)) = text_document_position(params) else {
        return JsonValue::Null;
    };
    let Some(document) = documents.get(uri) else {
        return JsonValue::Null;
    };
    let Some(offset) = position_to_byte(&document.text, position) else {
        return JsonValue::Null;
    };
    if let Some((name, definition)) = program_symbol_at(&document.text, offset) {
        return json!({ "contents": { "kind": "markdown", "value": format!("Reusable program `{name}` defined at line {}.", definition.line) } });
    }
    if let Some(manifest) = manifest_at(&document.text, offset, manifests) {
        let contract = manifest.contract();
        let details = json!({
            "name": contract["name"],
            "parameters": contract["parameters_schema"],
            "output": contract["io"]["output_schema"],
            "ports": contract.get("ports").cloned().unwrap_or(JsonValue::Null),
            "execution": contract["execution"]
        });
        return json!({ "contents": { "kind": "markdown", "value": format!("### Execute applet `{}`\n\n```json\n{}\n```", manifest.name, serde_json::to_string_pretty(&details).unwrap_or_default()) } });
    }
    JsonValue::Null
}

fn manifest_at<'a>(
    source: &str,
    offset: usize,
    manifests: &[&'a crate::manifest::Manifest],
) -> Option<&'a crate::manifest::Manifest> {
    let line = line_for_byte(source, offset)?;
    let current_position = position_for_byte(source, offset);
    let line_abs = line_bounds(source, current_position.line)?.0;
    manifests.iter().copied().find(|manifest| {
        let contract = manifest.contract();
        std::iter::once(manifest.name.as_str())
            .chain(
                contract["phrases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(JsonValue::as_str),
            )
            .any(|phrase| {
                line.match_indices(phrase).any(|(start, _)| {
                    let end = start + phrase.len();
                    offset >= line_abs + start && offset <= line_abs + end
                })
            })
    })
}

fn definition(params: &JsonValue, documents: &BTreeMap<String, Document>) -> JsonValue {
    let Some((uri, position)) = text_document_position(params) else {
        return JsonValue::Null;
    };
    let Some(document) = documents.get(uri) else {
        return JsonValue::Null;
    };
    let Some(offset) = position_to_byte(&document.text, position) else {
        return JsonValue::Null;
    };
    let Some((_, symbol)) = program_symbol_at(&document.text, offset) else {
        return JsonValue::Null;
    };
    json!({ "uri": uri, "range": range_from_offsets(&document.text, symbol.start, symbol.end) })
}

fn references(params: &JsonValue, documents: &BTreeMap<String, Document>) -> JsonValue {
    let Some((uri, position)) = text_document_position(params) else {
        return JsonValue::Null;
    };
    let Some(document) = documents.get(uri) else {
        return JsonValue::Null;
    };
    let Some(offset) = position_to_byte(&document.text, position) else {
        return JsonValue::Null;
    };
    let Some((name, _)) = program_symbol_at(&document.text, offset) else {
        return JsonValue::Null;
    };
    let include_declaration = params
        .pointer("/context/includeDeclaration")
        .and_then(JsonValue::as_bool)
        .unwrap_or(true);
    let symbol = find_program_symbol(&document.text, &name);
    let Some(symbol) = symbol else {
        return json!([]);
    };
    let mut locations = symbol
        .references
        .iter()
        .map(|(start, end)| json!({ "uri": uri, "range": range_from_offsets(&document.text, *start, *end) }))
        .collect::<Vec<_>>();
    if include_declaration {
        locations.push(json!({ "uri": uri, "range": range_from_offsets(&document.text, symbol.declaration.start, symbol.declaration.end) }));
    }
    json!(locations)
}

fn prepare_rename(params: &JsonValue, documents: &BTreeMap<String, Document>) -> JsonValue {
    let Some((uri, position)) = text_document_position(params) else {
        return JsonValue::Null;
    };
    let Some(document) = documents.get(uri) else {
        return JsonValue::Null;
    };
    let Some(offset) = position_to_byte(&document.text, position) else {
        return JsonValue::Null;
    };
    let Some((name, symbol)) = program_word_at(&document.text, offset) else {
        return JsonValue::Null;
    };
    json!({ "range": range_from_offsets(&document.text, symbol.start, symbol.end), "placeholder": name })
}

fn rename(params: &JsonValue, documents: &BTreeMap<String, Document>) -> JsonValue {
    let Some((uri, position)) = text_document_position(params) else {
        return JsonValue::Null;
    };
    let Some(document) = documents.get(uri) else {
        return JsonValue::Null;
    };
    let Some(new_name) = params.get("newName").and_then(JsonValue::as_str) else {
        return JsonValue::Null;
    };
    if !valid_identifier(new_name) {
        return JsonValue::Null;
    }
    let Some(offset) = position_to_byte(&document.text, position) else {
        return JsonValue::Null;
    };
    let Some((old_name, _)) = program_symbol_at(&document.text, offset) else {
        return JsonValue::Null;
    };
    let Some(symbol) = find_program_symbol(&document.text, &old_name) else {
        return JsonValue::Null;
    };
    if old_name == new_name {
        return json!({ "changes": {} });
    }
    if program_symbols(&document.text)
        .iter()
        .any(|other| other.name == new_name)
    {
        return JsonValue::Null;
    }

    let mut edit_locations = symbol
        .references
        .iter()
        .copied()
        .chain(std::iter::once((
            symbol.declaration.start,
            symbol.declaration.end,
        )))
        .collect::<Vec<_>>();
    if edit_locations.len() > MAX_RENAME_EDITS {
        return JsonValue::Null;
    }
    edit_locations.sort_by_key(|(start, _)| *start);
    let edits = edit_locations
        .iter()
        .map(|(start, end)| {
            json!({ "range": range_from_offsets(&document.text, *start, *end), "newText": new_name })
        })
        .collect::<Vec<_>>();
    let mut ordered_edits = symbol
        .references
        .iter()
        .map(|(start, end)| (*start, *end))
        .chain(std::iter::once((
            symbol.declaration.start,
            symbol.declaration.end,
        )))
        .collect::<Vec<_>>();
    ordered_edits.sort_by_key(|(start, _)| *start);
    let mut replaced = document.text.clone();
    for (start, end) in ordered_edits.into_iter().rev() {
        replaced.replace_range(start..end, new_name);
    }
    if crate::parser::parse(&replaced).is_err() {
        return JsonValue::Null;
    }
    let mut changes = serde_json::Map::new();
    changes.insert(uri.to_owned(), json!(edits));
    json!({ "changes": changes })
}

#[derive(Debug, Clone)]
struct ProgramSymbol {
    name: String,
    declaration: SymbolLocation,
    references: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
struct SymbolLocation {
    start: usize,
    end: usize,
    line: usize,
}

fn program_symbol_at(source: &str, offset: usize) -> Option<(String, SymbolLocation)> {
    let (name, _) = program_word_at(source, offset)?;
    let declaration = find_program_symbol(source, &name)?.declaration;
    Some((name, declaration))
}

fn program_word_at(source: &str, offset: usize) -> Option<(String, SymbolLocation)> {
    let word = lexical_words(source)
        .into_iter()
        .find(|word| !word.quoted && word.text_start <= offset && offset <= word.text_end)?;
    let name = source[word.text_start..word.text_end].to_owned();
    let symbol = find_program_symbol(source, &name)?;
    let is_symbol_site = (word.text_start == symbol.declaration.start
        && word.text_end == symbol.declaration.end)
        || symbol
            .references
            .iter()
            .any(|(start, end)| *start == word.text_start && *end == word.text_end);
    if !is_symbol_site {
        return None;
    }
    Some((
        name,
        SymbolLocation {
            start: word.text_start,
            end: word.text_end,
            line: word.line,
        },
    ))
}

fn find_program_symbol(source: &str, name: &str) -> Option<ProgramSymbol> {
    program_symbols(source)
        .into_iter()
        .find(|symbol| symbol.name == name)
}

fn program_symbols(source: &str) -> Vec<ProgramSymbol> {
    let words = lexical_words(source);
    let mut declarations = BTreeMap::<String, SymbolLocation>::new();
    let mut duplicate_declarations = BTreeSet::<String>::new();
    let mut references = BTreeMap::<String, Vec<(usize, usize)>>::new();
    for index in 0..words.len() {
        let declaration_on_one_line = words.get(index).is_some_and(|word| {
            (index..=index + 4).all(|position| {
                words
                    .get(position)
                    .is_some_and(|candidate| candidate.line == word.line)
            })
        });
        if declaration_on_one_line
            && word_eq(source, &words, index, "define")
            && word_eq(source, &words, index + 1, "a")
            && word_eq(source, &words, index + 2, "program")
            && word_eq(source, &words, index + 3, "named")
        {
            if let Some(name) = words.get(index + 4).filter(|word| !word.quoted) {
                let text = source[name.text_start..name.text_end].to_owned();
                if declarations.contains_key(&text) {
                    duplicate_declarations.insert(text.clone());
                }
                declarations.entry(text.clone()).or_insert(SymbolLocation {
                    start: name.text_start,
                    end: name.text_end,
                    line: name.line,
                });
                references.entry(text).or_default();
            }
        }
        let same_line = |end: usize| {
            words.get(index).is_some_and(|word| {
                (index..=end).all(|position| {
                    words
                        .get(position)
                        .is_some_and(|candidate| candidate.line == word.line)
                })
            })
        };
        let is_program_call = same_line(index + 2)
            && (word_eq(source, &words, index, "run") || word_eq(source, &words, index, "then"))
            && word_eq(source, &words, index + 1, "program")
            || same_line(index + 3)
                && word_eq(source, &words, index, "pipe")
                && word_eq(source, &words, index + 1, "to")
                && word_eq(source, &words, index + 2, "program");
        if is_program_call {
            let name_index = if word_eq(source, &words, index, "pipe") {
                index + 3
            } else {
                index + 2
            };
            if let Some(name) = words.get(name_index).filter(|word| !word.quoted) {
                let text = source[name.text_start..name.text_end].to_owned();
                references
                    .entry(text)
                    .or_default()
                    .push((name.text_start, name.text_end));
            }
        }
    }
    declarations
        .into_iter()
        .filter(|(name, _)| !duplicate_declarations.contains(name))
        .map(|(name, declaration)| ProgramSymbol {
            references: references.remove(&name).unwrap_or_default(),
            name,
            declaration,
        })
        .collect()
}

fn word_eq(source: &str, words: &[Word], index: usize, expected: &str) -> bool {
    words
        .get(index)
        .is_some_and(|word| !word.quoted && &source[word.text_start..word.text_end] == expected)
}

fn valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric() || ch == '-')
}

fn lexical_words(source: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut absolute = 0;
    for (line_index, raw_line) in source.split('\n').enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let content = crate::parser::strip_comment(line);
        let bytes = content.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if matches!(bytes[index], b' ' | b'\t' | b'\r' | b'[' | b']' | b',') {
                index += 1;
                continue;
            }
            if bytes[index] == b'"' {
                let start = index;
                index += 1;
                let mut escaped = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        break;
                    }
                }
                words.push(Word {
                    text_start: absolute + start,
                    text_end: absolute + index,
                    line: line_index + 1,
                    quoted: true,
                });
                continue;
            }
            let start = index;
            while index < bytes.len()
                && !matches!(
                    bytes[index],
                    b' ' | b'\t' | b'\r' | b'[' | b']' | b',' | b'"'
                )
            {
                index += 1;
            }
            words.push(Word {
                text_start: absolute + start,
                text_end: absolute + index,
                line: line_index + 1,
                quoted: false,
            });
        }
        absolute += raw_line.len() + 1;
    }
    words
}

fn line_for_byte(source: &str, offset: usize) -> Option<&str> {
    let position = position_for_byte(source, offset.min(source.len()));
    let (start, end) = line_bounds(source, position.line)?;
    source.get(start..end)
}

fn range_from_offsets(source: &str, start: usize, end: usize) -> JsonValue {
    SourcePositions::new(source).range(source, start, end)
}

struct SourcePositions {
    line_starts: Vec<usize>,
}

impl SourcePositions {
    fn new(source: &str) -> Self {
        let mut line_starts =
            Vec::with_capacity(source.bytes().filter(|byte| *byte == b'\n').count() + 1);
        line_starts.push(0);
        line_starts.extend(source.match_indices('\n').map(|(offset, _)| offset + 1));
        Self { line_starts }
    }

    fn position(&self, source: &str, offset: usize) -> Position {
        let offset = offset.min(source.len());
        let line = self
            .line_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let start = self.line_starts[line];
        Position {
            line,
            character: source[start..offset].encode_utf16().count(),
        }
    }

    fn range(&self, source: &str, start: usize, end: usize) -> JsonValue {
        json!({
            "start": position_json(self.position(source, start)),
            "end": position_json(self.position(source, end))
        })
    }
}

fn position_json(position: Position) -> JsonValue {
    json!({ "line": position.line, "character": position.character })
}

fn position_for_byte(source: &str, offset: usize) -> Position {
    let offset = offset.min(source.len());
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_text = prefix.rsplit_once('\n').map_or(prefix, |(_, tail)| tail);
    Position {
        line,
        character: line_text.encode_utf16().count(),
    }
}

fn position_to_byte(source: &str, position: Position) -> Option<usize> {
    let (start, end) = line_bounds(source, position.line)?;
    let text = source.get(start..end)?;
    let mut utf16 = 0usize;
    for (byte_offset, ch) in text.char_indices() {
        if utf16 >= position.character {
            return Some(start + byte_offset);
        }
        utf16 += ch.len_utf16();
        if utf16 > position.character {
            // A position between the two UTF-16 code units of a non-BMP
            // scalar cannot be represented as a valid UTF-8 byte boundary.
            return None;
        }
    }
    (utf16 == position.character || position.character > utf16).then_some(end)
}

fn line_bounds(source: &str, line: usize) -> Option<(usize, usize)> {
    let mut start = 0;
    for current in 0..=line {
        let rest = source.get(start..)?;
        let Some(relative_end) = rest.find('\n') else {
            if current == line {
                return Some((start, source.len()));
            }
            return None;
        };
        let end = start + relative_end;
        if current == line {
            let end = if end > start && source.as_bytes()[end - 1] == b'\r' {
                end - 1
            } else {
                end
            };
            return Some((start, end));
        }
        start = end + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Registry;

    fn request(id: u64, method: &str, params: JsonValue) -> JsonValue {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn frame(message: JsonValue) -> Vec<u8> {
        let body = serde_json::to_vec(&message).unwrap();
        let mut result = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        result.extend_from_slice(&body);
        result
    }

    fn decode_frames(bytes: &[u8]) -> Vec<JsonValue> {
        let mut reader = BufReader::new(bytes);
        let mut messages = Vec::new();
        while let Some(message) = read_message(&mut reader).unwrap() {
            messages.push(message.unwrap());
        }
        messages
    }

    #[test]
    fn advertises_save_and_registers_file_watches_when_supported() {
        let mut input = frame(request(
            1,
            "initialize",
            json!({"capabilities":{"workspace":{"didChangeWatchedFiles":{"dynamicRegistration":true}}}}),
        ));
        input.extend(frame(
            json!({"jsonrpc":"2.0", "method":"initialized", "params":{}}),
        ));
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        assert_eq!(
            messages[0]["result"]["capabilities"]["textDocumentSync"]["save"]["includeText"],
            false
        );
        let registration = messages
            .iter()
            .find(|message| message["method"] == "client/registerCapability")
            .unwrap();
        assert_eq!(
            registration["params"]["registrations"][0]["method"],
            "workspace/didChangeWatchedFiles"
        );
        assert_eq!(
            registration["params"]["registrations"][0]["registerOptions"]["watchers"][0]
                ["globPattern"],
            "**/*"
        );
    }

    #[test]
    fn caller_diagnostics_follow_open_callee_and_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let child_dir = dir.path().join("sub");
        fs::create_dir(&child_dir).unwrap();
        let caller = dir.path().join("caller.program");
        let callee = child_dir.join("callee.program");
        let caller_text = "accept here as text from program directory\nrun program file \"sub/callee.program\" with theme \"dark\" as out\nuse value out\n";
        let callee_text = "accept theme as text\nuse value theme\n";
        fs::write(&caller, caller_text).unwrap();
        fs::write(&callee, callee_text).unwrap();
        let root_uri = url::Url::from_directory_path(dir.path())
            .unwrap()
            .to_string();
        let caller_uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let callee_uri = url::Url::from_file_path(&callee).unwrap().to_string();
        let mut input = frame(request(
            1,
            "initialize",
            json!({ "rootUri": root_uri, "capabilities": {} }),
        ));
        for (uri, text) in [(&caller_uri, caller_text), (&callee_uri, callee_text)] {
            input.extend(frame(json!({ "jsonrpc":"2.0", "method":"textDocument/didOpen", "params":{"textDocument":{"uri":uri,"version":1,"text":text}} })));
        }
        input.extend(frame(json!({"jsonrpc":"2.0", "method":"textDocument/didChange", "params":{"textDocument":{"uri":callee_uri,"version":2},"contentChanges":[{"text":"run\n"}]}})));
        input.extend(frame(json!({"jsonrpc":"2.0", "method":"textDocument/didSave", "params":{"textDocument":{"uri":callee_uri}}})));
        input.extend(frame(json!({"jsonrpc":"2.0", "method":"textDocument/didClose", "params":{"textDocument":{"uri":callee_uri}}})));
        input.extend(frame(json!({"jsonrpc":"2.0", "method":"workspace/didChangeWatchedFiles", "params":{"changes":[{"uri":callee_uri,"type":2}]}})));
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        input.extend(frame(json!({"jsonrpc":"2.0", "method":"exit"})));
        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let caller_notifications: Vec<_> = messages
            .iter()
            .filter(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == caller_uri
            })
            .collect();
        let callee_notifications: Vec<_> = messages
            .iter()
            .filter(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == callee_uri
            })
            .collect();
        assert!(caller_notifications.len() >= 6, "{caller_notifications:#?}");
        assert!(
            caller_notifications
                .iter()
                .all(|message| message["params"]["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|diagnostic| diagnostic["severity"] != 1)),
            "{caller_notifications:#?}"
        );
        assert!(
            callee_notifications.iter().any(|message| {
                message["params"]["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|diagnostic| diagnostic["severity"] == 1)
            }),
            "{callee_notifications:#?}"
        );
        assert!(
            callee_notifications.last().unwrap()["params"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{callee_notifications:#?}"
        );
    }

    #[test]
    fn lsp_publishes_exact_callee_parse_token_span() {
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller.program");
        let callee = dir.path().join("callee.program");
        let caller_text = "run program file \"callee.program\"\n";
        let callee_text = "run \"printf\" and discard stderr trailing\n";
        fs::write(&caller, caller_text).unwrap();
        fs::write(&callee, callee_text).unwrap();
        let root_uri = url::Url::from_directory_path(dir.path())
            .unwrap()
            .to_string();
        let caller_uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let callee_uri = url::Url::from_file_path(&callee).unwrap().to_string();
        let mut input = frame(request(
            1,
            "initialize",
            json!({ "rootUri": root_uri, "capabilities": {} }),
        ));
        for (uri, text) in [(&caller_uri, caller_text), (&callee_uri, callee_text)] {
            input.extend(frame(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {"textDocument": {"uri": uri, "version": 1, "text": text}}
            })));
        }
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        input.extend(frame(json!({"jsonrpc": "2.0", "method": "exit"})));
        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let publish = messages
            .iter()
            .rev()
            .find(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == callee_uri
            })
            .unwrap();
        let diagnostic = publish["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|diagnostic| diagnostic["code"] == "parse.syntax")
            .expect("parse error is published to the open callee");
        assert_eq!(diagnostic["range"]["start"]["line"], 0);
        assert_eq!(diagnostic["range"]["start"]["character"], 32);
        assert_eq!(diagnostic["range"]["end"]["character"], 40);
        assert_eq!(diagnostic["data"]["span_precision"], "token");
        assert_eq!(
            diagnostic["relatedInformation"][0]["location"]["uri"],
            caller_uri
        );
    }

    #[test]
    fn missing_callee_matches_path_aware_check_and_untitled_is_informational() {
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller.program");
        let source = "run program file \"missing.program\"\n";
        fs::write(&caller, source).unwrap();
        let uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let mut documents = BTreeMap::new();
        documents.insert(
            uri.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let diagnostics = diagnostics_for_document(
            &uri,
            &documents,
            &Workspace::default(),
            &Registry::default(),
        );
        let check =
            crate::diagnostics::analyze_source_at(source, Some(&caller), &Registry::default());
        assert_eq!(diagnostics, check);
        let mut input = frame(request(1, "initialize", json!({"capabilities": {}})));
        input.extend(frame(json!({"jsonrpc":"2.0", "method":"textDocument/didOpen", "params":{"textDocument":{"uri":uri,"version":1,"text":source}}})));
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let published = messages
            .iter()
            .find(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
            })
            .unwrap();
        let lsp = published["params"]["diagnostics"].as_array().unwrap();
        assert!(
            check.iter().all(|diagnostic| lsp.iter().any(
                |item| item["code"] == diagnostic.code && item["message"] == diagnostic.message
            )),
            "{published:#?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == DiagnosticSeverity::Error),
            "{diagnostics:#?}"
        );
        let untitled = "untitled:caller".to_owned();
        documents.insert(
            untitled.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let diagnostics = diagnostics_for_document(
            &untitled,
            &documents,
            &Workspace::default(),
            &Registry::default(),
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == DiagnosticSeverity::Information
                    && d.message.contains("saved local file")),
            "{diagnostics:#?}"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == DiagnosticSeverity::Error),
            "{diagnostics:#?}"
        );
    }

    #[test]
    fn exact_issue_example_publishes_only_existing_lint_not_file_call_error() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("sub");
        fs::create_dir(&child).unwrap();
        let caller = dir.path().join("caller.program");
        let callee = child.join("callee.program");
        let caller_text = "accept here as text from program directory\nrun program file \"sub/callee.program\" with theme \"dark\" as out\n";
        let callee_text = "accept theme as text\nuse value theme\n";
        fs::write(&caller, caller_text).unwrap();
        fs::write(&callee, callee_text).unwrap();
        let caller_uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let callee_uri = url::Url::from_file_path(&callee).unwrap().to_string();
        let root_uri = url::Url::from_directory_path(dir.path())
            .unwrap()
            .to_string();
        let mut input = frame(request(
            1,
            "initialize",
            json!({"rootUri":root_uri,"capabilities":{}}),
        ));
        for (uri, text) in [(&caller_uri, caller_text), (&callee_uri, callee_text)] {
            input.extend(frame(json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"version":1,"text":text}}})));
        }
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let published = messages
            .iter()
            .rfind(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == caller_uri
            })
            .unwrap();
        let diagnostics = published["params"]["diagnostics"].as_array().unwrap();
        assert_eq!(
            diagnostics
                .iter()
                .map(|d| (d["code"].as_str().unwrap(), d["severity"].as_u64().unwrap()))
                .collect::<Vec<_>>(),
            [("lint.unused_input", 2), ("lint.unused_binding", 2)],
            "{published:#?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d["code"] == "lint.unused_binding"
                    && d["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("out"))),
            "{published:#?}"
        );
    }

    #[test]
    fn watched_callee_deletion_republishes_caller_error() {
        struct DeleteBeforeWatch {
            frames: Vec<Vec<u8>>,
            frame: usize,
            offset: usize,
            callee: PathBuf,
        }
        impl Read for DeleteBeforeWatch {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.frame >= self.frames.len() {
                    return Ok(0);
                }
                if self.frame == 2 && self.offset == 0 {
                    fs::remove_file(&self.callee)?;
                }
                let remaining = &self.frames[self.frame][self.offset..];
                let count = output.len().min(remaining.len());
                output[..count].copy_from_slice(&remaining[..count]);
                self.offset += count;
                if self.offset == self.frames[self.frame].len() {
                    self.frame += 1;
                    self.offset = 0;
                }
                Ok(count)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller.program");
        let callee = dir.path().join("callee.program");
        let source = "run program file \"callee.program\"\n";
        fs::write(&caller, source).unwrap();
        fs::write(&callee, "use value \"ok\"\n").unwrap();
        let caller_uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let callee_uri = url::Url::from_file_path(&callee).unwrap().to_string();
        let root_uri = url::Url::from_directory_path(dir.path())
            .unwrap()
            .to_string();
        let input = DeleteBeforeWatch {
            frames: vec![
                frame(request(
                    1,
                    "initialize",
                    json!({"rootUri":root_uri,"capabilities":{}}),
                )),
                frame(
                    json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":caller_uri,"version":1,"text":source}}}),
                ),
                frame(
                    json!({"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{"changes":[{"uri":callee_uri,"type":3}]}}),
                ),
                frame(request(2, "shutdown", JsonValue::Null)),
            ],
            frame: 0,
            offset: 0,
            callee,
        };
        let mut output = Vec::new();
        run_with_io(input, &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let notifications: Vec<_> = messages
            .iter()
            .filter(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == caller_uri
            })
            .collect();
        assert_eq!(notifications.len(), 2, "{notifications:#?}");
        assert!(
            notifications[0]["params"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .all(|d| d["severity"] != 1),
            "{notifications:#?}"
        );
        assert!(
            notifications[1]["params"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["severity"] == 1
                    && d["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("not found"))),
            "{notifications:#?}"
        );
    }

    #[test]
    fn issue_example_resolves_program_directory_and_callee() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let caller = dir.path().join("caller.program");
        let source = "accept here as text from program directory\nrun program file \"sub/callee.program\" with theme \"dark\" as out\n";
        fs::write(&caller, source).unwrap();
        fs::write(
            dir.path().join("sub/callee.program"),
            "accept theme as text\nuse value theme\n",
        )
        .unwrap();
        let uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let mut documents = BTreeMap::new();
        documents.insert(
            uri.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let diagnostics = diagnostics_for_document(
            &uri,
            &documents,
            &Workspace::default(),
            &Registry::default(),
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity != DiagnosticSeverity::Error),
            "{diagnostics:#?}"
        );
    }

    #[test]
    fn scoped_loader_snapshots_exact_authorized_overlay_even_on_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let caller = root.join("caller.program");
        let callee = root.join("callee.program");
        fs::write(&caller, "run program file \"callee.program\"\n").unwrap();
        fs::write(&callee, "use value \"disk\"\n").unwrap();
        let mut overlays = BTreeMap::new();
        overlays.insert(callee.clone(), "run\n");
        let scope =
            crate::program_files::SourceScope::new(&root, &overlays).with_source_snapshots();
        let diagnostics = analyze_source_scoped(
            "run program file \"callee.program\"\n",
            Some(&caller),
            &Registry::default(),
            Some(&scope),
        );
        assert!(diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error));
        assert_eq!(scope.loaded_source(&callee).as_deref(), Some("run\n"));
    }

    #[test]
    fn unsaved_transitive_callee_change_rechecks_top_level_caller() {
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller.program");
        let middle = dir.path().join("middle.program");
        let leaf = dir.path().join("leaf.program");
        let source = "run program file \"middle.program\"\n";
        fs::write(&caller, source).unwrap();
        fs::write(&middle, "run program file \"leaf.program\"\n").unwrap();
        fs::write(&leaf, "use value \"ok\"\n").unwrap();
        let caller_uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let leaf_uri = url::Url::from_file_path(&leaf).unwrap().to_string();
        let mut documents = BTreeMap::new();
        documents.insert(
            caller_uri.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        documents.insert(
            leaf_uri.clone(),
            Document {
                text: "run\n".to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let failed = diagnostics_for_document(
            &caller_uri,
            &documents,
            &Workspace::default(),
            &Registry::default(),
        );
        assert!(
            failed
                .iter()
                .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error),
            "{failed:#?}"
        );
        documents.get_mut(&leaf_uri).unwrap().text = "use value \"fixed\"\n".to_owned();
        let fixed = diagnostics_for_document(
            &caller_uri,
            &documents,
            &Workspace::default(),
            &Registry::default(),
        );
        assert!(fixed.is_empty(), "{fixed:#?}");
    }

    #[test]
    fn called_file_diagnostic_uses_unsaved_overlay_and_callee_coordinates() {
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller.program");
        let callee = dir.path().join("callee.program");
        let caller_text =
            "run program file \"callee.program\"\nrun program file \"./callee.program\"\n";
        let disk_text = "accept theme as text\nrun \"printf\" with arguments [\"ok\"]\n";
        let overlay_text = "accept theme as text\nrun \"printf\" with arguments [them]\n";
        fs::write(&caller, caller_text).unwrap();
        fs::write(&callee, disk_text).unwrap();
        let caller_uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let callee_uri = url::Url::from_file_path(&callee).unwrap().to_string();
        let mut documents = BTreeMap::new();
        documents.insert(
            caller_uri.clone(),
            Document {
                text: caller_text.into(),
                version: Some(1),
                canonical_path: None,
            },
        );
        documents.insert(
            callee_uri,
            Document {
                text: overlay_text.into(),
                version: Some(2),
                canonical_path: None,
            },
        );
        let diagnostics = diagnostics_for_document(
            &caller_uri,
            &documents,
            &Workspace::default(),
            &Registry::default(),
        );
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "check.undefined_name")
            .expect("the unsaved callee typo is reported");
        assert_eq!(
            diagnostic.span.path.as_deref(),
            Some(callee.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(diagnostic.span.line, 2);
        assert_eq!(
            diagnostic.span.precision,
            crate::diagnostics::SpanPrecision::Token
        );
        assert!(diagnostic.message.contains("did you mean \"theme\""));
        assert_eq!(
            diagnostic.related[0].span.path.as_deref(),
            Some(caller.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(diagnostic.related[0].span.line, 1);
    }

    #[test]
    fn open_callee_receives_its_diagnostics_with_the_caller_as_related_information() {
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller.program");
        let callee = dir.path().join("callee.program");
        let caller_text = "run program file \"callee.program\"\n";
        let callee_disk = "accept theme as text\nrun \"printf\" with arguments [\"ok\"]\n";
        let callee_overlay = "accept theme as text\nrun \"printf\" with arguments [them]\n";
        fs::write(&caller, caller_text).unwrap();
        fs::write(&callee, callee_disk).unwrap();
        let root_uri = url::Url::from_directory_path(dir.path())
            .unwrap()
            .to_string();
        let caller_uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let callee_uri = url::Url::from_file_path(&callee).unwrap().to_string();
        let mut input = frame(request(
            1,
            "initialize",
            json!({ "rootUri": root_uri, "capabilities": {} }),
        ));
        input.extend(frame(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": caller_uri, "version": 1, "text": caller_text}}
        })));
        input.extend(frame(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": callee_uri, "version": 1, "text": callee_overlay}}
        })));
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        input.extend(frame(json!({"jsonrpc": "2.0", "method": "exit"})));
        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let caller_publish = messages
            .iter()
            .rev()
            .find(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == caller_uri
            })
            .unwrap();
        assert!(caller_publish["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|diagnostic| diagnostic["code"] != "check.undefined_name"));
        let callee_publish = messages
            .iter()
            .rev()
            .find(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == callee_uri
            })
            .unwrap();
        let diagnostic = callee_publish["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|diagnostic| diagnostic["code"] == "check.undefined_name")
            .expect("the callee receives its own diagnostic");
        assert_eq!(diagnostic["range"]["start"]["line"], 1);
        assert_eq!(
            diagnostic["relatedInformation"][0]["location"]["uri"],
            caller_uri
        );
        assert_eq!(
            diagnostic["relatedInformation"][0]["location"]["range"]["start"]["line"],
            0
        );
    }

    #[test]
    fn nested_open_callee_keeps_middle_and_outer_callsites_when_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let middle = dir.path().join("a-middle.program");
        let leaf = dir.path().join("m-leaf.program");
        let outer = dir.path().join("z-outer.program");
        let middle_text = "run program file \"m-leaf.program\"\n";
        let leaf_text = "run \"printf\" with arguments [missing]\n";
        let outer_text = "run program file \"a-middle.program\"\n";
        fs::write(&middle, middle_text).unwrap();
        fs::write(&leaf, leaf_text).unwrap();
        fs::write(&outer, outer_text).unwrap();

        let root_uri = url::Url::from_directory_path(dir.path())
            .unwrap()
            .to_string();
        let middle_uri = url::Url::from_file_path(&middle).unwrap().to_string();
        let leaf_uri = url::Url::from_file_path(&leaf).unwrap().to_string();
        let outer_uri = url::Url::from_file_path(&outer).unwrap().to_string();
        let mut input = frame(request(
            1,
            "initialize",
            json!({ "rootUri": root_uri, "capabilities": {} }),
        ));
        for (uri, text) in [
            (&middle_uri, middle_text),
            (&leaf_uri, leaf_text),
            (&outer_uri, outer_text),
        ] {
            input.extend(frame(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {"textDocument": {"uri": uri, "version": 1, "text": text}}
            })));
        }
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        input.extend(frame(json!({"jsonrpc": "2.0", "method": "exit"})));

        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let leaf_publish = messages
            .iter()
            .rev()
            .find(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == leaf_uri
            })
            .expect("leaf diagnostics are published");
        let diagnostic = leaf_publish["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|diagnostic| diagnostic["code"] == "check.undefined_name")
            .expect("leaf undefined-name diagnostic is routed to the leaf document");
        let related = diagnostic["relatedInformation"].as_array().unwrap();
        assert_eq!(related.len(), 2, "{related:#?}");
        assert_eq!(related[0]["location"]["uri"], middle_uri);
        assert_eq!(related[1]["location"]["uri"], outer_uri);
    }

    #[test]
    fn nested_callee_reports_closed_middle_callsite_with_captured_utf16_range() {
        let dir = tempfile::tempdir().unwrap();
        let middle = dir.path().join("a-middle.program");
        let leaf = dir.path().join("m-leaf.program");
        let outer = dir.path().join("z-outer.program");
        let middle_text = "run program file \"m-leaf.program\" # 🍎\n";
        let leaf_text = "run \"printf\" with arguments [missing]\n";
        let outer_text = "run program file \"a-middle.program\"\n";
        fs::write(&middle, middle_text).unwrap();
        fs::write(&leaf, leaf_text).unwrap();
        fs::write(&outer, outer_text).unwrap();

        let root_uri = url::Url::from_directory_path(dir.path())
            .unwrap()
            .to_string();
        let middle_uri = url::Url::from_file_path(fs::canonicalize(&middle).unwrap())
            .unwrap()
            .to_string();
        let leaf_uri = url::Url::from_file_path(&leaf).unwrap().to_string();
        let outer_uri = url::Url::from_file_path(&outer).unwrap().to_string();
        let mut input = frame(request(
            1,
            "initialize",
            json!({ "rootUri": root_uri, "capabilities": {} }),
        ));
        for (uri, text) in [(&leaf_uri, leaf_text), (&outer_uri, outer_text)] {
            input.extend(frame(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {"textDocument": {"uri": uri, "version": 1, "text": text}}
            })));
        }
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        input.extend(frame(json!({"jsonrpc": "2.0", "method": "exit"})));

        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let leaf_publish = messages
            .iter()
            .rev()
            .find(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == leaf_uri
            })
            .expect("leaf diagnostics are published");
        let diagnostic = leaf_publish["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|diagnostic| diagnostic["code"] == "check.undefined_name")
            .expect("leaf undefined-name diagnostic is routed to the leaf document");
        let related = diagnostic["relatedInformation"].as_array().unwrap();
        assert_eq!(related.len(), 2, "{related:#?}");
        assert_eq!(related[0]["location"]["uri"], middle_uri);
        assert_eq!(related[1]["location"]["uri"], outer_uri);
        assert_eq!(related[0]["location"]["range"]["start"]["line"], 0);
        assert_eq!(related[0]["location"]["range"]["start"]["character"], 0);
        assert_eq!(
            related[0]["location"]["range"]["end"]["character"],
            middle_text.trim_end_matches('\n').encode_utf16().count()
        );
    }

    #[cfg(unix)]
    #[test]
    fn replayed_file_diagnostics_use_the_original_loader_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let caller = root.join("caller.program");
        let callee = root.join("callee.program");
        std::os::unix::fs::symlink(&callee, root.join("alias.program")).unwrap();
        let caller_text =
            "run program file \"callee.program\"\nrun program file \"alias.program\"\n";
        fs::write(&caller, caller_text).unwrap();
        fs::write(
            &callee,
            "accept theme as text\nrun \"printf\" with arguments [them]\n",
        )
        .unwrap();
        let overlays = BTreeMap::new();
        let scope =
            crate::program_files::SourceScope::new(&root, &overlays).with_source_snapshots();
        let registry = Registry::default();
        let access = crate::program_files::Access {
            registry: &registry,
            cwd: &root,
        };
        crate::program_files::parse_at_scoped(
            caller_text,
            Some(&caller),
            Some(access),
            Some(&scope),
        )
        .unwrap();
        assert!(scope.loaded_source(&callee).is_some());
        fs::remove_file(&callee).unwrap();
        let replay = scope.replay();
        let diagnostics =
            analyze_source_scoped(caller_text, Some(&caller), &registry, Some(&replay));
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "check.undefined_name"
                    && diagnostic.span.path.as_deref() == callee.to_str()
                    && diagnostic.span.line == 2
            }),
            "{diagnostics:#?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_callee_overlay_survives_backing_file_removal() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let caller = root.join("caller.program");
        let callee = root.join("callee.program");
        let caller_text = "run program file \"callee.program\"\n";
        let overlay_text = "accept theme as text\nrun \"printf\" with arguments [them]\n";
        fs::write(&caller, caller_text).unwrap();
        fs::write(&callee, "run \"printf\" with arguments [\"disk\"]\n").unwrap();
        let mut overlays = BTreeMap::new();
        overlays.insert(callee.clone(), overlay_text);
        let scope =
            crate::program_files::SourceScope::new(&root, &overlays).with_source_snapshots();
        let registry = Registry::default();
        let access = crate::program_files::Access {
            registry: &registry,
            cwd: &root,
        };
        fs::remove_file(&callee).unwrap();

        crate::program_files::parse_at_scoped(
            caller_text,
            Some(&caller),
            Some(access),
            Some(&scope),
        )
        .expect("an open callee overlay should be usable without its disk file");
        assert_eq!(scope.loaded_source(&callee).as_deref(), Some(overlay_text));
    }

    #[cfg(unix)]
    #[test]
    fn open_callee_overlay_survives_target_removal_through_symlink_alias() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let caller = root.join("caller.program");
        let callee = root.join("callee.program");
        let alias = root.join("alias.program");
        let caller_text = "run program file \"alias.program\"\n";
        let overlay_text = "accept theme as text\nrun \"printf\" with arguments [them]\n";
        fs::write(&caller, caller_text).unwrap();
        fs::write(&callee, "run \"printf\" with arguments [\"disk\"]\n").unwrap();
        symlink(&callee, &alias).unwrap();
        let canonical_callee = fs::canonicalize(&callee).unwrap();
        let caller_uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let alias_uri = url::Url::from_file_path(&alias).unwrap().to_string();
        let mut documents = BTreeMap::new();
        documents.insert(
            caller_uri.clone(),
            Document {
                text: caller_text.to_owned(),
                version: Some(1),
                canonical_path: Some(fs::canonicalize(&caller).unwrap()),
            },
        );
        documents.insert(
            alias_uri.clone(),
            Document {
                text: overlay_text.to_owned(),
                version: Some(1),
                canonical_path: Some(canonical_callee.clone()),
            },
        );
        fs::remove_file(&callee).unwrap();

        let workspace = Workspace {
            roots: vec![root],
            explicitly_configured: true,
        };
        let mut output = Vec::new();
        publish_affected_diagnostics(
            &mut output,
            &documents,
            &workspace,
            &Registry::default(),
            &mut BTreeMap::new(),
            Some(&caller_uri),
        )
        .unwrap();
        let messages = decode_frames(&output);
        let routed = messages
            .iter()
            .find(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == alias_uri
            })
            .expect("callee diagnostics should route to the open alias URI");
        assert!(routed["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "check.undefined_name"));
    }

    #[test]
    fn routed_callee_diagnostics_refresh_when_the_caller_closes_or_error_is_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller.program");
        let callee = dir.path().join("callee.program");
        let caller_text = "run program file \"callee.program\"\n";
        let broken = "accept theme as text\nrun \"printf\" with arguments [them]\n";
        let fixed = "accept theme as text\nrun \"printf\" with arguments [\"ok\"]\n";
        fs::write(&caller, caller_text).unwrap();
        fs::write(&callee, fixed).unwrap();
        let root_uri = url::Url::from_directory_path(dir.path())
            .unwrap()
            .to_string();
        let caller_uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let callee_uri = url::Url::from_file_path(&callee).unwrap().to_string();
        let mut input = frame(request(
            1,
            "initialize",
            json!({ "rootUri": root_uri, "capabilities": {} }),
        ));
        input.extend(frame(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": caller_uri, "version": 1, "text": caller_text}}
        })));
        input.extend(frame(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": callee_uri, "version": 1, "text": broken}}
        })));
        input.extend(frame(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": {"textDocument": {"uri": caller_uri}}
        })));
        input.extend(frame(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": {"uri": callee_uri, "version": 2},
                "contentChanges": [{"text": fixed}]
            }
        })));
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        input.extend(frame(json!({"jsonrpc": "2.0", "method": "exit"})));
        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let callee_updates = messages
            .iter()
            .filter(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == callee_uri
            })
            .collect::<Vec<_>>();
        assert!(
            callee_updates.iter().any(|message| {
                message["params"]["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|diagnostic| {
                        diagnostic["code"] == "check.undefined_name"
                            && diagnostic.get("relatedInformation").is_none()
                    })
            }),
            "{callee_updates:#?}"
        );
        assert!(callee_updates.last().unwrap()["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|diagnostic| diagnostic["code"] != "check.undefined_name"));
    }

    #[test]
    fn workspace_root_allows_parent_directory_calls_but_not_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let caller = sub.join("caller.program");
        let source = "run program file \"../callee.program\"\n";
        fs::write(&caller, source).unwrap();
        fs::write(dir.path().join("callee.program"), "use value \"ok\"\n").unwrap();
        let uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let mut documents = BTreeMap::new();
        documents.insert(
            uri.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let workspace = Workspace {
            roots: vec![fs::canonicalize(dir.path()).unwrap()],
            explicitly_configured: true,
        };
        let diagnostics =
            diagnostics_for_document(&uri, &documents, &workspace, &Registry::default());
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        let fallback = diagnostics_for_document(
            &uri,
            &documents,
            &Workspace::default(),
            &Registry::default(),
        );
        assert!(
            fallback
                .iter()
                .any(|d| d.message.contains("outside the language server workspace")),
            "{fallback:#?}"
        );
    }

    #[test]
    fn callee_diagnostics_do_not_offer_edits_to_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller.program");
        let source = "run program file \"callee.program\"\n";
        fs::write(&caller, source).unwrap();
        fs::write(
            dir.path().join("callee.program"),
            "run fixture.echo with txet \"x\"\n",
        )
        .unwrap();
        let registry = Registry::from_sources(&[serde_json::json!({
            "name": "fixture.echo", "binary": "fixture", "parameters": { "text": {"type":"string"} }, "arguments": []
        }).to_string()]).unwrap();
        let uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let mut documents = BTreeMap::new();
        documents.insert(
            uri.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let diagnostics =
            diagnostics_for_document(&uri, &documents, &Workspace::default(), &registry);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == "applet.parameter.unknown"),
            "{diagnostics:#?}"
        );
        let actions = code_actions(&json!({"textDocument":{"uri":uri}}), &documents, &registry);
        assert!(actions.as_array().unwrap().is_empty(), "{actions:#?}");
    }

    #[test]
    fn diagnostics_report_text_lines_items_as_text_inside_for_each() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("typed-loop.program");
        let source = "run text.lines with text \"a\\nb\" as lines\nfor each line in lines\n  run text.repeat with text \"x\" and count line\n";
        fs::write(&program, source).unwrap();
        let uri = url::Url::from_file_path(&program).unwrap().to_string();
        let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let registry = Registry::load(&manifests).unwrap();
        let mut documents = BTreeMap::new();
        documents.insert(
            uri.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let workspace = Workspace {
            roots: vec![fs::canonicalize(directory.path()).unwrap()],
            explicitly_configured: true,
        };
        let diagnostics = diagnostics_for_document(&uri, &documents, &workspace, &registry);
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "applet.parameter.type"
                    && diagnostic.span.line == 3
                    && diagnostic.message.contains("parameter \"count\"")
                    && diagnostic.message.contains("expects integer")
                    && diagnostic.message.contains("result \"line\" is text")
            }),
            "{diagnostics:#?}"
        );
    }

    #[test]
    fn diagnostics_treat_path_list_items_as_text_inside_for_each() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("path-loop.program");
        let source = "run fixture.paths as paths\nfor each path in paths\n  run fixture.text with text path\n";
        fs::write(&program, source).unwrap();
        let uri = url::Url::from_file_path(&program).unwrap().to_string();
        let registry = Registry::from_sources(&[
            json!({
                "name": "fixture.paths",
                "binary": "fixture-paths",
                "parameters": {},
                "arguments": [],
                "contract": {
                    "version": 1,
                    "output": {
                        "type": "list",
                        "encoding": "json",
                        "items": {"type": "path"}
                    }
                }
            })
            .to_string(),
            json!({
                "name": "fixture.text",
                "binary": "fixture-text",
                "parameters": {"text": {"type": "string", "required": true}},
                "arguments": [{"parameter": "text"}]
            })
            .to_string(),
        ])
        .unwrap();
        let mut documents = BTreeMap::new();
        documents.insert(
            uri.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let workspace = Workspace {
            roots: vec![fs::canonicalize(directory.path()).unwrap()],
            explicitly_configured: true,
        };
        let diagnostics = diagnostics_for_document(&uri, &documents, &workspace, &registry);
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code != "applet.parameter.type"),
            "path items are text-valued in runtime loops: {diagnostics:#?}"
        );
    }

    #[test]
    fn caller_rechecks_callee_after_disk_changes_and_bounds_open_text() {
        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("caller.program");
        let callee = dir.path().join("callee.program");
        let source = "run program file \"callee.program\"\n";
        fs::write(&caller, source).unwrap();
        fs::write(&callee, "use value \"ok\"\n").unwrap();
        let uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let callee_uri = url::Url::from_file_path(&callee).unwrap().to_string();
        let mut documents = BTreeMap::new();
        documents.insert(
            uri.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let diagnostics = || {
            diagnostics_for_document(
                &uri,
                &documents,
                &Workspace::default(),
                &Registry::default(),
            )
        };
        assert!(!diagnostics()
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error));
        fs::write(&callee, "run\n").unwrap();
        assert!(diagnostics()
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error));
        fs::write(&callee, "use value \"ok\"\n").unwrap();
        documents.insert(
            callee_uri,
            Document {
                text: "x".repeat(1024 * 1024 + 1),
                version: Some(1),
                canonical_path: None,
            },
        );
        let bounded = diagnostics_for_document(
            &uri,
            &documents,
            &Workspace::default(),
            &Registry::default(),
        );
        assert!(
            bounded
                .iter()
                .any(|d| d.message.contains("larger than 1 MiB")),
            "{bounded:#?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn callee_cannot_escape_workspace_through_a_symlink() {
        use std::os::unix::fs::symlink;
        let workspace_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(
            outside.path().join("outside.program"),
            "use value \"secret\"\n",
        )
        .unwrap();
        symlink(
            outside.path().join("outside.program"),
            workspace_dir.path().join("link.program"),
        )
        .unwrap();
        let caller = workspace_dir.path().join("caller.program");
        let source = "run program file \"link.program\"\n";
        fs::write(&caller, source).unwrap();
        let uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let mut documents = BTreeMap::new();
        documents.insert(
            uri.clone(),
            Document {
                text: source.to_owned(),
                version: Some(1),
                canonical_path: None,
            },
        );
        let workspace = Workspace {
            roots: vec![fs::canonicalize(workspace_dir.path()).unwrap()],
            explicitly_configured: true,
        };
        let diagnostics =
            diagnostics_for_document(&uri, &documents, &workspace, &Registry::default());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("outside the language server workspace")),
            "{diagnostics:#?}"
        );
        assert!(!format!("{diagnostics:?}").contains("secret"));
        let root = fs::canonicalize(workspace_dir.path()).unwrap();
        let outside_path = fs::canonicalize(outside.path().join("outside.program")).unwrap();
        let overlays = BTreeMap::new();
        let scope =
            crate::program_files::SourceScope::new(&root, &overlays).with_source_snapshots();
        let _ = analyze_source_scoped(source, Some(&caller), &Registry::default(), Some(&scope));
        assert!(scope.loaded_source(&outside_path).is_none());
    }

    #[test]
    fn legacy_root_path_scopes_file_calls_and_root_uri_takes_precedence() {
        let legacy = tempfile::tempdir().unwrap();
        let modern = tempfile::tempdir().unwrap();
        let legacy_root = fs::canonicalize(legacy.path()).unwrap();
        let modern_root = fs::canonicalize(modern.path()).unwrap();
        let workspace = Workspace::from_initialize(&json!({"rootPath": legacy.path()}));
        assert_eq!(
            workspace.root_for(&legacy_root.join("sub/caller.program")),
            Some(legacy_root.clone())
        );
        assert_eq!(
            workspace.root_for(&modern_root.join("caller.program")),
            None
        );
        let modern_uri = url::Url::from_directory_path(modern.path())
            .unwrap()
            .to_string();
        let workspace =
            Workspace::from_initialize(&json!({"rootUri": modern_uri, "rootPath": legacy.path()}));
        assert_eq!(
            workspace.root_for(&modern_root.join("caller.program")),
            Some(modern_root)
        );
        assert_eq!(
            workspace.root_for(&legacy_root.join("caller.program")),
            None
        );
        let invalid = Workspace::from_initialize(&json!({"rootPath": "relative/path"}));
        assert!(invalid
            .root_for(&legacy_root.join("caller.program"))
            .is_none());
    }

    #[test]
    fn legacy_root_path_is_used_for_lsp_file_call_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let caller = sub.join("caller.program");
        let source = "run program file \"../callee.program\"\n";
        fs::write(&caller, source).unwrap();
        fs::write(dir.path().join("callee.program"), "use value \"ok\"\n").unwrap();
        let uri = url::Url::from_file_path(&caller).unwrap().to_string();
        let mut input = frame(request(
            1,
            "initialize",
            json!({"rootPath": dir.path(), "capabilities": {}}),
        ));
        input.extend(frame(json!({"jsonrpc":"2.0", "method":"textDocument/didOpen", "params":{"textDocument":{"uri":uri,"version":1,"text":source}}})));
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        let published = messages
            .iter()
            .find(|message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
            })
            .unwrap();
        assert!(
            published["params"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{published:#?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_scope_rejects_parent_traversal_before_capturing_a_source() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let root = fs::canonicalize(&workspace).unwrap();
        let caller = root.join("caller.program");
        let outside = parent.path().join("outside.program");
        fs::write(&outside, "use value \"out-of-workspace-secret-sentinel\"\n").unwrap();
        let source = "run program file \"../outside.program\"\n";
        fs::write(&caller, source).unwrap();
        let overlays = BTreeMap::new();
        let scope =
            crate::program_files::SourceScope::new(&root, &overlays).with_source_snapshots();
        let diagnostics =
            analyze_source_scoped(source, Some(&caller), &Registry::default(), Some(&scope));
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("outside the language server workspace")),
            "{diagnostics:#?}"
        );
        assert!(scope
            .loaded_source(&fs::canonicalize(&outside).unwrap())
            .is_none());
        assert!(!format!("{diagnostics:?}").contains("out-of-workspace-secret-sentinel"));
    }

    #[test]
    fn removed_workspace_folder_revokes_file_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let uri = url::Url::from_directory_path(dir.path())
            .unwrap()
            .to_string();
        let mut workspace =
            Workspace::from_initialize(&json!({"workspaceFolders": [{"uri": uri}]}));
        assert!(workspace.root_for(&root.join("caller.program")).is_some());
        workspace.apply_folder_changes(&json!({"event": {"removed": [{"uri": uri}], "added": []}}));
        assert!(workspace.root_for(&root.join("caller.program")).is_none());
        workspace.apply_folder_changes(&json!({"event": {"removed": [], "added": [{"uri": uri}]}}));
        assert!(workspace.root_for(&root.join("caller.program")).is_some());
    }

    #[test]
    fn document_uri_must_be_local_and_preserves_encoded_filename_characters() {
        assert!(local_file_path("https://example.com/test.program").is_none());
        assert!(local_file_path("file://remote.example/test.program").is_none());
        assert!(local_file_path("file:///tmp/test.program?x=1").is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a #+雪.program");
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        assert_eq!(local_file_path(&uri).as_deref(), Some(path.as_path()));
    }

    #[test]
    fn code_actions_offer_each_suggested_fix_as_a_quickfix() {
        let mut input = frame(request(1, "initialize", json!({ "capabilities": {} })));
        input.extend(frame(json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": "file:///tmp/typo.program", "languageId": "execute-program", "version": 1, "text": "echo hello\n" } }
        })));
        input.extend(frame(request(2, "textDocument/codeAction", json!({
            "textDocument": { "uri": "file:///tmp/typo.program" },
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } },
            "context": { "diagnostics": [] }
        }))));
        input.extend(frame(request(3, "shutdown", JsonValue::Null)));
        input.extend(frame(json!({ "jsonrpc": "2.0", "method": "exit" })));

        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        assert_eq!(
            messages[0]["result"]["capabilities"]["codeActionProvider"]["codeActionKinds"][0],
            "quickfix"
        );
        let actions = messages
            .iter()
            .find(|message| message["id"] == 2)
            .expect("codeAction response")["result"]
            .as_array()
            .expect("an array of actions")
            .clone();
        assert_eq!(actions.len(), 1, "{actions:#?}");
        let action = &actions[0];
        assert_eq!(action["kind"], "quickfix");
        assert_eq!(action["diagnostics"][0]["code"], "applet.unknown");
        let edit = &action["edit"]["changes"]["file:///tmp/typo.program"][0];
        assert_eq!(edit["newText"], "run \"echo\" with arguments [\"hello\"]");
        assert_eq!(edit["range"]["start"]["character"], 0);
        assert_eq!(edit["range"]["end"]["character"], 10);
    }

    #[test]
    fn stdio_server_advertises_real_features_and_publishes_diagnostics() {
        let mut input = frame(request(1, "initialize", json!({ "capabilities": {} })));
        input.extend(frame(json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": "file:///tmp/main.program", "languageId": "execute-program", "version": 1, "text": "run\n" } }
        })));
        input.extend(frame(request(2, "shutdown", JsonValue::Null)));
        input.extend(frame(json!({ "jsonrpc": "2.0", "method": "exit" })));

        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        assert!(messages[0]["result"]["capabilities"]["completionProvider"].is_object());
        assert_eq!(
            messages[0]["result"]["capabilities"]["positionEncoding"],
            "utf-16"
        );
        assert!(messages.iter().any(|message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["diagnostics"][0]["code"] == "parse.syntax"
        }));
        assert_eq!(messages.last().unwrap()["result"], JsonValue::Null);
    }

    #[test]
    fn reusable_program_definition_references_and_rename_return_valid_edits() {
        let uri = "file:///tmp/main.program";
        let source = "define a program named emit\n  run printf\nrun program emit\n";
        let mut input = frame(request(1, "initialize", JsonValue::Null));
        input.extend(frame(json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": uri, "version": 1, "text": source } }
        })));
        input.extend(frame(request(
            2,
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri }, "position": { "line": 2, "character": 17 }
            }),
        )));
        input.extend(frame(request(
            3,
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri }, "position": { "line": 2, "character": 17 },
                "context": { "includeDeclaration": true }
            }),
        )));
        input.extend(frame(request(4, "textDocument/rename", json!({
            "textDocument": { "uri": uri }, "position": { "line": 2, "character": 17 }, "newName": "emit_text"
        }))));
        input.extend(frame(request(5, "shutdown", JsonValue::Null)));
        input.extend(frame(json!({ "jsonrpc": "2.0", "method": "exit" })));

        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let messages = decode_frames(&output);
        assert_eq!(
            messages.iter().find(|message| message["id"] == 2).unwrap()["result"]["range"]["start"]
                ["line"],
            0
        );
        assert_eq!(
            messages.iter().find(|message| message["id"] == 3).unwrap()["result"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let edits =
            &messages.iter().find(|message| message["id"] == 4).unwrap()["result"]["changes"][uri];
        assert_eq!(edits.as_array().unwrap().len(), 2);
        assert!(edits
            .as_array()
            .unwrap()
            .iter()
            .all(|edit| edit["newText"] == "emit_text"));
    }

    #[test]
    fn utf16_lsp_positions_round_trip_non_bmp_text() {
        let source = "accept title as text with default \"🐈\"\nrun printf\n";
        let offset = source.find("run").unwrap();
        let position = position_for_byte(source, offset);
        assert_eq!(position.line, 1);
        assert_eq!(position_to_byte(source, position), Some(offset));
    }

    #[test]
    fn utf16_positions_respect_astral_character_boundaries() {
        let source = "a🐈b\n";
        assert_eq!(
            position_to_byte(
                source,
                Position {
                    line: 0,
                    character: 1
                }
            ),
            Some(1)
        );
        assert_eq!(
            position_to_byte(
                source,
                Position {
                    line: 0,
                    character: 2
                }
            ),
            None
        );
        assert_eq!(
            position_to_byte(
                source,
                Position {
                    line: 0,
                    character: 3
                }
            ),
            Some(5)
        );
        assert_eq!(
            position_to_byte(
                source,
                Position {
                    line: 0,
                    character: 4
                }
            ),
            Some(6)
        );
    }

    #[test]
    fn invalid_late_change_does_not_partially_mutate_document() {
        let mut document = Document {
            text: "alpha beta\n".to_owned(),
            version: Some(4),
            canonical_path: None,
        };
        let params = json!({
            "textDocument": { "version": 5 },
            "contentChanges": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 5 }
                    },
                    "text": "gamma"
                },
                {
                    "range": {
                        "start": { "line": 9, "character": 0 },
                        "end": { "line": 9, "character": 0 }
                    },
                    "text": "bad"
                }
            ]
        });
        assert!(apply_content_changes(&mut document, &params).is_err());
        assert_eq!(document.text, "alpha beta\n");
        assert_eq!(document.version, Some(4));
    }

    #[test]
    fn oversized_unterminated_header_is_rejected_without_reading_unbounded_line() {
        let mut bytes = b"X-Header: ".to_vec();
        bytes.resize(MAX_HEADER_BYTES + 1, b'x');
        let error = read_message(&mut BufReader::new(bytes.as_slice())).unwrap_err();
        assert!(error.to_string().contains("headers exceed"));
    }

    #[test]
    fn stale_document_version_is_rejected() {
        let mut document = Document {
            text: "run printf\n".to_owned(),
            version: Some(4),
            canonical_path: None,
        };
        let params = json!({
            "textDocument": { "version": 4 },
            "contentChanges": [{ "text": "run echo\n" }]
        });
        assert!(apply_content_changes(&mut document, &params).is_err());
        assert_eq!(document.text, "run printf\n");
    }

    #[test]
    fn duplicate_content_length_headers_are_rejected() {
        let bytes = b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}";
        let error = read_message(&mut BufReader::new(bytes.as_slice())).unwrap_err();
        assert!(error.to_string().contains("duplicate Content-Length"));
    }

    #[test]
    fn malformed_json_frame_gets_parse_error_and_server_continues() {
        let initialize = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        }))
        .unwrap();
        let shutdown =
            serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"})).unwrap();
        let mut input = Vec::new();
        for body in [
            initialize.as_slice(),
            b"{bad json}".as_slice(),
            shutdown.as_slice(),
        ] {
            input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
            input.extend_from_slice(body);
        }
        let mut output = Vec::new();
        run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("-32700"), "{output}");
        assert!(
            output.contains("\"id\":2"),
            "shutdown reply missing: {output}"
        );
    }

    #[test]
    fn completion_suggests_applets_and_parameters_after_whitespace() {
        let manifest = serde_json::json!({
            "name": "fixture.echo",
            "description": "Print text to standard output.",
            "binary": "echo",
            "parameters": { "text": { "type": "string", "required": true } },
            "arguments": [{ "parameter": "text" }]
        });
        let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
        let uri = "file:///tmp/completion.program";
        let document = Document {
            text: "run \nrun fixture.echo with ".to_owned(),
            version: Some(1),
            canonical_path: None,
        };
        let documents = BTreeMap::from([(uri.to_owned(), document)]);

        let command = completion(
            &json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 4 }
            }),
            &documents,
            &registry,
            &registry.manifests().expect("fixture manifests parse"),
        );
        assert!(command["items"].as_array().unwrap().iter().any(|item| {
            item["label"] == "fixture.echo"
                && item["detail"] == "Print text to standard output."
                && item["filterText"]
                    .as_str()
                    .unwrap()
                    .contains("standard output")
        }));

        let parameter = completion(
            &json!({
                "textDocument": { "uri": uri },
                "position": { "line": 1, "character": 22 }
            }),
            &documents,
            &registry,
            &registry.manifests().expect("fixture manifests parse"),
        );
        assert!(parameter["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| { item["label"] == "text" }));
    }

    fn complete_source(
        source: &str,
        line: u64,
        character: u64,
        registry: &Registry,
        manifests: &[&crate::manifest::Manifest],
    ) -> JsonValue {
        let uri = "file:///tmp/completion-case.program";
        let document = Document {
            text: source.to_owned(),
            version: Some(1),
            canonical_path: None,
        };
        let documents = BTreeMap::from([(uri.to_owned(), document)]);
        completion(
            &json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
            &documents,
            registry,
            manifests,
        )
    }

    fn fixture_registry() -> Registry {
        let manifest = serde_json::json!({
            "name": "text.upper",
            "description": "Uppercase text.",
            "binary": "upper",
            "parameters": { "text": { "type": "string", "required": true } },
            "arguments": [{ "parameter": "text" }]
        });
        Registry::from_sources(&[manifest.to_string()]).unwrap()
    }

    #[test]
    fn completion_statement_start_lists_generated_forms_with_snippets() {
        let registry = Registry::default();
        let result = complete_source("", 0, 0, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        for required in [
            "try",
            "use value",
            "take",
            "on exit",
            "on interrupt",
            "with temporary file as",
            "launch detached",
            "hand over to",
            "exit with status",
            "fail with message",
            "then save to",
            "then append to",
        ] {
            assert!(
                items.iter().any(|item| item["label"] == required),
                "statement completion is missing `{required}`"
            );
        }
        let statement = items.iter().find(|item| item["label"] == "try").unwrap();
        assert_eq!(statement["insertTextFormat"], 2);
        assert!(statement["insertText"]
            .as_str()
            .unwrap()
            .contains("${"));
        // Loop-only, reusable-only and clause forms are hidden outside their
        // blocks (see `completion_scope`); they are covered by the scoped
        // test below instead.
        for hidden in [
            "break",
            "continue",
            "return",
            "catch",
            "finally",
            "otherwise",
            "otherwise if",
            "whenever a line arrives as",
        ] {
            assert!(
                !items.iter().any(|item| item["label"] == hidden),
                "top-level completion must not offer `{hidden}`, got {items:?}"
            );
        }
        // The keyword list is generated from the parser: every statement head
        // must start a completion form (`cd` is an operation, `interactively`
        // and `redirect` are suffixes, `until` reserves a future word).
        for keyword in crate::parser::STATEMENT_KEYWORDS {
            if ["cd", "interactively", "redirect", "until"].contains(keyword) {
                continue;
            }
            if ["break", "continue", "return", "catch", "finally", "otherwise", "whenever"]
                .contains(keyword)
            {
                continue;
            }
            assert!(
                items.iter().any(|item| {
                    item["label"]
                        .as_str()
                        .is_some_and(|label| label.split(' ').next() == Some(*keyword))
                }),
                "statement completion is missing a form for keyword `{keyword}`"
            );
        }
    }

    #[test]
    fn completion_value_position_lists_bindings_and_inputs_not_parameters() {
        let registry = fixture_registry();
        let manifests = registry.manifests().unwrap();
        let source = "accept shout as text\nrun text.upper with text greeting as upper\nrun text.upper with text ";
        let result = complete_source(source, 2, 25, &registry, &manifests);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "upper"),
            "expected the earlier binding, got {items:?}"
        );
        assert!(
            items.iter().any(|item| item["label"] == "shout"),
            "expected the declared input, got {items:?}"
        );
        assert!(
            !items.iter().any(|item| item["kind"] == 5),
            "value positions must not offer parameter names, got {items:?}"
        );
    }

    #[test]
    fn completion_condition_lists_one_of_choices() {
        let registry = Registry::default();
        let source = "accept mode as text one of [\"a\", \"b\"]\nif mode is ";
        let result = complete_source(source, 1, 11, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "\"a\""),
            "expected choice `\"a\"`, got {items:?}"
        );
        assert!(
            items.iter().any(|item| item["label"] == "\"b\""),
            "expected choice `\"b\"`, got {items:?}"
        );
    }

    #[test]
    fn completion_take_position_lists_value_types() {
        let registry = Registry::default();
        let result = complete_source("take ", 0, 5, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        for required in ["text", "integer", "boolean", "json", "list of text"] {
            assert!(
                items.iter().any(|item| item["label"] == required),
                "take completion is missing `{required}`"
            );
        }
    }

    #[test]
    fn completion_after_invocation_lists_suffixes() {
        let registry = fixture_registry();
        let manifests = registry.manifests().unwrap();
        let source = "run text.upper with text hello ";
        let result = complete_source(source, 0, 31, &registry, &manifests);
        let items = result["items"].as_array().unwrap();
        for required in ["as", "and keep status as", "and retry up to"] {
            assert!(
                items.iter().any(|item| item["label"] == required),
                "suffix completion is missing `{required}`"
            );
        }
    }

    #[test]
    fn completion_parameter_value_lists_manifest_enum_values() {
        let manifest = serde_json::json!({
            "name": "enum.test",
            "description": "Choice applet.",
            "binary": "choose",
            "parameters": { "choice": { "type": "string", "required": true, "enum": ["left", "right"] } },
            "arguments": [{ "parameter": "choice" }]
        });
        let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
        let manifests = registry.manifests().unwrap();
        let source = "run enum.test with choice ";
        let result = complete_source(source, 0, 26, &registry, &manifests);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "\"left\""),
            "expected enum value `\"left\"`, got {items:?}"
        );
        assert!(
            items.iter().any(|item| item["label"] == "\"right\""),
            "expected enum value `\"right\"`, got {items:?}"
        );
        assert!(
            !items.iter().any(|item| item["kind"] == 5),
            "value positions must not offer parameter names, got {items:?}"
        );
    }

    #[test]
    fn completion_quoted_value_gets_suffixes() {
        let registry = fixture_registry();
        let manifests = registry.manifests().unwrap();
        let source = "run text.upper with text \"hello\" ";
        let result = complete_source(source, 0, 33, &registry, &manifests);
        let items = result["items"].as_array().unwrap();
        for required in ["as", "and keep status as", "and keep result as"] {
            assert!(
                items.iter().any(|item| item["label"] == required),
                "quoted-value completion is missing `{required}`, got {items:?}"
            );
        }
        assert!(
            !items.iter().any(|item| item["label"] == "and discard stderr"),
            "bare commands must not offer `and discard stderr`, got {items:?}"
        );
    }

    #[test]
    fn completion_take_follows_take_grammar_without_suffixes() {
        let registry = Registry::default();
        let result = complete_source("take text ", 0, 10, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "at"),
            "take type completion is missing `at`, got {items:?}"
        );
        assert!(
            !items.iter().any(|item| item["label"] == "integer"),
            "take type position must not re-offer types, got {items:?}"
        );
        let result = complete_source("take text at \"/p\" ", 0, 18, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "from"),
            "take pointer completion is missing `from`, got {items:?}"
        );
        let result = complete_source("take text at \"/p\" from src ", 0, 27, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "as"),
            "take source completion is missing `as`, got {items:?}"
        );
        let result =
            complete_source("take text at \"/p\" from src as n ", 0, 32, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.is_empty(),
            "complete take must take no suffixes, got {items:?}"
        );
    }

    #[test]
    fn completion_complete_statements_take_no_suffixes() {
        let registry = Registry::default();
        for source in [
            "spawn as job ",
            "parallel at most 4 as results ",
            "with temporary file as scratch ",
            "then parse json as data ",
            "then encode x as json ",
            "exit with status 0 ",
            "fail with message \"m\" and status 1 ",
        ] {
            let character = source.chars().count() as u64;
            let result = complete_source(source, 0, character, &registry, &[]);
            let items = result["items"].as_array().unwrap();
            assert!(
                items.is_empty(),
                "complete `{source}` must complete nothing, got {items:?}"
            );
        }
    }

    #[test]
    fn completion_fail_message_offers_status_once() {
        let registry = Registry::default();
        let source = "fail with message \"m\" ";
        let result = complete_source(source, 0, 22, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "and status"),
            "fail message completion is missing `and status`, got {items:?}"
        );
    }

    #[test]
    fn completion_complete_paths_offer_only_as_or_nothing() {
        let registry = Registry::default();
        let result = complete_source("read \"f\" ", 0, 9, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert_eq!(
            items.len(),
            1,
            "complete read path must offer only `as`, got {items:?}"
        );
        assert_eq!(items[0]["label"], "as");
        let result = complete_source("use folder \"w\" ", 0, 15, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.is_empty(),
            "complete use folder path must complete nothing, got {items:?}"
        );
    }

    #[test]
    fn completion_block_tails_offer_follow_ups() {
        let registry = Registry::default();
        let result = complete_source("for each item ", 0, 14, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "in"),
            "for each binding completion is missing `in`, got {items:?}"
        );
        let result = complete_source("parallel ", 0, 9, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        for required in ["at most", "for each"] {
            assert!(
                items.iter().any(|item| item["label"] == required),
                "parallel completion is missing `{required}`, got {items:?}"
            );
        }
        let result = complete_source("parallel at most 4 ", 0, 19, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "as"),
            "parallel limit completion is missing `as`, got {items:?}"
        );
    }

    #[test]
    fn completion_statement_start_filters_by_block_scope() {
        let registry = Registry::default();
        // Top level hides every scoped form: a no-op filter would still pass
        // the positive cases below, so the negatives are the real gate.
        let result = complete_source("", 0, 0, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        for hidden in [
            "break",
            "continue",
            "return",
            "catch",
            "finally",
            "otherwise",
            "otherwise if",
            "whenever a line arrives as",
        ] {
            assert!(
                !items.iter().any(|item| item["label"] == hidden),
                "top-level completion must not offer `{hidden}`, got {items:?}"
            );
        }
        let result = complete_source("for each x in [\"a\"]\n  ", 1, 2, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "break"),
            "loop body completion is missing `break`, got {items:?}"
        );
        for hidden in [
            "return",
            "catch",
            "finally",
            "otherwise",
            "whenever a line arrives as",
        ] {
            assert!(
                !items.iter().any(|item| item["label"] == hidden),
                "loop body completion must not offer `{hidden}`, got {items:?}"
            );
        }
        let result = complete_source("define a program named p\n  ", 1, 2, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "return"),
            "reusable program completion is missing `return`, got {items:?}"
        );
        for hidden in ["break", "catch", "otherwise", "whenever a line arrives as"] {
            assert!(
                !items.iter().any(|item| item["label"] == hidden),
                "reusable body completion must not offer `{hidden}`, got {items:?}"
            );
        }
        let result = complete_source("try\n  run x\n", 2, 0, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        for required in ["catch", "finally"] {
            assert!(
                items.iter().any(|item| item["label"] == required),
                "try tail completion is missing `{required}`, got {items:?}"
            );
        }
        for hidden in ["break", "return", "otherwise", "otherwise if"] {
            assert!(
                !items.iter().any(|item| item["label"] == hidden),
                "try tail completion must not offer `{hidden}`, got {items:?}"
            );
        }
        let result = complete_source("if mode is x\n  run y\n", 2, 0, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        for required in ["otherwise", "otherwise if"] {
            assert!(
                items.iter().any(|item| item["label"] == required),
                "if tail completion is missing `{required}`, got {items:?}"
            );
        }
        for hidden in ["break", "return", "catch", "finally"] {
            assert!(
                !items.iter().any(|item| item["label"] == hidden),
                "if tail completion must not offer `{hidden}`, got {items:?}"
            );
        }
        let result = complete_source("listen to stdin\n  ", 1, 2, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items
                .iter()
                .any(|item| item["label"] == "whenever a line arrives as"),
            "listen body completion is missing `whenever`, got {items:?}"
        );
    }

    #[test]
    fn completion_bare_partial_value_lists_bindings() {
        let registry = fixture_registry();
        let manifests = registry.manifests().unwrap();
        let source = "accept shout as text\nrun text.upper with text greeting as upper\nrun text.upper with text upp";
        let result = complete_source(source, 2, 28, &registry, &manifests);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "upper"),
            "bare partial value must offer the `upper` binding, got {items:?}"
        );
    }

    #[test]
    fn completion_use_bytes_binds_nothing() {
        let registry = Registry::default();
        let result = complete_source("use bytes ", 0, 10, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.is_empty(),
            "fresh `use bytes` binding must complete nothing, got {items:?}"
        );
        // A partial binding name must also complete nothing, even when an
        // in-scope binding matches the prefix (a value flood would survive
        // the client-side prefix filter here).
        let source = "run text.upper with text x as payload\nuse bytes pay";
        let result = complete_source(source, 1, 13, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.is_empty(),
            "partial `use bytes` binding must complete nothing, got {items:?}"
        );
    }

    #[test]
    fn completion_quoted_job_reference_offers_follow_ups() {
        let registry = Registry::default();
        let source = "spawn as myjob\n  run x\nawait \"myjob\" ";
        let character = "await \"myjob\" ".chars().count() as u64;
        let result = complete_source(source, 2, character, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        for required in ["and keep status as", "and keep result as"] {
            assert!(
                items.iter().any(|item| item["label"] == required),
                "quoted await must offer `{required}`, got {items:?}"
            );
        }
        assert!(
            !items.iter().any(|item| item["label"] == "myjob"),
            "quoted await must not re-offer the job list, got {items:?}"
        );
        let source = "spawn as myjob\n  run x\ninspect \"myjob\" ";
        let character = "inspect \"myjob\" ".chars().count() as u64;
        let result = complete_source(source, 2, character, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "as"),
            "quoted inspect must offer `as`, got {items:?}"
        );
        let source = "spawn as myjob\n  run x\ncancel \"myjob\" ";
        let character = "cancel \"myjob\" ".chars().count() as u64;
        let result = complete_source(source, 2, character, &registry, &[]);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.is_empty(),
            "quoted cancel must complete nothing, got {items:?}"
        );
    }

    #[test]
    fn completion_interactively_only_for_plain_top_level_run() {
        let registry = fixture_registry();
        let manifests = registry.manifests().unwrap();
        let source = "run text.upper with text hello ";
        let result = complete_source(source, 0, 31, &registry, &manifests);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "interactively"),
            "plain top-level run must offer `interactively`, got {items:?}"
        );
        // Bare applet phrases never take `interactively` (`parse_interactive_run`
        // requires a leading `run`).
        let source = "text.upper with text hello ";
        let character = source.chars().count() as u64;
        let result = complete_source(source, 0, character, &registry, &manifests);
        let items = result["items"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "as"),
            "bare invocation must still offer suffixes, got {items:?}"
        );
        assert!(
            !items.iter().any(|item| item["label"] == "interactively"),
            "bare invocation must not offer `interactively`, got {items:?}"
        );
        // Redirection, stdin, environment and retry combinations reject it.
        for source in [
            "run text.upper with text hello and stdin from \"f\" ",
            "run text.upper with text hello and stderr to \"e\" ",
            "run text.upper with text hello and environment {\"A\": \"b\"} ",
            "run text.upper with text hello and retry up to 3 times ",
        ] {
            let character = source.chars().count() as u64;
            let result = complete_source(source, 0, character, &registry, &manifests);
            let items = result["items"].as_array().unwrap();
            assert!(
                !items.is_empty(),
                "suffix combination must still offer suffixes for `{source}`, got {items:?}"
            );
            assert!(
                !items.iter().any(|item| item["label"] == "interactively"),
                "suffix combination must not offer `interactively` for `{source}`, got {items:?}"
            );
        }
    }
}
