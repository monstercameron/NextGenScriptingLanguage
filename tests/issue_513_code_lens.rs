#![cfg(feature = "lsp")]

use flow::language_server::run_with_io;
use flow::manifest::Registry;
use serde_json::{json, Value as JsonValue};
use std::fs;
use std::path::Path;

/// The bundled applet manifests, so the explain plan knows what each step writes or
/// runs. Written into `directory` the way the other registry tests do it.
fn bundled_registry(directory: &Path) -> Registry {
    let index = flow::bundled_manifests::index().expect("bundled index");
    for (position, entry) in index.entries().iter().enumerate() {
        let body = index.body(position).expect("bundled manifest body");
        fs::write(directory.join(entry.file()), body).expect("write bundled manifest");
    }
    Registry::from_bundled(directory).expect("bundled registry")
}

fn request(id: u64, method: &str, params: JsonValue) -> JsonValue {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn frame(message: JsonValue) -> Vec<u8> {
    let body = serde_json::to_vec(&message).unwrap();
    let mut result = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    result.extend_from_slice(&body);
    result
}

fn decode_frames(mut bytes: &[u8]) -> Vec<JsonValue> {
    let mut messages = Vec::new();
    while !bytes.is_empty() {
        let header_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("frame header");
        let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let length: usize = header
            .strip_prefix("Content-Length: ")
            .expect("content length header")
            .trim()
            .parse()
            .unwrap();
        let body_start = header_end + 4;
        let body = &bytes[body_start..body_start + length];
        messages.push(serde_json::from_slice(body).unwrap());
        bytes = &bytes[body_start + length..];
    }
    messages
}

/// Runs the stdio server over one open document and returns the initialize result
/// and the code lenses answered for the document.
fn lens_response(uri: &str, source: &str) -> (JsonValue, JsonValue) {
    let mut input = frame(request(1, "initialize", JsonValue::Null));
    input.extend(frame(json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": { "textDocument": { "uri": uri, "languageId": "execute-program", "version": 1, "text": source } }
    })));
    input.extend(frame(request(
        2,
        "textDocument/codeLens",
        json!({ "textDocument": { "uri": uri } }),
    )));
    input.extend(frame(request(3, "shutdown", JsonValue::Null)));
    input.extend(frame(json!({ "jsonrpc": "2.0", "method": "exit" })));

    let directory = tempfile::tempdir().unwrap();
    let registry = bundled_registry(directory.path());
    let mut output = Vec::new();
    run_with_io(input.as_slice(), &mut output, registry).unwrap();
    let messages = decode_frames(&output);
    let initialize = messages
        .iter()
        .find(|message| message["id"] == 1)
        .expect("initialize response")["result"]
        .clone();
    let lenses = messages
        .iter()
        .find(|message| message["id"] == 2)
        .expect("codeLens response")["result"]
        .clone();
    (initialize, lenses)
}

fn lens_title_at(lenses: &JsonValue, line: u64) -> Option<String> {
    lenses
        .as_array()?
        .iter()
        .find(|lens| lens["range"]["start"]["line"] == line)
        .and_then(|lens| lens["command"]["title"].as_str().map(str::to_owned))
}

#[test]
fn code_lens_marks_write_and_process_steps_of_the_theme_demo() {
    let source = include_str!("../examples/demo/theme-refresh.program");
    let (initialize, lenses) = lens_response("file:///tmp/issue_513_theme.program", source);
    assert!(
        initialize["capabilities"]["codeLensProvider"].is_object(),
        "{initialize}"
    );

    // Lines are 1-based in the source and 0-based in LSP ranges.
    // Line 22 is `run fs.ensure_symlink`, line 23 is `run "hyprctl"`.
    let symlink = lens_title_at(&lenses, 21).expect("lens on the symlink write step");
    assert!(symlink.contains("writes"), "{lenses}");
    let reload = lens_title_at(&lenses, 22).expect("lens on the process step");
    assert!(reload.contains("process"), "{lenses}");
    // Line 4 is a comment and must not get a lens.
    assert_eq!(lens_title_at(&lenses, 3), None, "{lenses}");
}
