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

/// Runs the stdio server over one open document and returns the folding ranges
/// answered for it, plus the initialize result.
fn folding_response(source: &str) -> (JsonValue, JsonValue) {
    let uri = "file:///tmp/issue_484_folds.program";
    let mut input = frame(request(1, "initialize", JsonValue::Null));
    input.extend(frame(json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": { "textDocument": { "uri": uri, "languageId": "execute-program", "version": 1, "text": source } }
    })));
    input.extend(frame(request(
        2,
        "textDocument/foldingRange",
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
        .expect("initialize response")
        .clone();
    let folds = messages
        .iter()
        .find(|message| message["id"] == 2)
        .expect("foldingRange response")["result"]
        .clone();
    (initialize, folds)
}

#[test]
fn folding_range_covers_if_loop_and_try_bodies_to_their_last_body_line() {
    // Lines are 1-based in the source and 0-based in LSP ranges.
    let source = "use value \"a\"\n\
                  if true is true\n\
                  \x20 use value \"b\"\n\
                  \x20 run \"true\"\n\
                  for each item in [\"x\", \"y\"]\n\
                  \x20 run \"true\"\n\
                  try\n\
                  \x20 run \"true\"\n\
                  catch failure\n\
                  \x20 run \"true\"\n";
    let (initialize, folds) = folding_response(source);
    assert_eq!(
        initialize["result"]["capabilities"]["foldingRangeProvider"],
        true
    );
    assert_eq!(
        folds,
        json!([
            { "startLine": 1, "endLine": 3 },
            { "startLine": 4, "endLine": 5 },
            { "startLine": 6, "endLine": 9 }
        ])
    );
}

#[test]
fn folding_range_covers_if_bodies_inside_reusable_definitions() {
    let source = "define a program named emit\n\
                  \x20 if true is true\n\
                  \x20   run \"true\"\n\
                  run program emit\n";
    let (_, folds) = folding_response(source);
    assert_eq!(folds, json!([{ "startLine": 1, "endLine": 2 }]));
}

#[test]
fn folding_range_is_empty_for_a_document_without_blocks() {
    let (_, folds) = folding_response("use value \"a\"\nrun \"true\"\n");
    assert_eq!(folds, json!([]));
}
