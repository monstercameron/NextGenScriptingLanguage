use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn invoke(arguments: &[&str], input: &str, fixture: &Value) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.json");
    std::fs::write(&path, serde_json::to_vec(fixture).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute-model"))
        .args(arguments)
        .arg("--fixture")
        .arg(path)
        .env("EXECUTE_MODELS_OFFLINE", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn fixture(expected_request: Value, response: Value) -> Value {
    json!({"expected_request": expected_request, "response": response})
}

fn normalized(output: Output) -> Value {
    assert!(
        output.status.success(),
        "unexpected failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn offline_environment_blocks_fixture_omitted_requests_before_credentials() {
    let output = Command::new(env!("CARGO_BIN_EXE_execute-model"))
        .args([
            "generate",
            "--provider",
            "openai",
            "--model",
            "gpt-6-luna",
            "--instruction",
            "Do not send a request",
            "--endpoint",
            "http://127.0.0.1:1/v1/responses",
        ])
        .env("EXECUTE_MODELS_OFFLINE", "")
        .env_remove("OPENAI_API_KEY")
        .env_remove("openai")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"execute-model: offline\n");
}

#[test]
fn extraction_rejects_near_multiple_values_without_epsilon() {
    let schema = json!({
        "type":"object",
        "properties":{"value":{"type":"number","multipleOf":1}},
        "required":["value"],
        "additionalProperties":false
    });
    let schema_text = serde_json::to_string(&schema).unwrap();
    let output = invoke(
        &[
            "extract",
            "--provider",
            "openai",
            "--model",
            "gpt-6-luna",
            "--instruction",
            "Extract an integer",
            "--schema",
            &schema_text,
        ],
        "source",
        &fixture(
            json!({
                "provider":"openai","kind":"extract","model":"gpt-6-luna",
                "state":"source","instruction":"Extract an integer","schema":schema
            }),
            json!({
                "id":"resp-test","object":"response","status":"completed","model":"gpt-fixture-model",
                "output":[{"id":"msg-test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"{\"value\":1.0000000005}","annotations":[]}]}],
                "usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}
            }),
        ),
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"execute-model: schema\n");
}

#[test]
fn provider_fixtures_normalize_each_supported_operation() {
    let classify = normalized(invoke(
        &[
            "classify",
            "--question",
            "Is this urgent?",
            "--labels",
            "urgent",
            "other",
            "--minimum",
            "80",
        ],
        r#"{"body":"need help"}"#,
        &fixture(
            json!({
                "provider":"jev","kind":"classify","model":"jev-latest",
                "state":{"body":"need help"},"question":"Is this urgent?",
                "minimum":80,"labels":["urgent","other"]
            }),
            json!({
                "model":"jev-latest",
                "answers":{"decision":{"type":"choice","choice":"urgent","confidence":0.79,"probabilities":{"urgent":0.79,"other":0.21}}},
                "usage":{"input_tokens":3,"output_tokens":2}
            }),
        ),
    ));
    assert_eq!(classify["label"], "urgent");
    assert_eq!(classify["confidence_percent"], 79);
    assert_eq!(classify["accepted"], false);
    assert_eq!(classify["transport"], "fixture");

    let judge = normalized(invoke(
        &["judge", "--question", "Is it true?", "--minimum", "80"],
        "claim",
        &fixture(
            json!({
                "provider":"jev","kind":"judge","model":"jev-latest",
                "state":"claim","question":"Is it true?","minimum":80
            }),
            json!({
                "model":"jev-latest","answers":{"decision":{"type":"noul","noul":0.2}},
                "usage":{"input_tokens":4,"output_tokens":1}
            }),
        ),
    ));
    assert_eq!(judge["decision"], false);
    assert_eq!(judge["confidence_percent"], 80);
    assert_eq!(judge["accepted"], true);

    let score = normalized(invoke(
        &[
            "score",
            "--question",
            "Rate quality",
            "--levels",
            "low",
            "medium",
            "high",
        ],
        "answer",
        &fixture(
            json!({
                "provider":"jev","kind":"score","model":"jev-latest",
                "state":"answer","question":"Rate quality","minimum":0,
                "levels":["low","medium","high"]
            }),
            json!({
                "model":"jev-latest",
                "answers":{"decision":{"type":"score","score":1.5,"confidence":0.85,"legend":{"0":"low","1":"medium","2":"high"},"probabilities":{"0":0.1,"1":0.8,"2":0.1}}},
                "usage":{"input_tokens":6,"output_tokens":3}
            }),
        ),
    ));
    assert_eq!(score["score"], 1.5);
    assert_eq!(score["confidence_percent"], 85);

    let generated = normalized(invoke(
        &[
            "generate",
            "--provider",
            "openai",
            "--model",
            "gpt-6-luna",
            "--instruction",
            "Draft result",
        ],
        "source",
        &fixture(
            json!({
                "provider":"openai","kind":"generate","model":"gpt-6-luna",
                "state":"source","instruction":"Draft result"
            }),
            json!({
                "id":"resp-test","object":"response","status":"completed","model":"gpt-fixture-model",
                "output":[{"id":"msg-test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Draft result","annotations":[]}]}],
                "usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}
            }),
        ),
    ));
    assert_eq!(generated["kind"], "generate");
    assert_eq!(generated["model"], "gpt-fixture-model");
    assert_eq!(generated["text"], "Draft result");

    let schema = json!({
        "type":"object",
        "properties":{"name":{"type":"string"},"count":{"type":"integer","minimum":1}},
        "required":["name","count"],
        "additionalProperties":false
    });
    let extracted = normalized(invoke(
        &[
            "extract",
            "--provider",
            "openai",
            "--model",
            "gpt-6-luna",
            "--instruction",
            "Extract fields",
            "--schema",
            r#"{"type":"object","properties":{"name":{"type":"string"},"count":{"type":"integer","minimum":1}},"required":["name","count"],"additionalProperties":false}"#,
        ],
        "Ada has 2 things",
        &fixture(
            json!({
                "provider":"openai","kind":"extract","model":"gpt-6-luna",
                "state":"Ada has 2 things","instruction":"Extract fields","schema":schema
            }),
            json!({
                "id":"resp-test","object":"response","status":"completed","model":"gpt-fixture-model",
                "output":[{"id":"msg-test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"{\"name\":\"Ada\",\"count\":2}","annotations":[]}]}],
                "usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}
            }),
        ),
    ));
    assert_eq!(extracted["value"], json!({"name":"Ada","count":2}));
}

#[test]
fn refusal_is_a_static_sanitized_failure() {
    let output = invoke(
        &[
            "generate",
            "--provider",
            "openai",
            "--model",
            "fixture-model",
            "--instruction",
            "private prompt text",
        ],
        "private input text",
        &fixture(
            json!({
                "provider":"openai","kind":"generate","model":"fixture-model",
                "state":"private input text","instruction":"private prompt text"
            }),
            json!({
                "status":"completed","model":"fixture-model",
                "output":[{"type":"message","role":"assistant","status":"completed","content":[{"type":"refusal","refusal":"private refusal text"}]}],
                "usage":{"input_tokens":1,"output_tokens":1}
            }),
        ),
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.trim(), "execute-model: refusal");
    assert!(!stderr.contains("private"));
}
