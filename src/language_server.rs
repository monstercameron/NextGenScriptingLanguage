//! A small stdio Language Server Protocol implementation for `.program` files.
//!
//! The server implements incremental document sync, diagnostics, completion,
//! hover, reusable-program definition/references, and validated rename edits.
//! It uses JSON-RPC framing from the LSP specification and writes no logs to
//! stdout, keeping the channel safe for editor clients.

use crate::diagnostics::{analyze_source, Diagnostic};
use crate::manifest::Registry;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

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
            "initialize" => Some(json!({
                "capabilities": {
                    "textDocumentSync": 2,
                    "positionEncoding": "utf-16",
                    "completionProvider": { "triggerCharacters": [" ", "."] },
                    "hoverProvider": true,
                    "definitionProvider": true,
                    "referencesProvider": true,
                    "renameProvider": { "prepareProvider": true },
                    "codeActionProvider": { "codeActionKinds": ["quickfix"] }
                },
                "serverInfo": { "name": "execute-language-server", "version": env!("CARGO_PKG_VERSION") }
            })),
            "initialized" | "$/cancelRequest" => None,
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
                            },
                        );
                        publish_diagnostics(
                            &mut writer,
                            uri,
                            documents.get(uri).unwrap(),
                            &registry,
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
                                    publish_diagnostics(&mut writer, uri, document, &registry)?;
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
                }
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

fn publish_diagnostics<W: Write>(
    writer: &mut W,
    uri: &str,
    document: &Document,
    registry: &Registry,
) -> Result<()> {
    let positions = SourcePositions::new(&document.text);
    let diagnostics = analyze_source(&document.text, registry)
        .into_iter()
        .map(|diagnostic| lsp_diagnostic(&document.text, &positions, diagnostic))
        .collect::<Vec<_>>();
    let mut params = json!({ "uri": uri, "diagnostics": diagnostics });
    if let Some(version) = document.version {
        params["version"] = version.into();
    }
    write_notification(writer, "textDocument/publishDiagnostics", params)
}

fn lsp_diagnostic(source: &str, positions: &SourcePositions, diagnostic: Diagnostic) -> JsonValue {
    json!({
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
    })
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
    let words = lexical_words(prefix);
    let trimmed = prefix.trim_start();
    let parameter_context = trimmed.contains(" with ") || trimmed.contains(" and ");
    let command_context = trimmed.starts_with("run ")
        || trimmed.starts_with("then ")
        || trimmed.starts_with("pipe to ");

    let mut items = Vec::<JsonValue>::new();
    if parameter_context {
        let operation = operation_prefix(trimmed);
        let manifests = operation
            .and_then(|name| registry.resolve(name).ok())
            .into_iter()
            .collect::<Vec<_>>();
        for manifest in manifests {
            let contract = manifest.contract();
            let properties = contract
                .pointer("/parameters_schema/properties")
                .and_then(JsonValue::as_object);
            let required = contract
                .pointer("/parameters_schema/required")
                .and_then(JsonValue::as_array);
            if let Some(properties) = properties {
                for (name, schema) in properties {
                    let is_required = required.is_some_and(|values| {
                        values.iter().any(|value| value.as_str() == Some(name))
                    });
                    let detail = schema
                        .get("type")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("value");
                    items.push(json!({
                        "label": name,
                        "kind": 5,
                        "detail": format!("{} parameter{}", if is_required { "required" } else { "optional" }, format!(" · {detail}")),
                        "documentation": { "kind": "markdown", "value": format!("Applet `{}` parameter schema:\n\n```json\n{}\n```", manifest.name, serde_json::to_string_pretty(schema).unwrap_or_default()) },
                        "insertText": name
                    }));
                }
            }
        }
    } else if command_context {
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
                "filterText": format!("{} {} {}", manifest.name, manifest.contract()["phrases"].as_array().into_iter().flatten().filter_map(JsonValue::as_str).collect::<Vec<_>>().join(" "), description)
            }));
        }
        if let Ok(program) = crate::parser::parse(&document.text) {
            for name in program.definitions.keys() {
                items.push(json!({ "label": name, "kind": 3, "detail": "Reusable program in this file", "insertText": name }));
            }
        }
    } else {
        for keyword in [
            "accept",
            "define a program named",
            "run",
            "then",
            "pipe to",
            "read from stdin",
            "then parse json as",
            "take",
            "if",
            "otherwise",
            "repeat",
            "for each",
            "while",
            "parallel",
            "race",
            "spawn as",
            "inspect",
            "await",
            "cancel",
            "listen to",
            "every",
            "on schedule",
        ] {
            items.push(json!({ "label": keyword, "kind": 14, "detail": "Execute language form", "insertText": keyword }));
        }
    }

    let prefix_word = if prefix.chars().last().is_some_and(char::is_whitespace) {
        ""
    } else {
        words
            .last()
            .map(|word| &prefix[word.text_start..word.text_end])
            .unwrap_or("")
    };
    if !prefix_word.is_empty() {
        items.retain(|item| {
            item.get("filterText")
                .or_else(|| item.get("label"))
                .and_then(JsonValue::as_str)
                .is_some_and(|label| {
                    label
                        .to_ascii_lowercase()
                        .contains(&prefix_word.to_ascii_lowercase())
                })
        });
    }
    json!({ "isIncomplete": false, "items": items })
}

fn operation_prefix(line_prefix: &str) -> Option<&str> {
    let command_start = line_prefix
        .strip_prefix("pipe to ")
        .or_else(|| line_prefix.strip_prefix("run "))
        .or_else(|| line_prefix.strip_prefix("then "))?;
    let operation = command_start
        .split(" with ")
        .next()?
        .split(" and ")
        .next()?
        .trim();
    (!operation.is_empty()).then_some(operation)
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
}
