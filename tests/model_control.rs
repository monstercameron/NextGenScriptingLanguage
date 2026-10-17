//! Offline security and checkpoint checks for the native `execute-model` child.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MODEL_HELPER: &str = env!("CARGO_BIN_EXE_execute-model");
const EXECUTE: &str = env!("CARGO_BIN_EXE_execute");

fn run_helper(args: &[String], input: &[u8], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(MODEL_HELPER);
    command
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("start execute-model");
    child
        .stdin
        .take()
        .expect("piped model stdin")
        .write_all(input)
        .expect("write model state");
    child.wait_with_output().expect("wait for execute-model")
}

fn run_fixture(args: &[String], input: &[u8], expected: Value, response: Value) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("fixture.json");
    std::fs::write(
        &fixture,
        json!({ "expected_request": expected, "response": response }).to_string(),
    )
    .unwrap();
    let mut with_fixture = args.to_vec();
    with_fixture.extend(["--fixture".into(), fixture.to_string_lossy().into_owned()]);
    run_helper(&with_fixture, input, &[])
}

fn loopback() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").expect("bind owned loopback listener")
}

fn endpoint(listener: &TcpListener, path: &str) -> String {
    format!(
        "http://127.0.0.1:{}{path}",
        listener.local_addr().unwrap().port()
    )
}

fn http_response(status: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn accept_once(listener: TcpListener, response: Vec<u8>) -> JoinHandle<bool> {
    thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_millis(700);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = read_headers(&mut stream);
                    let _ = stream.write_all(&response);
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return false,
            }
        }
    })
}

fn read_headers(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    while bytes.len() < 64 * 1024 && !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(bytes)
}

fn wait_bounded(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => return None,
        }
    }
}

fn collect_output(child: &mut Child, status: ExitStatus) -> Output {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}

fn invoke_execute(directory: &Path, manifests: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(EXECUTE)
        .current_dir(directory)
        .args(args)
        .arg("--manifest-dir")
        .arg(manifests)
        .env_clear()
        .env("JEV_API_KEY", "synthetic-test-key")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start execute");
    let status = wait_bounded(&mut child, Duration::from_secs(5)).unwrap_or_else(|| {
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        if wait_bounded(&mut child, Duration::from_secs(1)).is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("checkpointed model command exceeded five-second test deadline");
    });
    collect_output(&mut child, status)
}

fn openai_response(text: &str) -> Value {
    json!({
        "model": "model-under-test",
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "output": [{
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": text }]
        }],
        "usage": { "input_tokens": 2, "output_tokens": 1 }
    })
}

