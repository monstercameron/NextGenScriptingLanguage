#![cfg(feature = "lsp")]

use flow::language_server::run_with_io;
use flow::manifest::Registry;
use serde_json::{json, Value as JsonValue};

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

/// Opens `source` as the document at `uri` and returns the document links the server
/// answers for it.
fn document_links_response(uri: &str, source: &str) -> JsonValue {
    let mut input = frame(request(1, "initialize", JsonValue::Null));
    input.extend(frame(json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": { "textDocument": { "uri": uri, "languageId": "execute-program", "version": 1, "text": source } }
    })));
    input.extend(frame(request(
        2,
        "textDocument/documentLink",
        json!({ "textDocument": { "uri": uri } }),
    )));
    input.extend(frame(request(3, "shutdown", JsonValue::Null)));
    input.extend(frame(json!({ "jsonrpc": "2.0", "method": "exit" })));

    let mut output = Vec::new();
    run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
    let messages = decode_frames(&output);
    let initialize = messages
        .iter()
        .find(|message| message["id"] == 1)
        .expect("initialize response");
    assert!(initialize["result"]["capabilities"]["documentLinkProvider"].is_object());
    messages
        .into_iter()
        .find(|message| message["id"] == 2)
        .expect("documentLink response")
}

#[test]
fn document_link_points_run_program_file_paths_at_existing_files() {
    let directory = tempfile::tempdir().unwrap();
    let child = directory.path().join("child.program");
    std::fs::write(&child, "print \"child\"\n").unwrap();
    let main = directory.path().join("main.program");
    let main_uri = url::Url::from_file_path(&main).unwrap();
    let source = "run program file \"child.program\"\n\
                  then program file \"missing.program\"\n\
                  run program file \"sub/child.program\"\n";

    let response = document_links_response(main_uri.as_str(), source);
    let links = response["result"].as_array().expect("link array");

    // Only the path that exists produces a link, covering the text between the quotes.
    assert_eq!(links.len(), 1, "links: {links:?}");
    assert_eq!(
        links[0]["range"]["start"],
        json!({ "line": 0, "character": 18 })
    );
    assert_eq!(
        links[0]["range"]["end"],
        json!({ "line": 0, "character": 31 })
    );
    let child_uri = url::Url::from_file_path(&child).unwrap();
    assert_eq!(links[0]["target"], json!(child_uri.as_str()));
}

#[test]
fn document_link_is_empty_for_an_untitled_document() {
    let source = "run program file \"child.program\"\n";
    let response = document_links_response("untitled:Untitled-1", source);
    assert_eq!(response["result"], json!([]));
}
