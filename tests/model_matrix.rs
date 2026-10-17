//! Table-driven model adapter tests: argument validation, fixture contracts,
//! endpoint and credential policy, request shapes, and provider normalization.
//!
//! Every case is hermetic. Validation cases run with `EXECUTE_MODELS_OFFLINE`
//! set, so a valid argument set reports `offline` and an invalid one reports
//! `configuration` before any socket opens. Fixture cases never reach a network.
//! Live-shaped cases talk only to an owned 127.0.0.1 server. No test reads the
//! repository `.env`: each run uses a temporary working directory and removes
//! inherited provider credentials and proxy variables first.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HELPER: &str = env!("CARGO_BIN_EXE_execute-model");
const CREDENTIAL_VARS: [&str; 5] = [
    "OPENAI_API_KEY",
    "openai",
    "JEV_API_KEY",
    "TYPESAFE_API_KEY",
    "jev",
];
const PROXY_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// Run the helper in `dir` with a clean credential and proxy environment.
/// `offline` sets `EXECUTE_MODELS_OFFLINE`; `envs` adds explicit variables.
fn run(dir: &Path, args: &[String], stdin: &[u8], offline: bool, envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(HELPER);
    command
        .args(args)
        .current_dir(dir)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in CREDENTIAL_VARS.iter().chain(PROXY_VARS.iter()) {
        command.env_remove(name);
    }
    if offline {
        command.env("EXECUTE_MODELS_OFFLINE", "1");
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("start execute-model");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin)
        .expect("write state");
    child.wait_with_output().expect("wait for execute-model")
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn stderr_line(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

/// Assert a clean static failure: no stdout, one diagnostic line, and the class.
fn assert_failure_class(output: &Output, class: &str, context: &str) {
    assert!(
        !output.status.success(),
        "{context}: unexpectedly succeeded"
    );
    assert!(output.stdout.is_empty(), "{context}: wrote stdout");
    assert_eq!(
        stderr_line(output),
        format!("execute-model: {class}"),
        "{context}"
    );
}

fn json_output(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "unexpected failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).expect("helper emits JSON")
}

fn write_fixture(dir: &Path, name: &str, expected: Value, response: Value) -> String {
    let path = dir.join(name);
    std::fs::write(
        &path,
        json!({ "expected_request": expected, "response": response }).to_string(),
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

/// Base OpenAI generate arguments; callers append options.
fn generate(extra: &[&str]) -> Vec<String> {
    let mut args = argv(&[
        "generate",
        "--provider",
        "openai",
        "--model",
        "gpt-6-luna",
        "--instruction",
        "Draft one line",
    ]);
    args.extend(extra.iter().map(|part| (*part).to_owned()));
    args
}

fn extract(schema: &str, extra: &[&str]) -> Vec<String> {
    let mut args = argv(&[
        "extract",
        "--provider",
        "openai",
        "--model",
        "gpt-6-luna",
        "--instruction",
        "Extract the count",
        "--schema",
        schema,
    ]);
    args.extend(extra.iter().map(|part| (*part).to_owned()));
    args
}

const COUNT_SCHEMA: &str = r#"{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false}"#;

fn responses_text(text: &str) -> Value {
    json!({
        "model": "gpt-fixture",
        "status": "completed",
        "output": [{
            "type": "message", "role": "assistant", "status": "completed",
            "content": [{ "type": "output_text", "text": text }]
        }],
        "usage": { "input_tokens": 2, "output_tokens": 1, "total_tokens": 3 }
    })
}

fn chat_completion(content: Value) -> Value {
    json!({
        "model": "local-fixture",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6 }
    })
}

/// Semantic request for a default `generate` call (no optional fields).
fn generate_semantic(state: &str) -> Value {
    json!({
        "provider": "openai", "kind": "generate", "model": "gpt-6-luna",
        "state": state, "instruction": "Draft one line"
    })
}

// ---------------------------------------------------------------------------
// Owned loopback server with request capture
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Captured {
    path: String,
    headers: String,
    body: Vec<u8>,
}

struct Reply {
    status: u16,
    extra_headers: String,
    body: Vec<u8>,
}

fn reply(status: u16, body: &Value) -> Reply {
    Reply {
        status,
        extra_headers: String::new(),
        body: body.to_string().into_bytes(),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn read_request(stream: &mut TcpStream) -> Option<Captured> {
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = find(&bytes, b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 || bytes.len() > 1 << 20 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let end = (header_end + length).min(bytes.len());
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_owned();
    Some(Captured {
        path,
        headers: head.to_ascii_lowercase(),
        body: bytes[header_end..end].to_vec(),
    })
}

/// Serve one reply per expected request on 127.0.0.1. Returns the base URL
/// (`http://127.0.0.1:port`) and a join handle yielding what was received.
fn serve(replies: Vec<Reply>) -> (String, thread::JoinHandle<Vec<Captured>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind owned loopback listener");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut captured = Vec::new();
        for reply in replies {
            let accepted = loop {
                match listener.accept() {
                    Ok((stream, _)) => break Some(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            break None;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break None,
                }
            };
            let Some(mut stream) = accepted else { break };
            stream.set_nonblocking(false).unwrap();
            if let Some(request) = read_request(&mut stream) {
                captured.push(request);
            }
            let head = format!(
                "HTTP/1.1 {} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                reply.status,
                reply.body.len(),
                reply.extra_headers
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&reply.body);
        }
        captured
    });
    (base, handle)
}

fn chat_base(base: &str) -> String {
    format!("{base}/v1")
}

// ---------------------------------------------------------------------------
// 1. Argument validation: invalid sets fail as configuration before any I/O.
// ---------------------------------------------------------------------------

#[test]
fn invalid_argument_sets_fail_as_configuration_before_any_request() {
    let directory = tempfile::tempdir().unwrap();
    let long_labels: Vec<String> = (0..129).map(|index| format!("l{index}")).collect();
    let too_many_questions: Value = Value::Object(
        (0..33)
            .map(|index| {
                (
                    format!("q{index}"),
                    json!({"type":"noul","question":"Is it true?"}),
                )
            })
            .collect(),
    );
    let cases: Vec<(&str, Vec<String>)> = vec![
        ("unknown api", generate(&["--api", "anthropic"])),
        (
            "api on jev",
            argv(&["judge", "--question", "Ok?", "--api", "responses"]),
        ),
        (
            "duplicate api",
            generate(&["--api", "responses", "--api", "responses"]),
        ),
        ("effort minimal", generate(&["--effort", "minimal"])),
        ("effort uppercase", generate(&["--effort", "HIGH"])),
        ("effort empty", generate(&["--effort", ""])),
        (
            "effort on jev",
            argv(&["judge", "--question", "Ok?", "--effort", "low"]),
        ),
        (
            "duplicate effort",
            generate(&["--effort", "low", "--effort", "low"]),
        ),
        ("temperature above two", generate(&["--temperature", "2.5"])),
        ("temperature negative", generate(&["--temperature", "-0.1"])),
        ("temperature text", generate(&["--temperature", "warm"])),
        (
            "temperature quoted number",
            generate(&["--temperature", "\"0.5\""]),
        ),
        ("temperature boolean", generate(&["--temperature", "true"])),
        (
            "temperature overflow",
            generate(&["--temperature", "1e400"]),
        ),
        (
            "temperature on jev",
            argv(&["judge", "--question", "Ok?", "--temperature", "1"]),
        ),
        (
            "duplicate temperature",
            generate(&["--temperature", "0.2", "--temperature", "0.3"]),
        ),
        ("option without value", generate(&["--effort"])),
        ("unknown option", generate(&["--tone", "warm"])),
        (
            "openai without model",
            argv(&["generate", "--provider", "openai", "--instruction", "x"]),
        ),
        ("max tokens zero", generate(&["--max-tokens", "0"])),
        ("max tokens over cap", generate(&["--max-tokens", "65537"])),
        ("timeout zero", generate(&["--timeout-ms", "0"])),
        ("max bytes zero", generate(&["--max-bytes", "0"])),
        (
            "classify labels and criteria",
            argv(&[
                "classify",
                "--question",
                "Route?",
                "--labels",
                "a",
                "b",
                "--criteria",
                r#"{"a":"x","b":"y"}"#,
            ]),
        ),
        (
            "classify criteria with one entry",
            argv(&[
                "classify",
                "--question",
                "Route?",
                "--criteria",
                r#"{"a":"x"}"#,
            ]),
        ),
        (
            "classify criteria not an object",
            argv(&[
                "classify",
                "--question",
                "Route?",
                "--criteria",
                r#"["a","b"]"#,
            ]),
        ),
        (
            "classify criteria empty description",
            argv(&[
                "classify",
                "--question",
                "Route?",
                "--criteria",
                r#"{"a":"","b":"y"}"#,
            ]),
        ),
        (
            "classify criteria label starts with dashes",
            argv(&[
                "classify",
                "--question",
                "Route?",
                "--criteria",
                r#"{"--a":"x","b":"y"}"#,
            ]),
        ),
        (
            "classify criteria duplicate key",
            argv(&[
                "classify",
                "--question",
                "Route?",
                "--criteria",
                r#"{"a":"x","a":"y"}"#,
            ]),
        ),
        (
            "classify without labels or criteria",
            argv(&["classify", "--question", "Route?"]),
        ),
        (
            "criteria on judge",
            argv(&[
                "judge",
                "--question",
                "Ok?",
                "--criteria",
                r#"{"a":"x","b":"y"}"#,
            ]),
        ),
        (
            "questions on classify",
            argv(&[
                "classify",
                "--question",
                "Route?",
                "--labels",
                "a",
                "b",
                "--questions",
                "{}",
            ]),
        ),
        (
            "ask with a top-level question",
            argv(&[
                "ask",
                "--questions",
                r#"{"a":{"type":"noul","question":"Ok?"}}"#,
                "--question",
                "x",
            ]),
        ),
        ("ask without questions", argv(&["ask"])),
        (
            "ask with empty questions",
            argv(&["ask", "--questions", "{}"]),
        ),
        (
            "ask with top-level minimum",
            argv(&[
                "ask",
                "--questions",
                r#"{"a":{"type":"noul","question":"Ok?"}}"#,
                "--minimum",
                "50",
            ]),
        ),
        (
            "ask with too many questions",
            argv(&["ask", "--questions", &too_many_questions.to_string()]),
        ),
        (
            "ask unknown question type",
            argv(&[
                "ask",
                "--questions",
                r#"{"a":{"type":"rank","question":"Ok?"}}"#,
            ]),
        ),
        (
            "ask choice with labels and criteria",
            argv(&[
                "ask",
                "--questions",
                r#"{"a":{"type":"choice","question":"Route?","labels":["x","y"],"criteria":{"x":"p","y":"q"}}}"#,
            ]),
        ),
        (
            "ask choice with one label",
            argv(&[
                "ask",
                "--questions",
                r#"{"a":{"type":"choice","question":"Route?","labels":["x"]}}"#,
            ]),
        ),
        (
            "ask noul with labels",
            argv(&[
                "ask",
                "--questions",
                r#"{"a":{"type":"noul","question":"Ok?","labels":["x","y"]}}"#,
            ]),
        ),
        (
            "ask score with criteria",
            argv(&[
                "ask",
                "--questions",
                r#"{"a":{"type":"score","question":"Rate?","criteria":{"x":"p"}}}"#,
            ]),
        ),
        (
            "ask question with unknown field",
            argv(&[
                "ask",
                "--questions",
                r#"{"a":{"type":"noul","question":"Ok?","tone":"warm"}}"#,
            ]),
        ),
        (
            "ask minimum above 100",
            argv(&[
                "ask",
                "--questions",
                r#"{"a":{"type":"noul","question":"Ok?","minimum":101}}"#,
            ]),
        ),
        (
            "ask name starts with dashes is still a JSON key",
            argv(&[
                "ask",
                "--questions",
                r#"{"":{"type":"noul","question":"Ok?"}}"#,
            ]),
        ),
        (
            "labels beginning with dashes",
            argv(&["classify", "--question", "Route?", "--labels", "--a", "b"]),
        ),
        ("long label list", {
            let mut args = argv(&["classify", "--question", "Route?", "--labels"]);
            args.extend(long_labels);
            args
        }),
    ];
    for (name, args) in cases {
        let output = run(directory.path(), &args, b"state", true, &[]);
        assert!(!output.status.success(), "{name}: accepted an invalid set");
        assert!(output.stdout.is_empty(), "{name}: wrote stdout");
        let line = stderr_line(&output);
        assert!(
            line.starts_with("execute-model: "),
            "{name}: unexpected diagnostic {line:?}"
        );
        assert_ne!(
            line, "execute-model: offline",
            "{name}: validation was skipped"
        );
    }
}

#[test]
fn valid_argument_sets_reach_the_offline_guard_without_network() {
    let directory = tempfile::tempdir().unwrap();
    let cases: Vec<Vec<String>> = vec![
        generate(&[]),
        generate(&["--api", "responses"]),
        generate(&[
            "--api",
            "chat_completions",
            "--endpoint",
            "http://127.0.0.1:9/v1",
        ]),
        generate(&["--effort", "none"]),
        generate(&["--effort", "xhigh"]),
        generate(&["--effort", "max", "--temperature", "0"]),
        generate(&["--temperature", "2"]),
        generate(&["--temperature", "1.5", "--max-tokens", "1"]),
        argv(&["classify", "--question", "Route?", "--labels", "a", "b"]),
        argv(&[
            "classify",
            "--question",
            "Route?",
            "--criteria",
            r#"{"billing":"invoices and refunds","account":"login help"}"#,
        ]),
        argv(&["judge", "--question", "Is it supported?"]),
        argv(&["score", "--question", "Rate it", "--levels", "low", "high"]),
        argv(&[
            "ask",
            "--questions",
            r#"{"route":{"type":"choice","question":"Route?","labels":["a","b"]},"ok":{"type":"noul","question":"Ok?","minimum":70},"rate":{"type":"score","question":"Rate?","levels":["low","high"]}}"#,
        ]),
    ];
    for args in cases {
        let output = run(directory.path(), &args, b"state", true, &[]);
        assert_failure_class(&output, "offline", &format!("{args:?}"));
    }
}

// ---------------------------------------------------------------------------
// 2. Fixture contract: exact semantic match, new fields only when used.
// ---------------------------------------------------------------------------

#[test]
fn default_requests_keep_the_original_semantic_shape_exactly() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = write_fixture(
        directory.path(),
        "default.json",
        generate_semantic("source"),
        responses_text("ok"),
    );
    let mut args = generate(&[]);
    args.extend(["--fixture".into(), fixture]);
    let result = json_output(&run(directory.path(), &args, b"source", false, &[]));
    assert_eq!(result["text"], "ok");
    assert_eq!(result["transport"], "fixture");
}

#[test]
fn optional_fields_must_appear_in_the_fixture_exactly_when_given() {
    let directory = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, Vec<String>, Value)> = vec![
        (
            "effort given, fixture omits it",
            generate(&["--effort", "high"]),
            generate_semantic("source"),
        ),
        (
            "effort absent, fixture has it",
            generate(&[]),
            json!({ "provider":"openai","kind":"generate","model":"gpt-6-luna","state":"source","instruction":"Draft one line","effort":"high" }),
        ),
        (
            "temperature given, fixture omits it",
            generate(&["--temperature", "0.5"]),
            generate_semantic("source"),
        ),
        (
            "chat api given, fixture omits it",
            generate(&[
                "--api",
                "chat_completions",
                "--endpoint",
                "http://127.0.0.1:9/v1",
            ]),
            generate_semantic("source"),
        ),
        (
            "responses api given explicitly, fixture must not name it",
            generate(&["--api", "responses"]),
            json!({ "provider":"openai","kind":"generate","model":"gpt-6-luna","state":"source","instruction":"Draft one line","api":"responses" }),
        ),
    ];
    for (name, extra, expected) in cases {
        let fixture = write_fixture(
            directory.path(),
            "case.json",
            expected,
            responses_text("ok"),
        );
        let mut args = extra;
        args.extend(["--fixture".into(), fixture]);
        let output = run(directory.path(), &args, b"source", false, &[]);
        assert_failure_class(&output, "fixture_mismatch", name);
    }
}

#[test]
fn optional_fields_match_when_the_fixture_describes_them() {
    let directory = tempfile::tempdir().unwrap();
    let mut expected = generate_semantic("source");
    expected["effort"] = json!("high");
    expected["temperature"] = json!(0.5);
    let fixture = write_fixture(
        directory.path(),
        "with-options.json",
        expected,
        responses_text("tuned"),
    );
    let mut args = generate(&["--effort", "high", "--temperature", "0.5"]);
    args.extend(["--fixture".into(), fixture]);
    let result = json_output(&run(directory.path(), &args, b"source", false, &[]));
    assert_eq!(result["text"], "tuned");
}

#[test]
fn integer_temperature_is_recorded_as_a_decimal_in_the_semantic_request() {
    // `--temperature 1` and `--temperature 1.0` share one canonical semantic
    // form, so fixtures must spell the value as a decimal such as 1.0.
    let directory = tempfile::tempdir().unwrap();
    let mut expected = generate_semantic("source");
    expected["temperature"] = json!(1.0);
    let fixture = write_fixture(
        directory.path(),
        "integer-temperature.json",
        expected,
        responses_text("ok"),
    );
    let mut args = generate(&["--temperature", "1"]);
    args.extend(["--fixture".into(), fixture]);
    let result = json_output(&run(directory.path(), &args, b"source", false, &[]));
    assert_eq!(result["text"], "ok");
}

#[test]
fn endpoint_is_transport_and_never_part_of_the_fixture_request() {
    let directory = tempfile::tempdir().unwrap();
    for endpoint in [
        "http://127.0.0.1:9/v1/responses",
        "http://127.0.0.1:10/v1",
        "http://localhost:11/anything",
    ] {
        let fixture = write_fixture(
            directory.path(),
            "endpoint.json",
            generate_semantic("source"),
            responses_text("ok"),
        );
        let mut args = generate(&["--endpoint", endpoint]);
        args.extend(["--fixture".into(), fixture]);
        let result = json_output(&run(directory.path(), &args, b"source", true, &[]));
        assert_eq!(result["transport"], "fixture", "{endpoint}");
    }
}

#[test]
fn fixture_present_but_empty_or_missing_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory
        .path()
        .join("missing.json")
        .to_string_lossy()
        .into_owned();
    let empty_file = directory.path().join("empty.json");
    std::fs::write(&empty_file, b"").unwrap();
    for fixture in [
        String::new(),
        missing,
        empty_file.to_string_lossy().into_owned(),
    ] {
        let mut args = generate(&[]);
        args.extend(["--fixture".into(), fixture.clone()]);
        let output = run(directory.path(), &args, b"source", false, &[]);
        assert!(!output.status.success(), "fixture {fixture:?} succeeded");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn fixture_response_must_be_a_single_json_object_with_only_the_two_keys() {
    let directory = tempfile::tempdir().unwrap();
    let bad_documents = [
        json!([1, 2]).to_string(),
        json!({"expected_request": generate_semantic("source")}).to_string(),
        json!({"expected_request": generate_semantic("source"), "response": responses_text("ok"), "extra": 1}).to_string(),
        json!({"expected_request": generate_semantic("source"), "response": "not an object"}).to_string(),
    ];
    for (index, document) in bad_documents.iter().enumerate() {
        let path = directory.path().join(format!("bad-{index}.json"));
        std::fs::write(&path, document).unwrap();
        let mut args = generate(&[]);
        args.extend(["--fixture".into(), path.to_string_lossy().into_owned()]);
        let output = run(directory.path(), &args, b"source", false, &[]);
        assert!(!output.status.success(), "bad fixture {index} accepted");
        assert!(output.stdout.is_empty());
    }
}

// ---------------------------------------------------------------------------
// 3. Chat Completions normalization through fixtures.
// ---------------------------------------------------------------------------

fn chat_generate_semantic(state: &str) -> Value {
    let mut expected = generate_semantic(state);
    expected["api"] = json!("chat_completions");
    expected
}

#[test]
fn chat_completions_generate_normalizes_content_and_usage() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = write_fixture(
        directory.path(),
        "chat.json",
        chat_generate_semantic("source"),
        chat_completion(json!("A short draft.")),
    );
    let mut args = generate(&[
        "--api",
        "chat_completions",
        "--endpoint",
        "http://127.0.0.1:9/v1",
    ]);
    args.extend(["--fixture".into(), fixture]);
    let result = json_output(&run(directory.path(), &args, b"source", false, &[]));
    assert_eq!(result["protocol"], "execute.model/1");
    assert_eq!(result["provider"], "openai");
    assert_eq!(result["kind"], "generate");
    assert_eq!(result["model"], "local-fixture");
    assert_eq!(result["text"], "A short draft.");
    assert_eq!(
        result["usage"],
        json!({"input_tokens": 4, "output_tokens": 2, "total_tokens": 6})
    );
}

#[test]
fn chat_completions_extract_validates_the_schema_like_responses() {
    let directory = tempfile::tempdir().unwrap();
    let schema: Value = serde_json::from_str(COUNT_SCHEMA).unwrap();
    let expected = json!({
        "provider": "openai", "kind": "extract", "model": "gpt-6-luna",
        "state": "two items", "instruction": "Extract the count",
        "schema": schema, "api": "chat_completions"
    });
    let good = write_fixture(
        directory.path(),
        "extract-good.json",
        expected.clone(),
        chat_completion(json!(r#"{"count":2}"#)),
    );
    let mut args = extract(
        COUNT_SCHEMA,
        &[
            "--api",
            "chat_completions",
            "--endpoint",
            "http://127.0.0.1:9/v1",
        ],
    );
    args.extend(["--fixture".into(), good]);
    let result = json_output(&run(directory.path(), &args, b"two items", false, &[]));
    assert_eq!(result["value"], json!({"count": 2}));

    let bad_cases = [
        (r#"{"count":"two"}"#, "schema"),
        (r#"{"count":2,"extra":true}"#, "schema"),
        (r#"{"count":2"#, "schema"),
        (r#"{"count":1,"count":2}"#, "schema"),
    ];
    for (content, class) in bad_cases {
        let fixture = write_fixture(
            directory.path(),
            "extract-bad.json",
            expected.clone(),
            chat_completion(json!(content)),
        );
        let mut args = extract(
            COUNT_SCHEMA,
            &[
                "--api",
                "chat_completions",
                "--endpoint",
                "http://127.0.0.1:9/v1",
            ],
        );
        args.extend(["--fixture".into(), fixture]);
        let output = run(directory.path(), &args, b"two items", false, &[]);
        assert_failure_class(&output, class, content);
    }
}

#[test]
fn chat_completions_refusals_and_truncation_fail_closed_with_static_classes() {
    let directory = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "refusal text present",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":null,"refusal":"no"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
            "refusal",
        ),
        (
            "content filter stop",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":""},"finish_reason":"content_filter"}],"usage":{"prompt_tokens":1,"completion_tokens":0,"total_tokens":1}}),
            "refusal",
        ),
        (
            "length truncation",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":"partial"},"finish_reason":"length"}],"usage":{"prompt_tokens":1,"completion_tokens":512,"total_tokens":513}}),
            "incomplete",
        ),
        (
            "missing finish reason",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":"x"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
            "provider_output",
        ),
        (
            "two choices",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":"a"},"finish_reason":"stop"},{"message":{"role":"assistant","content":"b"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}),
            "provider_output",
        ),
        (
            "no choices",
            json!({"model":"m","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":0,"total_tokens":1}}),
            "provider_output",
        ),
        (
            "null content without refusal",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":0,"total_tokens":1}}),
            "provider_output",
        ),
        (
            "blank content",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":"  \n"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
            "provider_output",
        ),
        (
            "tool calls",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":"x","tool_calls":[{"id":"t"}]},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
            "provider_output",
        ),
        (
            "wrong role",
            json!({"model":"m","choices":[{"message":{"role":"user","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
            "provider_output",
        ),
        (
            "usage missing",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}]}),
            "provider_output",
        ),
        (
            "usage total inconsistent",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":9}}),
            "provider_output",
        ),
        (
            "usage negative",
            json!({"model":"m","choices":[{"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":-1,"completion_tokens":1}}),
            "provider_output",
        ),
        (
            "model missing",
            json!({"choices":[{"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
            "provider_output",
        ),
    ];
    for (name, response, class) in cases {
        let fixture = write_fixture(
            directory.path(),
            "chat-failure.json",
            chat_generate_semantic("source"),
            response,
        );
        let mut args = generate(&[
            "--api",
            "chat_completions",
            "--endpoint",
            "http://127.0.0.1:9/v1",
        ]);
        args.extend(["--fixture".into(), fixture]);
        let output = run(directory.path(), &args, b"source", false, &[]);
        assert_failure_class(&output, class, name);
    }
}

#[test]
fn responses_refusal_and_incomplete_remain_unchanged_under_the_default_api() {
    let directory = tempfile::tempdir().unwrap();
    let mut incomplete = responses_text("partial");
    incomplete["status"] = json!("incomplete");
    incomplete["incomplete_details"] = json!({"reason": "max_output_tokens"});
    let fixture = write_fixture(
        directory.path(),
        "responses-incomplete.json",
        generate_semantic("source"),
        incomplete,
    );
    let mut args = generate(&[]);
    args.extend(["--fixture".into(), fixture]);
    assert_failure_class(
        &run(directory.path(), &args, b"source", false, &[]),
        "incomplete",
        "responses incomplete",
    );
}

// ---------------------------------------------------------------------------
// 4. Request shape on the wire, through the owned loopback server.
// ---------------------------------------------------------------------------

fn bearer_present(captured: &Captured, key: &str) -> bool {
    captured
        .headers
        .lines()
        .any(|line| line == format!("authorization: bearer {}", key.to_ascii_lowercase()))
}

#[test]
fn chat_completions_posts_to_base_with_the_expected_body() {
    let directory = tempfile::tempdir().unwrap();
    let (base, server) = serve(vec![reply(200, &chat_completion(json!("hello")))]);
    let args = generate(&[
        "--api",
        "chat_completions",
        "--endpoint",
        &chat_base(&base),
        "--effort",
        "low",
        "--temperature",
        "0.25",
        "--max-tokens",
        "64",
    ]);
    let result = json_output(&run(directory.path(), &args, b"source text", false, &[]));
    assert_eq!(result["transport"], "http_loopback");
    assert_eq!(result["text"], "hello");

    let captured = server.join().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/v1/chat/completions");
    let body: Value = serde_json::from_slice(&captured[0].body).unwrap();
    assert_eq!(body["model"], "gpt-6-luna");
    assert_eq!(body["stream"], false);
    assert_eq!(body["max_tokens"], 64);
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["temperature"], 0.25);
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "Draft one line"},
            {"role": "user", "content": "source text"}
        ])
    );
    assert!(body.get("response_format").is_none());
    assert!(body.get("reasoning").is_none());
    assert!(body.get("store").is_none());
}

#[test]
fn chat_completions_extract_sends_strict_json_schema_response_format() {
    let directory = tempfile::tempdir().unwrap();
    let (base, server) = serve(vec![reply(200, &chat_completion(json!(r#"{"count":3}"#)))]);
    let args = extract(
        COUNT_SCHEMA,
        &["--api", "chat_completions", "--endpoint", &chat_base(&base)],
    );
    let result = json_output(&run(directory.path(), &args, b"three", false, &[]));
    assert_eq!(result["value"], json!({"count": 3}));

    let captured = server.join().unwrap();
    let body: Value = serde_json::from_slice(&captured[0].body).unwrap();
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(
        body["response_format"]["json_schema"]["schema"],
        serde_json::from_str::<Value>(COUNT_SCHEMA).unwrap()
    );
    assert!(body.get("text").is_none());
}

#[test]
fn responses_api_carries_effort_and_temperature_only_when_given() {
    let directory = tempfile::tempdir().unwrap();
    let (base, server) = serve(vec![
        reply(200, &responses_text("first")),
        reply(200, &responses_text("second")),
    ]);
    let endpoint = format!("{base}/v1/responses");

    let tuned = generate(&[
        "--endpoint",
        &endpoint,
        "--effort",
        "xhigh",
        "--temperature",
        "1.5",
    ]);
    json_output(&run(directory.path(), &tuned, b"s", false, &[]));
    let plain = generate(&["--endpoint", &endpoint]);
    json_output(&run(directory.path(), &plain, b"s", false, &[]));

    let captured = server.join().unwrap();
    let tuned_body: Value = serde_json::from_slice(&captured[0].body).unwrap();
    assert_eq!(tuned_body["reasoning"], json!({"effort": "xhigh"}));
    assert_eq!(tuned_body["temperature"], 1.5);
    assert!(tuned_body.get("reasoning_effort").is_none());
    let plain_body: Value = serde_json::from_slice(&captured[1].body).unwrap();
    assert!(plain_body.get("reasoning").is_none());
    assert!(plain_body.get("temperature").is_none());
    assert_eq!(plain_body["store"], false);
    assert_eq!(plain_body["max_output_tokens"], 512);
}

#[test]
fn loopback_endpoint_without_any_key_sends_no_authorization_header() {
    let directory = tempfile::tempdir().unwrap();
    let (base, server) = serve(vec![reply(200, &chat_completion(json!("local")))]);
    let args = generate(&["--api", "chat_completions", "--endpoint", &chat_base(&base)]);
    let result = json_output(&run(directory.path(), &args, b"s", false, &[]));
    assert_eq!(result["text"], "local");
    let captured = server.join().unwrap();
    assert!(
        !captured[0].headers.contains("authorization"),
        "keyless loopback request carried an Authorization header"
    );
}

#[test]
fn default_provider_keys_are_never_sent_to_a_custom_loopback_server() {
    let directory = tempfile::tempdir().unwrap();
    let env_file = directory.path().join("default-keys.env");
    std::fs::write(
        &env_file,
        "OPENAI_API_KEY=file-default-key\nopenai=file-alias-key\nJEV_API_KEY=file-jev-key\n",
    )
    .unwrap();
    let env_file_text = env_file.to_string_lossy().into_owned();
    let environment = [
        ("OPENAI_API_KEY", "process-default-key"),
        ("openai", "process-alias-key"),
        ("JEV_API_KEY", "process-jev-key"),
        ("TYPESAFE_API_KEY", "process-typesafe-key"),
    ];

    // Process-environment keys first, then env-file keys, both to loopback.
    let (base, server) = serve(vec![
        reply(200, &chat_completion(json!("one"))),
        reply(200, &chat_completion(json!("two"))),
    ]);
    let endpoint = chat_base(&base);
    let process_args = generate(&["--api", "chat_completions", "--endpoint", &endpoint]);
    json_output(&run(
        directory.path(),
        &process_args,
        b"s",
        false,
        &environment,
    ));
    let file_args = generate(&[
        "--api",
        "chat_completions",
        "--endpoint",
        &endpoint,
        "--env-file",
        &env_file_text,
    ]);
    json_output(&run(directory.path(), &file_args, b"s", false, &[]));
    for captured in server.join().unwrap() {
        assert!(
            !captured.headers.contains("authorization"),
            "a default provider key reached a custom loopback server: {}",
            captured.headers
        );
    }

    // The Jev default keys have the same boundary on a custom Jev endpoint.
    let (base, server) = serve(vec![reply(
        200,
        &ask_response(json!({
            "route": route_answer(), "ok": ok_answer(), "rate": rate_answer()
        })),
    )]);
    let mut args = ask_args(&ask_questions_json());
    args.extend([
        "--endpoint".into(),
        format!("{base}/v1/systemone"),
        "--env-file".into(),
        env_file_text,
    ]);
    json_output(&run(directory.path(), &args, b"", false, &environment));
    let captured = server.join().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(
        !captured[0].headers.contains("authorization"),
        "a Jev default key reached a custom loopback server"
    );
}

#[test]
fn api_key_env_names_the_only_key_a_custom_endpoint_receives() {
    let directory = tempfile::tempdir().unwrap();
    let env_file = directory.path().join("local-keys.env");
    std::fs::write(&env_file, "EXECUTE_MODEL_KEY_FILE=file-local-key\n").unwrap();
    let env_file_text = env_file.to_string_lossy().into_owned();

    let (base, server) = serve(vec![reply(200, &chat_completion(json!("process")))]);
    let args = generate(&[
        "--api",
        "chat_completions",
        "--endpoint",
        &chat_base(&base),
        "--api-key-env",
        "EXECUTE_MODEL_KEY_PROCESS",
    ]);
    json_output(&run(
        directory.path(),
        &args,
        b"s",
        false,
        &[
            ("EXECUTE_MODEL_KEY_PROCESS", "process-local-key"),
            ("OPENAI_API_KEY", "default-must-not-be-sent"),
        ],
    ));
    let captured = server.join().unwrap();
    assert!(
        bearer_present(&captured[0], "process-local-key"),
        "named process key was not sent"
    );
    assert!(!captured[0].headers.contains("default-must-not-be-sent"));

    let (base, server) = serve(vec![reply(200, &chat_completion(json!("file")))]);
    let args = generate(&[
        "--api",
        "chat_completions",
        "--endpoint",
        &chat_base(&base),
        "--api-key-env",
        "EXECUTE_MODEL_KEY_FILE",
        "--env-file",
        &env_file_text,
    ]);
    json_output(&run(
        directory.path(),
        &args,
        b"s",
        false,
        &[("OPENAI_API_KEY", "default-must-not-be-sent")],
    ));
    let captured = server.join().unwrap();
    assert!(
        bearer_present(&captured[0], "file-local-key"),
        "named file key was not sent"
    );
    assert!(!captured[0].headers.contains("default-must-not-be-sent"));
}

#[test]
fn api_key_env_naming_a_default_variable_is_rejected_before_any_request() {
    let directory = tempfile::tempdir().unwrap();
    for name in [
        "OPENAI_API_KEY",
        "openai",
        "JEV_API_KEY",
        "TYPESAFE_API_KEY",
        "jev",
        "Openai_Api_Key",
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "PATH",
        "execute_model_key_local",
        "Execute_Model_Key_Local",
        "EXECUTE_MODEL_KEY_",
        "EXECUTE_MODEL_KEYLOCAL",
    ] {
        let args = generate(&["--api-key-env", name, "--endpoint", "http://127.0.0.1:9/v1"]);
        let output = run(
            directory.path(),
            &args,
            b"s",
            true,
            &[("OPENAI_API_KEY", "default-must-not-be-sent")],
        );
        assert_failure_class(&output, "configuration", name);
    }
}

#[test]
fn api_key_env_with_no_value_fails_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    // No replies are queued, so any connection would fail as a transport error
    // instead of the expected credential class.
    let (base, server) = serve(Vec::new());
    let args = generate(&[
        "--api",
        "chat_completions",
        "--endpoint",
        &chat_base(&base),
        "--api-key-env",
        "EXECUTE_MODEL_KEY_UNSET",
    ]);
    assert_failure_class(
        &run(directory.path(), &args, b"s", false, &[]),
        "credentials_missing",
        "unset named key",
    );
    assert!(server.join().unwrap().is_empty());
}

#[test]
fn default_endpoint_without_a_default_key_fails_before_any_connection() {
    let directory = tempfile::tempdir().unwrap();
    let args = generate(&[]);
    let output = run(directory.path(), &args, b"s", false, &[]);
    assert_failure_class(&output, "credentials_missing", "default host without key");
    let jev = argv(&["judge", "--question", "Ok?"]);
    assert_failure_class(
        &run(directory.path(), &jev, b"s", false, &[]),
        "credentials_missing",
        "jev default host without key",
    );
}

#[test]
fn remote_plain_http_endpoints_are_rejected_even_with_a_key() {
    let directory = tempfile::tempdir().unwrap();
    for endpoint in [
        "http://api.example.test/v1",
        "http://10.0.0.5:8080/v1",
        "http://192.168.1.10/v1",
    ] {
        let args = generate(&["--api", "chat_completions", "--endpoint", endpoint]);
        let output = run(
            directory.path(),
            &args,
            b"s",
            false,
            &[("OPENAI_API_KEY", "process-test-key")],
        );
        assert_failure_class(&output, "configuration", endpoint);
    }
}

#[test]
fn malformed_chat_base_urls_are_rejected_before_any_request() {
    let directory = tempfile::tempdir().unwrap();
    for endpoint in [
        "http://user:pass@127.0.0.1:9/v1",
        "http://127.0.0.1:9/v1?x=1",
        "http://127.0.0.1:9/v1#frag",
        "ftp://127.0.0.1:9/v1",
        "http://localhost.evil.test:9/v1",
        "not a url",
        "",
    ] {
        let args = generate(&["--api", "chat_completions", "--endpoint", endpoint]);
        let output = run(
            directory.path(),
            &args,
            b"s",
            false,
            &[("OPENAI_API_KEY", "process-test-key")],
        );
        assert!(!output.status.success(), "accepted {endpoint:?}");
        assert!(output.stdout.is_empty(), "{endpoint:?} wrote stdout");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("test-key"));
    }
}

#[test]
fn trailing_slash_on_chat_base_does_not_double_the_path() {
    let directory = tempfile::tempdir().unwrap();
    let (base, server) = serve(vec![reply(200, &chat_completion(json!("ok")))]);
    let args = generate(&[
        "--api",
        "chat_completions",
        "--endpoint",
        &format!("{base}/v1/"),
    ]);
    json_output(&run(directory.path(), &args, b"s", false, &[]));
    let captured = server.join().unwrap();
    assert_eq!(captured[0].path, "/v1/chat/completions");
}

#[test]
fn chat_completions_redirect_is_not_followed() {
    let directory = tempfile::tempdir().unwrap();
    let (destination, destination_server) =
        serve(vec![reply(200, &chat_completion(json!("leaked")))]);
    let mut redirect = reply(302, &json!({}));
    redirect.extra_headers = format!("Location: {destination}/v1/chat/completions\r\n");
    let (base, source_server) = serve(vec![redirect]);
    let args = generate(&["--api", "chat_completions", "--endpoint", &chat_base(&base)]);
    let output = run(
        directory.path(),
        &args,
        b"s",
        false,
        &[("OPENAI_API_KEY", "process-test-key")],
    );
    assert_failure_class(&output, "http_status 302", "redirect");
    assert_eq!(source_server.join().unwrap().len(), 1);
    assert!(
        destination_server.join().unwrap().is_empty(),
        "redirect was followed"
    );
}

#[test]
fn proxy_environment_is_ignored_for_chat_completions() {
    let directory = tempfile::tempdir().unwrap();
    let (proxy, proxy_server) = serve(vec![reply(200, &chat_completion(json!("proxied")))]);
    let (base, server) = serve(vec![reply(200, &chat_completion(json!("direct")))]);
    let args = generate(&["--api", "chat_completions", "--endpoint", &chat_base(&base)]);
    let result = json_output(&run(
        directory.path(),
        &args,
        b"s",
        false,
        &[
            ("HTTP_PROXY", &proxy),
            ("http_proxy", &proxy),
            ("ALL_PROXY", &proxy),
        ],
    ));
    assert_eq!(result["text"], "direct");
    assert_eq!(server.join().unwrap().len(), 1);
    assert!(
        proxy_server.join().unwrap().is_empty(),
        "proxy received provider traffic"
    );
}

#[test]
fn chat_response_byte_cap_and_request_timeout_are_enforced() {
    let directory = tempfile::tempdir().unwrap();
    let (base, server) = serve(vec![reply(200, &chat_completion(json!("x".repeat(4096))))]);
    let args = generate(&[
        "--api",
        "chat_completions",
        "--endpoint",
        &chat_base(&base),
        "--max-bytes",
        "512",
    ]);
    let output = run(
        directory.path(),
        &args,
        b"s",
        false,
        &[("OPENAI_API_KEY", "process-test-key")],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(server.join().unwrap().len(), 1);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let stalled = format!("http://{}/v1", listener.local_addr().unwrap());
    let stall = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = read_request(&mut stream);
        thread::sleep(Duration::from_millis(1500));
    });
    let args = generate(&[
        "--api",
        "chat_completions",
        "--endpoint",
        &stalled,
        "--timeout-ms",
        "200",
    ]);
    let started = Instant::now();
    let output = run(directory.path(), &args, b"s", false, &[]);
    let elapsed = started.elapsed();
    assert_failure_class(&output, "transport", "stalled chat server");
    assert!(
        elapsed < Duration::from_millis(1200),
        "timeout ignored: {elapsed:?}"
    );
    stall.join().unwrap();
}

#[test]
fn secret_values_never_appear_in_chat_failure_diagnostics() {
    let directory = tempfile::tempdir().unwrap();
    let secret = "synthetic-chat-secret-value";
    let (base, server) = serve(vec![Reply {
        status: 401,
        extra_headers: String::new(),
        body: json!({"error": {"message": secret}})
            .to_string()
            .into_bytes(),
    }]);
    let args = generate(&["--api", "chat_completions", "--endpoint", &chat_base(&base)]);
    let output = run(
        directory.path(),
        &args,
        b"s",
        false,
        &[("OPENAI_API_KEY", secret)],
    );
    assert_failure_class(&output, "http_status 401", "401 from loopback");
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    server.join().unwrap();
}

// ---------------------------------------------------------------------------
// 5. Manifest mappings through the real `execute` CLI. These programs end
//    after `take`, so they need no printf and run on every platform. A fixture
//    mismatch or a missing argument makes the run fail.
// ---------------------------------------------------------------------------

fn run_program(dir: &Path, source: &str, expected: Value, response: Value) -> Output {
    let fixture = write_fixture(dir, "mapping-fixture.json", expected, response);
    let program = dir.join("mapping.program");
    let body = source.replace("FIXTURE", &serde_json::to_string(&fixture).unwrap());
    std::fs::write(&program, body).unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .arg(&program)
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(dir)
        .env("EXECUTE_MODELS_OFFLINE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in CREDENTIAL_VARS.iter().chain(PROXY_VARS.iter()) {
        command.env_remove(name);
    }
    command.output().expect("start execute")
}

#[test]
fn llm_ask_manifest_maps_endpoint_api_effort_and_temperature() {
    let directory = tempfile::tempdir().unwrap();
    let source = r#"llm.ask with model "gpt-6-luna" and instruction "Draft one line" and endpoint "http://127.0.0.1:1/v1" and api "chat_completions" and effort "low" and temperature 0.2 and fixture FIXTURE as raw
then parse json as result
take text at "/text" from result as answer
"#;
    let expected = json!({
        "provider": "openai", "kind": "generate", "model": "gpt-6-luna",
        "state": "", "instruction": "Draft one line",
        "api": "chat_completions", "effort": "low", "temperature": 0.2
    });
    let output = run_program(
        directory.path(),
        source,
        expected,
        chat_completion(json!("draft")),
    );
    assert!(
        output.status.success(),
        "manifest mapping failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn llm_ask_manifest_omits_optional_fields_when_not_written() {
    let directory = tempfile::tempdir().unwrap();
    let source = r#"llm.ask with model "gpt-6-luna" and instruction "Draft one line" and fixture FIXTURE as raw
then parse json as result
take text at "/text" from result as answer
"#;
    let output = run_program(
        directory.path(),
        source,
        generate_semantic(""),
        responses_text("draft"),
    );
    assert!(
        output.status.success(),
        "default mapping gained fields: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn jev_classify_manifest_maps_criteria_and_labels_as_distinct_forms() {
    let directory = tempfile::tempdir().unwrap();
    let criteria_source = r#"jev.classify with question "Route?" and criteria {"billing": "invoices and refunds", "account": "login help"} and fixture FIXTURE as raw
then parse json as result
take text at "/label" from result as label
"#;
    let criteria_expected = json!({
        "provider": "jev", "kind": "classify", "model": "jev-latest",
        "state": "", "question": "Route?", "minimum": 0,
        "criteria": {"billing": "invoices and refunds", "account": "login help"}
    });
    let choice = json!({
        "model": "jev-latest",
        "answers": {"decision": {"type": "choice", "choice": "billing", "confidence": 0.9,
            "probabilities": {"billing": 0.9, "account": 0.1}}},
        "usage": {"input_tokens": 3, "output_tokens": 1}
    });
    let output = run_program(
        directory.path(),
        criteria_source,
        criteria_expected,
        choice.clone(),
    );
    assert!(
        output.status.success(),
        "criteria mapping failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let labels_source = r#"jev.classify with question "Route?" and labels ["billing", "account"] and fixture FIXTURE as raw
then parse json as result
take text at "/label" from result as label
"#;
    let labels_expected = json!({
        "provider": "jev", "kind": "classify", "model": "jev-latest",
        "state": "", "question": "Route?", "minimum": 0,
        "labels": ["billing", "account"]
    });
    let output = run_program(directory.path(), labels_source, labels_expected, choice);
    assert!(
        output.status.success(),
        "labels mapping failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// 6. jev.ask: one batched TypeSafe request with named choice, noul and score
//    questions. Every answer is validated before the call succeeds.
// ---------------------------------------------------------------------------

fn ask_questions_json() -> String {
    json!({
        "route": {"type": "choice", "question": "Route?", "labels": ["billing", "account"], "minimum": 75},
        "ok": {"type": "noul", "question": "Ok?", "minimum": 80},
        "rate": {"type": "score", "question": "Rate?", "levels": ["low", "medium", "high"], "minimum": 50}
    })
    .to_string()
}

fn ask_args(questions: &str) -> Vec<String> {
    argv(&["ask", "--questions", questions])
}

fn ask_semantic(state: &str) -> Value {
    json!({
        "provider": "jev", "kind": "ask", "model": "jev-latest", "state": state,
        "questions": {
            "route": {"type": "choice", "question": "Route?", "minimum": 75, "labels": ["billing", "account"]},
            "ok": {"type": "noul", "question": "Ok?", "minimum": 80},
            "rate": {"type": "score", "question": "Rate?", "minimum": 50, "levels": ["low", "medium", "high"]}
        }
    })
}

fn route_answer() -> Value {
    json!({"type": "choice", "choice": "billing", "confidence": 0.8, "probabilities": {"billing": 0.8, "account": 0.2}})
}

fn ok_answer() -> Value {
    json!({"type": "noul", "noul": 0.1})
}

fn rate_answer() -> Value {
    json!({"type": "score", "score": 1.5, "confidence": 0.6, "legend": {"0": "low", "1": "medium", "2": "high"}, "probabilities": {"0": 0.2, "1": 0.6, "2": 0.2}})
}

fn ask_response(answers: Value) -> Value {
    json!({"model": "jev-latest", "answers": answers, "usage": {"input_tokens": 5, "output_tokens": 3}})
}

fn ask_fixture_run(dir: &Path, answers: Value) -> Output {
    let fixture = write_fixture(dir, "ask.json", ask_semantic(""), ask_response(answers));
    let mut args = ask_args(&ask_questions_json());
    args.extend(["--fixture".into(), fixture]);
    run(dir, &args, b"", false, &[])
}

#[test]
fn batched_ask_returns_one_normalized_answer_per_named_question() {
    let directory = tempfile::tempdir().unwrap();
    let result = json_output(&ask_fixture_run(
        directory.path(),
        json!({"route": route_answer(), "ok": ok_answer(), "rate": rate_answer()}),
    ));
    assert_eq!(result["kind"], "ask");
    assert_eq!(result["provider"], "jev");
    assert_eq!(result["transport"], "fixture");
    assert_eq!(result["usage"]["total_tokens"], 8);
    assert_eq!(
        result["answers"],
        json!({
            "route": {"label": "billing", "confidence_percent": 80, "accepted": true},
            "ok": {"decision": false, "confidence_percent": 90, "accepted": true},
            "rate": {"score": 1.5, "confidence_percent": 60, "accepted": true}
        })
    );
    assert!(
        result.get("label").is_none(),
        "batch result leaked a single-question field"
    );
}

#[test]
fn batched_ask_accepted_follows_each_question_minimum_independently() {
    let directory = tempfile::tempdir().unwrap();
    let low_route = json!({"type": "choice", "choice": "account", "confidence": 0.7, "probabilities": {"billing": 0.3, "account": 0.7}});
    let low_ok = json!({"type": "noul", "noul": 0.4});
    let boundary_rate = json!({"type": "score", "score": 0.0, "confidence": 0.5, "legend": {"0": "low", "1": "medium", "2": "high"}, "probabilities": {"0": 0.5, "1": 0.25, "2": 0.25}});
    let result = json_output(&ask_fixture_run(
        directory.path(),
        json!({"route": low_route, "ok": low_ok, "rate": boundary_rate}),
    ));
    // 70 is below 75. Noul 0.4 gives decision false with confidence 60, below 80.
    // A confidence of exactly 50 meets the minimum of 50.
    assert_eq!(
        result["answers"]["route"],
        json!({"label": "account", "confidence_percent": 70, "accepted": false})
    );
    assert_eq!(
        result["answers"]["ok"],
        json!({"decision": false, "confidence_percent": 60, "accepted": false})
    );
    assert_eq!(
        result["answers"]["rate"],
        json!({"score": 0.0, "confidence_percent": 50, "accepted": true})
    );
}

#[test]
fn an_invalid_answer_in_any_question_fails_the_whole_batch() {
    let directory = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, Value)> = vec![
        (
            "route unknown label",
            json!({"route": {"type": "choice", "choice": "other", "confidence": 0.8, "probabilities": {"billing": 0.2, "account": 0.8}}, "ok": ok_answer(), "rate": rate_answer()}),
        ),
        (
            "route missing a probability",
            json!({"route": {"type": "choice", "choice": "billing", "confidence": 0.8, "probabilities": {"billing": 0.8}}, "ok": ok_answer(), "rate": rate_answer()}),
        ),
        (
            "route wrong type",
            json!({"route": ok_answer(), "ok": ok_answer(), "rate": rate_answer()}),
        ),
        (
            "route confidence above one",
            json!({"route": {"type": "choice", "choice": "billing", "confidence": 1.2, "probabilities": {"billing": 0.8, "account": 0.2}}, "ok": ok_answer(), "rate": rate_answer()}),
        ),
        (
            "route is not an object",
            json!({"route": "billing", "ok": ok_answer(), "rate": rate_answer()}),
        ),
        (
            "ok missing noul",
            json!({"route": route_answer(), "ok": {"type": "noul"}, "rate": rate_answer()}),
        ),
        (
            "ok wrong type",
            json!({"route": route_answer(), "ok": {"type": "choice", "choice": "billing", "confidence": 0.5, "probabilities": {"billing": 0.5, "account": 0.5}}, "rate": rate_answer()}),
        ),
        (
            "ok noul above one",
            json!({"route": route_answer(), "ok": {"type": "noul", "noul": 1.5}, "rate": rate_answer()}),
        ),
        (
            "rate score out of range",
            json!({"route": route_answer(), "ok": ok_answer(), "rate": {"type": "score", "score": 3.0, "confidence": 0.6, "legend": {"0": "low", "1": "medium", "2": "high"}, "probabilities": {"0": 0.2, "1": 0.6, "2": 0.2}}}),
        ),
        (
            "rate legend differs",
            json!({"route": route_answer(), "ok": ok_answer(), "rate": {"type": "score", "score": 1.0, "confidence": 0.6, "legend": {"0": "bad", "1": "medium", "2": "high"}, "probabilities": {"0": 0.2, "1": 0.6, "2": 0.2}}}),
        ),
        (
            "rate probabilities do not sum to one",
            json!({"route": route_answer(), "ok": ok_answer(), "rate": {"type": "score", "score": 1.0, "confidence": 0.6, "legend": {"0": "low", "1": "medium", "2": "high"}, "probabilities": {"0": 0.9, "1": 0.9, "2": 0.9}}}),
        ),
        (
            "answers missing a question",
            json!({"route": route_answer(), "ok": ok_answer()}),
        ),
        (
            "answers with an extra name",
            json!({"route": route_answer(), "ok": ok_answer(), "rate": rate_answer(), "bonus": ok_answer()}),
        ),
        (
            "answers with a renamed question",
            json!({"route": route_answer(), "ok": ok_answer(), "scale": rate_answer()}),
        ),
    ];
    for (name, answers) in cases {
        let output = ask_fixture_run(directory.path(), answers);
        assert_failure_class(&output, "provider_output", name);
    }
    // The untouched batch still passes, so each failure above is attributable.
    let good = json!({"route": route_answer(), "ok": ok_answer(), "rate": rate_answer()});
    json_output(&ask_fixture_run(directory.path(), good));
}

#[test]
fn batched_ask_caps_and_bounds_are_enforced_before_any_request() {
    let directory = tempfile::tempdir().unwrap();
    let noul = |index: usize| {
        (
            format!("q{index}"),
            json!({"type": "noul", "question": "Ok?"}),
        )
    };
    let thirty_two: Value = Value::Object((0..32).map(noul).collect());
    let thirty_three: Value = Value::Object((0..33).map(noul).collect());
    let long_name = "n".repeat(128);
    let too_long_name = "n".repeat(129);
    let labels_128: Vec<String> = (0..128).map(|index| format!("l{index}")).collect();
    let labels_129: Vec<String> = (0..129).map(|index| format!("l{index}")).collect();

    let accepted: Vec<(&str, Value)> = vec![
        ("32 questions", thirty_two),
        (
            "name of 128 bytes",
            json!({ (long_name): {"type": "noul", "question": "Ok?"} }),
        ),
        (
            "minimum 0 and 100",
            json!({"a": {"type": "noul", "question": "Ok?", "minimum": 0}, "b": {"type": "noul", "question": "Ok?", "minimum": 100}}),
        ),
        (
            "choice with 128 labels",
            json!({"a": {"type": "choice", "question": "Route?", "labels": labels_128}}),
        ),
    ];
    for (name, questions) in accepted {
        let args = ask_args(&questions.to_string());
        assert_failure_class(
            &run(directory.path(), &args, b"", true, &[]),
            "offline",
            name,
        );
    }

    let rejected: Vec<(&str, Value)> = vec![
        ("33 questions", thirty_three),
        (
            "name of 129 bytes",
            json!({ (too_long_name): {"type": "noul", "question": "Ok?"} }),
        ),
        (
            "minimum 101",
            json!({"a": {"type": "noul", "question": "Ok?", "minimum": 101}}),
        ),
        (
            "minimum fractional",
            json!({"a": {"type": "noul", "question": "Ok?", "minimum": 50.5}}),
        ),
        (
            "minimum negative",
            json!({"a": {"type": "noul", "question": "Ok?", "minimum": -1}}),
        ),
        (
            "choice with 129 labels",
            json!({"a": {"type": "choice", "question": "Route?", "labels": labels_129}}),
        ),
    ];
    for (name, questions) in rejected {
        let args = ask_args(&questions.to_string());
        let output = run(directory.path(), &args, b"", true, &[]);
        assert_failure_class(&output, "configuration", name);
    }
}

#[test]
fn batched_ask_question_shapes_are_validated_before_any_request() {
    let directory = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, Value)> = vec![
        ("missing type", json!({"a": {"question": "Ok?"}})),
        (
            "unknown type",
            json!({"a": {"type": "rank", "question": "Ok?"}}),
        ),
        ("missing question text", json!({"a": {"type": "noul"}})),
        (
            "blank question text",
            json!({"a": {"type": "noul", "question": "   "}}),
        ),
        ("question not an object", json!({"a": "Ok?"})),
        (
            "unknown field",
            json!({"a": {"type": "noul", "question": "Ok?", "tone": "warm"}}),
        ),
        (
            "choice with labels and criteria",
            json!({"a": {"type": "choice", "question": "Route?", "labels": ["x", "y"], "criteria": {"x": "p", "y": "q"}}}),
        ),
        (
            "choice with neither",
            json!({"a": {"type": "choice", "question": "Route?"}}),
        ),
        (
            "choice with levels",
            json!({"a": {"type": "choice", "question": "Route?", "labels": ["x", "y"], "levels": ["x", "y"]}}),
        ),
        (
            "choice with one label",
            json!({"a": {"type": "choice", "question": "Route?", "labels": ["x"]}}),
        ),
        (
            "choice with one criterion",
            json!({"a": {"type": "choice", "question": "Route?", "criteria": {"x": "p"}}}),
        ),
        (
            "criterion description not text",
            json!({"a": {"type": "choice", "question": "Route?", "criteria": {"x": "p", "y": 3}}}),
        ),
        (
            "criterion description blank",
            json!({"a": {"type": "choice", "question": "Route?", "criteria": {"x": "", "y": "q"}}}),
        ),
        (
            "label begins with dashes",
            json!({"a": {"type": "choice", "question": "Route?", "labels": ["--x", "y"]}}),
        ),
        (
            "duplicate labels",
            json!({"a": {"type": "choice", "question": "Route?", "labels": ["x", "x"]}}),
        ),
        (
            "label not text",
            json!({"a": {"type": "choice", "question": "Route?", "labels": ["x", 2]}}),
        ),
        (
            "noul with labels",
            json!({"a": {"type": "noul", "question": "Ok?", "labels": ["x", "y"]}}),
        ),
        (
            "noul with criteria",
            json!({"a": {"type": "noul", "question": "Ok?", "criteria": {"x": "p", "y": "q"}}}),
        ),
        (
            "score with labels",
            json!({"a": {"type": "score", "question": "Rate?", "labels": ["x", "y"]}}),
        ),
        (
            "score without levels",
            json!({"a": {"type": "score", "question": "Rate?"}}),
        ),
        (
            "score with one level",
            json!({"a": {"type": "score", "question": "Rate?", "levels": ["x"]}}),
        ),
        (
            "score duplicate levels",
            json!({"a": {"type": "score", "question": "Rate?", "levels": ["x", "x"]}}),
        ),
        (
            "question list instead of object",
            json!([{"type": "noul", "question": "Ok?"}]),
        ),
        (
            "name is empty",
            json!({"": {"type": "noul", "question": "Ok?"}}),
        ),
    ];
    for (name, questions) in cases {
        let args = ask_args(&questions.to_string());
        assert_failure_class(
            &run(directory.path(), &args, b"", true, &[]),
            "configuration",
            name,
        );
    }

    // Duplicate names in the question object are rejected as JSON, not merged.
    let duplicate =
        r#"{"a":{"type":"noul","question":"Ok?"},"a":{"type":"noul","question":"Ok?"}}"#;
    assert_failure_class(
        &run(directory.path(), &ask_args(duplicate), b"", true, &[]),
        "configuration",
        "duplicate names",
    );
}

#[test]
fn batched_ask_mixes_criteria_and_levels_and_sends_one_request() {
    let directory = tempfile::tempdir().unwrap();
    let answers = json!({
        "route": {"type": "choice", "choice": "billing", "confidence": 0.9, "probabilities": {"billing": 0.9, "account": 0.1}},
        "ok": {"type": "noul", "noul": 0.95},
        "rate": rate_answer()
    });
    let (base, server) = serve(vec![reply(200, &ask_response(answers))]);
    let questions = json!({
        "route": {"type": "choice", "question": "Route?", "criteria": {"billing": "invoices and refunds", "account": "login help"}},
        "ok": {"type": "noul", "question": "Ok?"},
        "rate": {"type": "score", "question": "Rate?", "levels": ["low", "medium", "high"]}
    });
    let mut args = ask_args(&questions.to_string());
    args.extend(["--endpoint".into(), format!("{base}/v1/systemone")]);
    let result = json_output(&run(directory.path(), &args, b"", false, &[]));
    assert_eq!(result["answers"]["route"]["label"], "billing");
    assert_eq!(
        result["answers"]["ok"],
        json!({"decision": true, "confidence_percent": 95, "accepted": true})
    );

    let captured = server.join().unwrap();
    assert_eq!(captured.len(), 1, "a batch must be one provider request");
    assert_eq!(captured[0].path, "/v1/systemone");
    let body: Value = serde_json::from_slice(&captured[0].body).unwrap();
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["state"], "");
    let sent = &body["questions"];
    assert_eq!(sent["route"]["type"], "choice");
    assert_eq!(sent["route"]["instructions"], "Route?");
    assert_eq!(
        sent["route"]["criteria"],
        json!({"billing": "invoices and refunds", "account": "login help"})
    );
    assert_eq!(sent["ok"]["type"], "noul");
    assert_eq!(sent["ok"]["instructions"], "Ok?");
    assert_eq!(sent["rate"]["type"], "score");
    assert_eq!(sent["rate"]["criteria"], json!(["low", "medium", "high"]));
    assert!(
        body.get("minimum").is_none(),
        "per-question minimum must not reach the provider"
    );
}

#[test]
fn batched_ask_answer_set_must_match_the_request_on_the_wire() {
    let directory = tempfile::tempdir().unwrap();
    let answers = json!({"route": route_answer(), "ok": ok_answer()});
    let (base, server) = serve(vec![reply(200, &ask_response(answers))]);
    let mut args = ask_args(&ask_questions_json());
    args.extend(["--endpoint".into(), format!("{base}/v1/systemone")]);
    assert_failure_class(
        &run(directory.path(), &args, b"", false, &[]),
        "provider_output",
        "missing answer on the wire",
    );
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn batched_ask_branches_a_program_on_two_answers() {
    let directory = tempfile::tempdir().unwrap();
    let questions = ask_questions_json();
    let fixture = write_fixture(
        directory.path(),
        "branch.json",
        ask_semantic(""),
        ask_response(json!({"route": route_answer(), "ok": ok_answer(), "rate": rate_answer()})),
    );
    // Joined lines keep the nested indentation that `if` blocks require.
    let program = [
        format!(
            "jev.ask with questions {questions} and fixture {} as raw",
            serde_json::to_string(&fixture).unwrap()
        ),
        "then parse json as result".to_owned(),
        "take boolean at \"/answers/ok/accepted\" from result as ok_accepted".to_owned(),
        "take text at \"/answers/route/label\" from result as route".to_owned(),
        "if ok_accepted is true".to_owned(),
        "  if route is \"billing\"".to_owned(),
        "    then save to \"billing.json\"".to_owned(),
        "  otherwise".to_owned(),
        "    then save to \"other.json\"".to_owned(),
        "otherwise".to_owned(),
        "  then save to \"review.json\"".to_owned(),
    ]
    .join("\n")
        + "\n";
    std::fs::write(directory.path().join("branch.program"), program).unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .arg("branch.program")
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory.path())
        .env("EXECUTE_MODELS_OFFLINE", "1")
        .stdin(Stdio::null());
    for name in CREDENTIAL_VARS.iter().chain(PROXY_VARS.iter()) {
        command.env_remove(name);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        directory.path().join("billing.json").exists(),
        "billing branch not taken"
    );
    assert!(!directory.path().join("other.json").exists());
    assert!(!directory.path().join("review.json").exists());
    let saved: Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("billing.json")).unwrap())
            .unwrap();
    assert_eq!(saved["answers"]["route"]["label"], "billing");
}

// ---------------------------------------------------------------------------
// 7. Typed results: a v1 contract makes `as NAME` bind a decoded JSON record,
//    so a program can read fields with `take` and skip `parse json`. The
//    pipeline still carries the raw bytes, so a later `parse json` still works.
// ---------------------------------------------------------------------------

fn judge_expected() -> Value {
    json!({
        "provider": "jev", "kind": "judge", "model": "jev-latest",
        "state": "", "question": "Is it true?", "minimum": 80
    })
}

fn judge_response() -> Value {
    json!({
        "model": "jev-latest",
        "answers": {"decision": {"type": "noul", "noul": 0.1}},
        "usage": {"input_tokens": 4, "output_tokens": 1}
    })
}

#[test]
fn typed_binding_reads_fields_without_parse_json() {
    let directory = tempfile::tempdir().unwrap();
    let source = r#"jev.judge with question "Is it true?" and minimum 80 and fixture FIXTURE as verdict
take boolean at "/accepted" from verdict as accepted
take boolean at "/decision" from verdict as decision
if accepted is true
  if decision is false
    then save to "typed-ok.txt"
"#;
    let output = run_program(directory.path(), source, judge_expected(), judge_response());
    assert!(
        output.status.success(),
        "typed binding failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        directory.path().join("typed-ok.txt").exists(),
        "typed branch not taken"
    );
}

#[test]
fn parse_json_after_a_typed_binding_still_reads_the_raw_pipeline() {
    let directory = tempfile::tempdir().unwrap();
    let source = r#"jev.judge with question "Is it true?" and minimum 80 and fixture FIXTURE as verdict
then parse json as result
take boolean at "/accepted" from result as accepted
if accepted is true
  then save to "pipeline-ok.txt"
"#;
    let output = run_program(directory.path(), source, judge_expected(), judge_response());
    assert!(
        output.status.success(),
        "pipeline parse failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        directory.path().join("pipeline-ok.txt").exists(),
        "pipeline branch not taken"
    );
}

// ---------------------------------------------------------------------------
// 8. Breadth: checkpoint resume, limits, and extraction through the real CLI.
//    The checkpoint test uses an owned loopback server that accepts exactly one
//    request. A second request could not connect, so a resume that re-requested
//    the model would fail as a transport error instead of the expected step.
// ---------------------------------------------------------------------------

fn checkpoint_program(endpoint: &str) -> String {
    format!(
        "llm.ask with model \"gpt-6-luna\" and instruction \"Draft one line\" and endpoint {} and api \"chat_completions\" as raw\n\
then parse json as result\n\
take text at \"/text\" from result as answer\n\
take text at \"/missing\" from result as bad\n",
        serde_json::to_string(endpoint).unwrap()
    )
}

fn run_execute(directory: &Path, args: &[&str]) -> Output {
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .args(args)
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in CREDENTIAL_VARS.iter().chain(PROXY_VARS.iter()) {
        command.env_remove(name);
    }
    command.env_remove("EXECUTE_MODELS_OFFLINE");
    command.output().expect("start execute")
}

#[test]
fn completed_model_call_is_not_requested_again_when_a_checkpoint_resumes() {
    let directory = tempfile::tempdir().unwrap();
    let (base, server) = serve(vec![reply(200, &chat_completion(json!("draft")))]);
    let program = directory.path().join("checkpoint.program");
    std::fs::write(&program, checkpoint_program(&format!("{base}/v1"))).unwrap();
    let checkpoint = directory.path().join("checkpoint");
    let program_text = program.to_string_lossy().into_owned();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();

    // First run: the model call completes and is checkpointed, then a later
    // step fails. The single queued reply is consumed here.
    let first = run_execute(
        directory.path(),
        &[&program_text, "--checkpoint", &checkpoint_text],
    );
    assert!(!first.status.success());
    assert!(
        String::from_utf8_lossy(&first.stderr).contains("JSON pointer"),
        "first run did not reach the failing step: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        server.join().unwrap().len(),
        1,
        "first run made one request"
    );

    // The listener is closed now. Any re-request would fail as a transport error.
    for _ in 0..2 {
        let resumed = run_execute(
            directory.path(),
            &["resume", &program_text, "--checkpoint", &checkpoint_text],
        );
        let stderr = String::from_utf8_lossy(&resumed.stderr).into_owned();
        assert!(!resumed.status.success());
        assert!(
            stderr.contains("JSON pointer"),
            "resume re-requested the model or changed failure: {stderr}"
        );
        assert!(
            !stderr.contains("transport"),
            "resume re-requested the model: {stderr}"
        );
    }
}

#[test]
fn input_limit_rejects_oversized_state_before_any_model_call() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = write_fixture(
        directory.path(),
        "limit.json",
        generate_semantic("abcdefgh"),
        responses_text("unused"),
    );
    let args = generate(&["--max-bytes", "4", "--fixture", &fixture]);
    let output = run(directory.path(), &args, b"abcdefgh", false, &[]);
    assert_failure_class(&output, "input", "oversized state");
}

#[test]
fn output_limit_rejects_an_oversized_jev_response_on_the_wire() {
    let directory = tempfile::tempdir().unwrap();
    let big = json!({
        "model": "jev-latest",
        "answers": {"decision": {"type": "noul", "noul": 0.5}},
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "padding": "x".repeat(4096)
    });
    let (base, server) = serve(vec![reply(200, &big)]);
    let args = argv(&[
        "judge",
        "--question",
        "Ok?",
        "--endpoint",
        &format!("{base}/v1/systemone"),
        "--max-bytes",
        "512",
    ]);
    let output = run(directory.path(), &args, b"s", false, &[]);
    assert_failure_class(&output, "output_limit", "oversized jev response");
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn extracted_fields_feed_take_and_branch_through_the_cli() {
    let directory = tempfile::tempdir().unwrap();
    let schema: Value = serde_json::from_str(COUNT_SCHEMA).unwrap();
    let fixture = write_fixture(
        directory.path(),
        "extract.json",
        json!({
            "provider": "openai", "kind": "extract", "model": "gpt-6-luna",
            "state": "", "instruction": "Extract the count", "schema": schema
        }),
        responses_text(r#"{"count":2}"#),
    );
    // Joined lines keep the indentation that the `if` block requires.
    let source = [
        format!(
            "llm.extract with model \"gpt-6-luna\" and instruction \"Extract the count\" and schema {} and fixture {} as raw",
            serde_json::to_string(COUNT_SCHEMA).unwrap(),
            serde_json::to_string(&fixture).unwrap()
        ),
        "then parse json as result".to_owned(),
        "take integer at \"/value/count\" from result as count".to_owned(),
        "if count is 2".to_owned(),
        "  then save to \"two.txt\"".to_owned(),
    ]
    .join("\n")
        + "\n";
    let program = directory.path().join("extract.program");
    std::fs::write(&program, source).unwrap();
    let output = run_execute(directory.path(), &[&program.to_string_lossy()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        directory.path().join("two.txt").exists(),
        "extracted branch not taken"
    );
}