fn openai_args(endpoint: &str) -> Vec<String> {
    [
        "generate",
        "--provider",
        "openai",
        "--model",
        "model-under-test",
        "--instruction",
        "return a short result",
        "--endpoint",
        endpoint,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[test]
fn fixture_replay_never_reads_env_file_or_contacts_loopback() {
    let directory = tempfile::tempdir().unwrap();
    let listener = loopback();
    let monitor = accept_once(
        listener.try_clone().unwrap(),
        http_response(
            "200 OK",
            openai_response("should not be sent").to_string().as_bytes(),
            "",
        ),
    );
    let missing_env_file = directory.path().join("does-not-exist.env");
    let expected = json!({
        "provider": "openai",
        "kind": "generate",
        "model": "model-under-test",
        "state": "source text",
        "instruction": "return a short result"
    });
    let fixture = directory.path().join("fixture.json");
    std::fs::write(
        &fixture,
        json!({ "expected_request": expected, "response": openai_response("fixture result") })
            .to_string(),
    )
    .unwrap();

    let mut args = openai_args(&endpoint(&listener, "/v1/responses"));
    args.extend([
        "--fixture".into(),
        fixture.to_string_lossy().into_owned(),
        "--env-file".into(),
        missing_env_file.to_string_lossy().into_owned(),
    ]);
    let output = run_helper(&args, b"source text", &[]);

    assert!(
        output.status.success(),
        "fixture replay failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["transport"], "fixture");
    assert_eq!(result["text"], "fixture result");
    assert!(
        !monitor.join().unwrap(),
        "fixture replay made a network request"
    );
}

#[test]
fn fixture_mismatch_fails_closed_before_loopback_or_credential_lookup() {
    let directory = tempfile::tempdir().unwrap();
    let listener = loopback();
    let monitor = accept_once(
        listener.try_clone().unwrap(),
        http_response(
            "200 OK",
            openai_response("must not be used").to_string().as_bytes(),
            "",
        ),
    );
    let fixture = directory.path().join("mismatched.json");
    let wrong_request = json!({
        "provider": "openai",
        "kind": "generate",
        "model": "model-under-test",
        "state": "different source",
        "instruction": "return a short result"
    });
    std::fs::write(
        &fixture,
        json!({ "expected_request": wrong_request, "response": openai_response("wrong") })
            .to_string(),
    )
    .unwrap();
    let mut args = openai_args(&endpoint(&listener, "/v1/responses"));
    args.extend(["--fixture".into(), fixture.to_string_lossy().into_owned()]);

    let output = run_helper(&args, b"source text", &[]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        !monitor.join().unwrap(),
        "fixture mismatch made a network request"
    );
}

#[test]
fn empty_fixture_is_rejected_instead_of_falling_through_to_live_transport() {
    let listener = loopback();
    let valid = openai_response("live response").to_string();
    let monitor = accept_once(
        listener.try_clone().unwrap(),
        http_response("200 OK", valid.as_bytes(), ""),
    );
    let mut args = openai_args(&endpoint(&listener, "/v1/responses"));
    args.extend(["--fixture".into(), String::new()]);

    let output = run_helper(
        &args,
        b"source text",
        &[("OPENAI_API_KEY", "synthetic-test-key")],
    );

    assert!(
        !output.status.success(),
        "an explicit empty fixture unexpectedly became a live request"
    );
    assert!(!monitor.join().unwrap(), "empty fixture contacted loopback");
}

#[test]
fn explicit_dotenv_secret_is_not_echoed_when_owned_server_rejects_request() {
    let directory = tempfile::tempdir().unwrap();
    let listener = loopback();
    let secret = "synthetic-secret-for-redaction-test";
    let error_body = json!({ "error": { "message": secret } }).to_string();
    let monitor = accept_once(
        listener.try_clone().unwrap(),
        http_response("401 Unauthorized", error_body.as_bytes(), ""),
    );
    let env_file = directory.path().join("synthetic-provider-credentials");
    std::fs::write(&env_file, format!("EXECUTE_MODEL_KEY_DOTENV={secret}\n")).unwrap();
    let mut args = openai_args(&endpoint(&listener, "/v1/responses"));
    args.extend([
        "--env-file".into(),
        env_file.to_string_lossy().into_owned(),
        "--api-key-env".into(),
        "EXECUTE_MODEL_KEY_DOTENV".into(),
    ]);

    let output = run_helper(&args, b"source text", &[]);
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!output.status.success());
    assert!(
        monitor.join().unwrap(),
        "helper did not reach the owned loopback server"
    );
    assert!(output.stdout.is_empty());
    assert!(
        !diagnostics.contains(secret),
        "diagnostic leaked explicit dotenv content"
    );
}

#[test]
fn custom_key_env_is_read_from_the_file_and_default_keys_are_not_sent() {
    let directory = tempfile::tempdir().unwrap();
    let listener = loopback();
    let endpoint = endpoint(&listener, "/v1/responses");
    let response = http_response(
        "200 OK",
        openai_response("file credential worked")
            .to_string()
            .as_bytes(),
        "",
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let headers = read_headers(&mut stream).unwrap();
        let headers = String::from_utf8_lossy(&headers).to_ascii_lowercase();
        let had_expected_auth = headers.contains("authorization: bearer synthetic-file-key");
        stream.write_all(&response).unwrap();
        had_expected_auth
    });
    let env_file = directory.path().join("synthetic-credentials");
    std::fs::write(
        &env_file,
        "EXECUTE_MODEL_KEY_FILE=synthetic-file-key\nUNRELATED_SETTING=ignored\n",
    )
    .unwrap();
    let mut args = openai_args(&endpoint);
    args.extend([
        "--env-file".into(),
        env_file.to_string_lossy().into_owned(),
        "--api-key-env".into(),
        "EXECUTE_MODEL_KEY_FILE".into(),
    ]);
    let output = run_helper(&args, b"source", &[("OPENAI_API_KEY", "")]);
    assert!(
        output.status.success(),
        "empty process credential did not fall back to the file: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("file credential worked"));
    assert!(
        server.join().unwrap(),
        "file credential was not sent as a Bearer token"
    );
}

#[test]
fn live_request_timeout_is_enforced_by_helper() {
    let listener = loopback();
    let endpoint = endpoint(&listener, "/v1/responses");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = read_headers(&mut stream);
        thread::sleep(Duration::from_millis(700));
        let response = http_response(
            "200 OK",
            openai_response("too late").to_string().as_bytes(),
            "",
        );
        let _ = stream.write_all(&response);
    });
    let mut args = openai_args(&endpoint);
    args.extend(["--timeout-ms".into(), "100".into()]);
    let start = Instant::now();
    let output = run_helper(
        &args,
        b"source",
        &[("OPENAI_API_KEY", "synthetic-test-key")],
    );
    let elapsed = start.elapsed();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed < Duration::from_millis(600),
        "request timeout was not honored: {elapsed:?}"
    );
    server.join().unwrap();
}

