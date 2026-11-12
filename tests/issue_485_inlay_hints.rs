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

/// Runs the stdio server over one open document, asks for its inlay hints over the
/// whole document, and returns the hints answered for it.
fn inlay_hint_response(source: &str, initialization_options: Option<JsonValue>) -> JsonValue {
    let uri = "file:///tmp/issue_485_hints.program";
    let mut initialize = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    if let Some(options) = initialization_options {
        initialize["params"]["initializationOptions"] = options;
    }
    let mut input = frame(initialize);
    input.extend(frame(json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": { "textDocument": { "uri": uri, "languageId": "execute-program", "version": 1, "text": source } }
    })));
    input.extend(frame(request(
        2,
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 100, "character": 0 } }
        }),
    )));
    input.extend(frame(request(3, "shutdown", JsonValue::Null)));
    input.extend(frame(json!({ "jsonrpc": "2.0", "method": "exit" })));

    let mut output = Vec::new();
    run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
    let messages = decode_frames(&output);
    messages
        .iter()
        .find(|message| message["id"] == 2)
        .expect("inlay hint response")["result"]
        .clone()
}

const PROGRAM: &str =
    "run \"false\" and keep status as code\nrun \"printf\" with arguments [\"x\"] as message\n";

#[test]
fn keep_status_binding_gets_an_integer_hint_after_its_name_when_the_client_opts_in() {
    let hints = inlay_hint_response(PROGRAM, Some(json!({ "inlayHints": true })));
    let hints = hints.as_array().expect("hint array");
    assert_eq!(
        hints.len(),
        1,
        "only the keep status binding is hinted: {hints:?}"
    );
    let hint = &hints[0];
    assert_eq!(hint["label"], "integer");
    assert_eq!(hint["position"]["line"], 0);
    // The line `run "false" and keep status as code` ends after `code`, at column 35.
    assert_eq!(hint["position"]["character"], 35);
}

#[test]
fn no_hints_are_returned_when_the_client_did_not_opt_in() {
    let hints = inlay_hint_response(PROGRAM, None);
    assert_eq!(hints, json!([]));
    let hints = inlay_hint_response(PROGRAM, Some(json!({ "inlayHints": false })));
    assert_eq!(hints, json!([]));
}

#[test]
fn results_without_a_known_static_type_get_no_hint() {
    let source = "run \"printf\" with arguments [\"x\"] as message\n";
    let hints = inlay_hint_response(source, Some(json!({ "inlayHints": true })));
    assert_eq!(hints, json!([]));
}