#[test]
fn malformed_utf8_and_oversized_input_fail_without_provider_calls() {
    let directory = tempfile::tempdir().unwrap();
    let expected = json!({
        "provider": "openai",
        "kind": "generate",
        "model": "model-under-test",
        "state": "unused",
        "instruction": "return a short result"
    });
    let fixture = directory.path().join("fixture.json");
    std::fs::write(
        &fixture,
        json!({ "expected_request": expected, "response": openai_response("ok") }).to_string(),
    )
    .unwrap();
    let mut args = [
        "generate",
        "--provider",
        "openai",
        "--model",
        "model-under-test",
        "--instruction",
        "return a short result",
        "--max-bytes",
        "2",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    args.extend(["--fixture".into(), fixture.to_string_lossy().into_owned()]);

    let oversized = run_helper(&args, b"abc", &[]);
    assert!(!oversized.status.success());
    assert!(oversized.stdout.is_empty());

    args[8] = "8".into();
    let invalid_utf8 = run_helper(&args, &[0xff], &[]);
    assert!(!invalid_utf8.status.success());
    assert!(invalid_utf8.stdout.is_empty());
}

#[test]
fn valid_utf8_nul_state_is_fixture_matched_and_json_escaped() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("fixture.json");
    let expected = json!({
        "provider": "openai",
        "kind": "generate",
        "model": "model-under-test",
        "state": "\u{0}input",
        "instruction": "return a short result"
    });
    std::fs::write(
        &fixture,
        json!({ "expected_request": expected, "response": openai_response("ok") }).to_string(),
    )
    .unwrap();
    let mut args = openai_args("http://127.0.0.1:1/unused");
    args.extend(["--fixture".into(), fixture.to_string_lossy().into_owned()]);

    let output = run_helper(&args, b"\0input", &[]);

    assert!(
        output.status.success(),
        "NUL is valid UTF-8 model state and must be JSON-escaped: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["text"], "ok");
}

#[test]
fn response_byte_limit_is_enforced_on_owned_loopback_body() {
    let listener = loopback();
    let body = openai_response(&"x".repeat(2048)).to_string();
    let monitor = accept_once(
        listener.try_clone().unwrap(),
        http_response("200 OK", body.as_bytes(), ""),
    );
    let mut args = openai_args(&endpoint(&listener, "/v1/responses"));
    args.extend(["--max-bytes".into(), "256".into()]);

    let output = run_helper(&args, b"short", &[("OPENAI_API_KEY", "synthetic-test-key")]);

    assert!(!output.status.success());
    assert!(monitor.join().unwrap());
    assert!(output.stdout.is_empty());
}

#[test]
fn loopback_redirect_is_rejected_without_following_destination() {
    let source = loopback();
    let destination = loopback();
    let destination_monitor = accept_once(
        destination.try_clone().unwrap(),
        http_response(
            "200 OK",
            openai_response("should not arrive").to_string().as_bytes(),
            "",
        ),
    );
    let location = endpoint(&destination, "/redirected");
    let source_monitor = accept_once(
        source.try_clone().unwrap(),
        http_response("302 Found", b"", &format!("Location: {location}\r\n")),
    );
    let args = openai_args(&endpoint(&source, "/v1/responses"));

    let output = run_helper(&args, b"short", &[("OPENAI_API_KEY", "synthetic-test-key")]);

    assert!(!output.status.success());
    assert!(source_monitor.join().unwrap());
    assert!(
        !destination_monitor.join().unwrap(),
        "helper followed a provider redirect"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn provider_transport_ignores_proxy_environment_and_uses_only_the_explicit_endpoint() {
    let endpoint_listener = loopback();
    let proxy_listener = loopback();
    let endpoint_monitor = accept_once(
        endpoint_listener.try_clone().unwrap(),
        http_response(
            "200 OK",
            openai_response("direct endpoint").to_string().as_bytes(),
            "",
        ),
    );
    let proxy_monitor = accept_once(
        proxy_listener.try_clone().unwrap(),
        http_response(
            "200 OK",
            openai_response("proxy endpoint").to_string().as_bytes(),
            "",
        ),
    );
    let proxy = format!("http://{}", proxy_listener.local_addr().unwrap());
    let args = openai_args(&endpoint(&endpoint_listener, "/v1/responses"));

    let output = run_helper(
        &args,
        b"source",
        &[
            ("OPENAI_API_KEY", "synthetic-test-key"),
            ("HTTP_PROXY", &proxy),
            ("ALL_PROXY", &proxy),
        ],
    );

    assert!(
        output.status.success(),
        "explicit loopback endpoint failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("direct endpoint"));
    assert!(
        endpoint_monitor.join().unwrap(),
        "explicit endpoint was not contacted"
    );
    assert!(
        !proxy_monitor.join().unwrap(),
        "helper routed provider traffic through a proxy"
    );
}

#[test]
fn malformed_endpoint_components_are_rejected_before_any_loopback_request() {
    for suffix in [
        "http://user:pass@127.0.0.1:1234/v1",
        "http://127.0.0.1:1234/v1?token=secret",
        "http://127.0.0.1:1234/v1#fragment",
        "http://example.invalid/v1",
        "http://[::ffff:127.0.0.1]:1234/v1",
        "http://localhost。evil/v1",
    ] {
        let args = openai_args(suffix);
        let output = run_helper(&args, b"short", &[("OPENAI_API_KEY", "synthetic-test-key")]);
        assert!(!output.status.success(), "accepted endpoint {suffix}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn model_generated_shell_syntax_stays_a_literal_argv_value() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let model_manifest: Value =
        serde_json::from_str(include_str!("../manifests/llm.ask.json")).unwrap();
    std::fs::write(
        manifests.join("llm.ask.json"),
        serde_json::to_vec(&model_manifest).unwrap(),
    )
    .unwrap();
    let printf_path = ["/usr/bin/printf", "/bin/printf"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .expect("Unix printf executable");
    let mut printf_manifest: Value =
        serde_json::from_str(include_str!("../manifests/printf.json")).unwrap();
    printf_manifest["binary"] = json!(printf_path);
    std::fs::write(
        manifests.join("printf.json"),
        serde_json::to_vec(&printf_manifest).unwrap(),
    )
    .unwrap();

    let injected_path = directory.path().join("model-injected-command-ran");
    let text = format!(
        "$(touch {}); `touch {}` | * $HOME",
        injected_path.display(),
        injected_path.display()
    );
    let fixture = directory.path().join("literal-output.json");
    std::fs::write(
        &fixture,
        json!({
            "expected_request": {
                "provider": "openai",
                "kind": "generate",
                "model": "model-under-test",
                "state": "",
                "instruction": "draft literal text"
            },
            "response": openai_response(&text)
        })
        .to_string(),
    )
    .unwrap();
    let workflow = directory.path().join("model-output.program");
    std::fs::write(
        &workflow,
        format!(
            "llm.ask with model \"model-under-test\" and instruction \"draft literal text\" and fixture {} as raw\nthen parse json as result\ntake text at \"/text\" from result as answer\nrun printf with message answer\n",
            serde_json::to_string(&fixture.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();

    let output = invoke_execute(directory.path(), &manifests, &[workflow.to_str().unwrap()]);

    assert!(
        output.status.success(),
        "model text did not pass as a literal argument: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, format!("{text}\n").as_bytes());
    assert!(
        !injected_path.exists(),
        "model output executed shell syntax"
    );
}

#[test]
fn duplicate_keys_are_rejected_in_fixture_provider_and_extracted_json() {
    let directory = tempfile::tempdir().unwrap();
    let listener = loopback();
    let monitor = accept_once(
        listener.try_clone().unwrap(),
        http_response(
            "200 OK",
            br#"{"model":"model-under-test","status":"incomplete","status":"completed","error":null,"incomplete_details":null,"output":[{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ambiguous"}]}],"usage":{"input_tokens":1,"output_tokens":1}}"#,
            "",
        ),
    );
    let expected = json!({
        "provider": "openai",
        "kind": "generate",
        "model": "model-under-test",
        "state": "source",
        "instruction": "return a short result"
    });
    let ambiguous_response = r#"{"model":"model-under-test","status":"incomplete","status":"completed","error":null,"incomplete_details":null,"output":[{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ambiguous"}]}],"usage":{"input_tokens":1,"output_tokens":1}}"#;

    let fixture_path = directory.path().join("duplicate-fixture.json");
    let fixture_text = format!(
        "{{\"expected_request\":{},\"response\":{ambiguous_response}}}",
        expected
    );
    std::fs::write(&fixture_path, fixture_text).unwrap();
    let mut fixture_args = openai_args(&endpoint(&listener, "/v1/responses"));
    fixture_args.extend([
        "--fixture".into(),
        fixture_path.to_string_lossy().into_owned(),
    ]);
    let fixture_result = run_helper(&fixture_args, b"source", &[]);
    assert!(
        !fixture_result.status.success(),
        "duplicate fixture response key was accepted"
    );
    assert!(fixture_result.stdout.is_empty());
    assert!(
        !monitor.join().unwrap(),
        "fixture parse opened a loopback socket"
    );

    let provider_listener = loopback();
    let provider_monitor = accept_once(
        provider_listener.try_clone().unwrap(),
        http_response("200 OK", ambiguous_response.as_bytes(), ""),
    );
    let provider_args = openai_args(&endpoint(&provider_listener, "/v1/responses"));
    let provider_result = run_helper(
        &provider_args,
        b"source",
        &[("OPENAI_API_KEY", "synthetic-test-key")],
    );
    assert!(
        !provider_result.status.success(),
        "duplicate provider response key was accepted"
    );
    assert!(provider_result.stdout.is_empty());
    assert!(provider_monitor.join().unwrap());

    let schema = json!({
        "type": "object",
        "properties": { "count": { "type": "integer" } },
        "required": ["count"],
        "additionalProperties": false
    });
    let extract_expected = json!({
        "provider": "openai",
        "kind": "extract",
        "model": "model-under-test",
        "state": "source",
        "instruction": "extract count",
        "schema": schema
    });
    let extract_fixture = directory.path().join("duplicate-extraction.json");
    std::fs::write(
        &extract_fixture,
        json!({
            "expected_request": extract_expected,
            "response": openai_response(r#"{"count":0,"count":1}"#)
        })
        .to_string(),
    )
    .unwrap();
    let mut extract_args = vec![
        "extract".into(),
        "--provider".into(),
        "openai".into(),
        "--model".into(),
        "model-under-test".into(),
        "--instruction".into(),
        "extract count".into(),
        "--schema".into(),
        serde_json::to_string(&schema).unwrap(),
        "--fixture".into(),
        extract_fixture.to_string_lossy().into_owned(),
    ];
    let extraction_result = run_helper(&extract_args, b"source", &[]);
    extract_args.clear();
    assert!(
        !extraction_result.status.success(),
        "duplicate key in structured output was accepted"
    );
    assert!(extraction_result.stdout.is_empty());
}

#[test]
fn duplicate_keys_in_json_input_are_not_silently_reinterpreted_as_text() {
    let args = [
        "generate",
        "--provider",
        "openai",
        "--model",
        "model-under-test",
        "--instruction",
        "return a short result",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let raw_input = br#"{"x":1,"x":2}"#;
    let response = openai_response("safe result");
    let output = run_fixture(
        &args,
        raw_input,
        json!({
            "provider": "openai",
            "kind": "generate",
            "model": "model-under-test",
            "state": "{\"x\":1,\"x\":2}",
            "instruction": "return a short result"
        }),
        response,
    );
    assert!(
        !output.status.success(),
        "ambiguous duplicate-key JSON input was reinterpreted and sent as plain text"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn jev_rejects_unknown_choices_invalid_confidence_and_incomplete_probability_vectors() {
    let expected = json!({
        "provider": "jev",
        "kind": "classify",
        "model": "jev-latest",
        "state": "source",
        "question": "choose",
        "minimum": 0,
        "labels": ["urgent", "other"]
    });
    let args = [
        "classify",
        "--question",
        "choose",
        "--labels",
        "urgent",
        "other",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let cases = [
        json!({
            "model":"jev-latest",
            "answers":{"decision":{"type":"choice","choice":"unknown","confidence":0.9,"probabilities":{"urgent":0.9,"other":0.1}}},
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
        json!({
            "model":"jev-latest",
            "answers":{"decision":{"type":"choice","choice":"urgent","confidence":1.01,"probabilities":{"urgent":0.9,"other":0.1}}},
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
        json!({
            "model":"jev-latest",
            "answers":{"decision":{"type":"choice","choice":"urgent","confidence":0.9,"probabilities":{"urgent":0.9}}},
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
        json!({
            "model":"jev-latest",
            "answers":{"decision":{"type":"choice","choice":"urgent","confidence":0.9,"probabilities":{"urgent":0.2,"other":0.2}}},
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
    ];
    for response in cases {
        let output = run_fixture(&args, b"source", expected.clone(), response);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn openai_incomplete_and_schema_invalid_responses_fail_closed() {
    let generate_args = [
        "generate",
        "--provider",
        "openai",
        "--model",
        "model-under-test",
        "--instruction",
        "return a short result",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let incomplete_expected = json!({
        "provider":"openai","kind":"generate","model":"model-under-test",
        "state":"source","instruction":"return a short result"
    });
    let mut incomplete = openai_response("truncated result");
    incomplete["status"] = json!("incomplete");
    incomplete["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    let output = run_fixture(&generate_args, b"source", incomplete_expected, incomplete);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("incomplete"));

    let schema = json!({
        "type":"object",
        "properties":{"count":{"type":"integer","minimum":1}},
        "required":["count"],
        "additionalProperties":false
    });
    let extract_args = [
        "extract",
        "--provider",
        "openai",
        "--model",
        "model-under-test",
        "--instruction",
        "extract count",
        "--schema",
        r#"{"type":"object","properties":{"count":{"type":"integer","minimum":1}},"required":["count"],"additionalProperties":false}"#,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let output = run_fixture(
        &extract_args,
        b"count is zero",
        json!({
            "provider":"openai","kind":"extract","model":"model-under-test",
            "state":"count is zero","instruction":"extract count","schema":schema
        }),
        openai_response(r#"{"count":0}"#),
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("schema"));
}

#[test]
fn ordinary_model_applet_failure_requires_retry_and_successful_checkpoint_is_not_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let wrapper = json!({
        "name": "synthetic-model",
        "binary": "/bin/sh",
        "accepts_stdin": false,
        "cacheable": false,
        "uncertain_on_failure": true,
        "parameters": {},
        "arguments": [{
            "literal": "-c"
        }, {
            "literal": "count=0; if [ -f calls ]; then count=$(wc -l < calls); fi; printf 'call\\n' >> calls; if [ \"$count\" -eq 0 ]; then exit 1; fi; printf 'retried\\n'"
        }]
    });
    std::fs::write(manifests.join("synthetic-model.json"), wrapper.to_string()).unwrap();
    let workflow = directory.path().join("model.program");
    let checkpoint = directory.path().join("checkpoint");
    std::fs::write(&workflow, "run synthetic-model\n").unwrap();

    let first = invoke_execute(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(
        !first.status.success(),
        "synthetic model's first attempt should fail normally; status={}, stdout={:?}, stderr={}",
        first.status,
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        std::fs::read(directory.path().join("calls")).unwrap(),
        b"call\n"
    );

    let refused = invoke_execute(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("retry-uncertain"));
    assert_eq!(
        std::fs::read(directory.path().join("calls")).unwrap(),
        b"call\n"
    );

    let retried = invoke_execute(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--retry-uncertain",
        ],
    );
    assert!(
        retried.status.success(),
        "explicit retry failed: {}",
        String::from_utf8_lossy(&retried.stderr)
    );
    assert_eq!(retried.stdout, b"retried\n");
    assert_eq!(
        std::fs::read(directory.path().join("calls")).unwrap(),
        b"call\ncall\n"
    );

    let completed_resume = invoke_execute(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(completed_resume.status.success());
    assert_eq!(
        std::fs::read(directory.path().join("calls")).unwrap(),
        b"call\ncall\n"
    );
}

#[test]
fn cancelled_native_model_call_closes_socket_requires_retry_and_completed_resume_skips_it() {
    use std::sync::mpsc::RecvTimeoutError;

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Accepted(usize),
        FirstClientClosed,
    }

    let directory = tempfile::tempdir().unwrap();
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let mut manifest: Value =
        serde_json::from_str(include_str!("../manifests/jev.classify.json")).unwrap();
    manifest["parameters"]["endpoint"] = json!({ "type": "string" });
    manifest["arguments"].as_array_mut().unwrap().push(json!({
        "when_present": "endpoint",
        "append": [{ "literal": "--endpoint" }, { "parameter": "endpoint" }]
    }));
    std::fs::write(
        manifests.join("jev.classify.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let listener = loopback();
    listener.set_nonblocking(true).unwrap();
    let endpoint = endpoint(&listener, "/v1/systemone");
    let stopped = Arc::new(AtomicBool::new(false));
    let request_count = Arc::new(AtomicUsize::new(0));
    let server_stop = Arc::clone(&stopped);
    let server_count = Arc::clone(&request_count);
    let (event_tx, event_rx) = mpsc::channel();
    let answer = json!({
        "model": "jev-latest",
        "answers": {
            "decision": {
                "type": "choice",
                "choice": "urgent",
                "confidence": 0.9,
                "probabilities": { "urgent": 0.9, "other": 0.1 }
            }
        },
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    })
    .to_string();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !server_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let ordinal = server_count.fetch_add(1, Ordering::Relaxed) + 1;
                    let _ = read_headers(&mut stream);
                    let _ = event_tx.send(Event::Accepted(ordinal));
                    if ordinal == 1 {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(100)))
                            .unwrap();
                        let mut byte = [0u8; 1];
                        loop {
                            match stream.read(&mut byte) {
                                Ok(0) => break,
                                Ok(_) => {}
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                    ) => {}
                                Err(_) => break,
                            }
                            if server_stop.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                        let _ = event_tx.send(Event::FirstClientClosed);
                    } else {
                        let body = answer.as_bytes();
                        let _ = stream.write_all(&http_response("200 OK", body, ""));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        server_count.load(Ordering::Relaxed)
    });

    let workflow = directory.path().join("model.program");
    let checkpoint = directory.path().join("checkpoint");
    std::fs::write(
        &workflow,
        format!(
            "jev.classify with question \"Choose a route\" and labels [\"urgent\", \"other\"] and endpoint \"{endpoint}\" as raw\n"
        ),
    )
    .unwrap();

    let mut child = Command::new(EXECUTE)
        .current_dir(directory.path())
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(&manifests)
        .arg("--checkpoint")
        .arg(&checkpoint)
        .arg("--command-timeout-ms")
        .arg("30000")
        .env_clear()
        .env("JEV_API_KEY", "synthetic-test-key")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start checkpointed workflow");
    let accepted = event_rx
        .recv_timeout(Duration::from_secs(4))
        .expect("native execute-model child never sent its local request");
    assert_eq!(accepted, Event::Accepted(1));
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = wait_bounded(&mut child, Duration::from_secs(4))
        .expect("outer CLI did not stop after SIGTERM");
    assert!(!status.success());
    let cancelled = collect_output(&mut child, status);
    assert!(
        event_rx.recv_timeout(Duration::from_secs(2)).unwrap() == Event::FirstClientClosed,
        "cancelled outer CLI left the model request socket open"
    );

    let refused = invoke_execute(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("retry-uncertain"));
    assert!(matches!(
        event_rx.recv_timeout(Duration::from_millis(200)),
        Err(RecvTimeoutError::Timeout)
    ));
    assert_eq!(request_count.load(Ordering::Relaxed), 1);

    let retried = invoke_execute(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--retry-uncertain",
        ],
    );
    assert!(
        retried.status.success(),
        "explicit retry failed: {}",
        String::from_utf8_lossy(&retried.stderr)
    );
    assert_eq!(
        event_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        Event::Accepted(2)
    );

    let completed_resume = invoke_execute(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(
        completed_resume.status.success(),
        "completed model result was not restored: {}",
        String::from_utf8_lossy(&completed_resume.stderr)
    );
    assert_eq!(request_count.load(Ordering::Relaxed), 2);

    stopped.store(true, Ordering::Relaxed);
    assert_eq!(server.join().unwrap(), 2);
    assert!(!cancelled.stderr.is_empty());
}
