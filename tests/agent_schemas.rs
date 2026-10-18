//! Conformance: the published JSON Schemas in `docs/schemas/` accept the output
//! the real `execute` binary prints, and reject damaged documents. The checker
//! implements only the keywords these schemas use: type, const, enum, required,
//! properties, additionalProperties (false), items, minimum, maximum, oneOf.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const HELPER: &str = env!("CARGO_BIN_EXE_execute-model");

fn project() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn load_schema(relative: &str) -> Value {
    let text = std::fs::read_to_string(project().join(relative))
        .unwrap_or_else(|error| panic!("reading {relative}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parsing {relative}: {error}"))
}

fn matches_type(instance: &Value, kind: &str) -> bool {
    match kind {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "integer" => instance.is_i64() || instance.is_u64(),
        "number" => instance.is_number(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        _ => false,
    }
}

/// Collect every violation, so a failure names all of them at once.
fn violations(schema: &Value, instance: &Value, path: &str, errors: &mut Vec<String>) {
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = branches
            .iter()
            .filter(|branch| {
                let mut branch_errors = Vec::new();
                violations(branch, instance, path, &mut branch_errors);
                branch_errors.is_empty()
            })
            .count();
        if matching != 1 {
            errors.push(format!(
                "{path}: matches {matching} oneOf branches, expected exactly 1"
            ));
        }
    }
    if let Some(constant) = schema.get("const") {
        if constant != instance {
            errors.push(format!("{path}: expected {constant}"));
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(instance) {
            errors.push(format!("{path}: {instance} is not an allowed value"));
        }
    }
    if let Some(kind) = schema.get("type") {
        let kinds: Vec<&str> = match kind {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !kinds.iter().any(|name| matches_type(instance, name)) {
            errors.push(format!("{path}: expected type {kind}"));
            return;
        }
    }
    if let (Some(minimum), Some(number)) = (
        schema.get("minimum").and_then(Value::as_f64),
        instance.as_f64(),
    ) {
        if number < minimum {
            errors.push(format!("{path}: {number} is below {minimum}"));
        }
    }
    if let (Some(maximum), Some(number)) = (
        schema.get("maximum").and_then(Value::as_f64),
        instance.as_f64(),
    ) {
        if number > maximum {
            errors.push(format!("{path}: {number} is above {maximum}"));
        }
    }
    match instance {
        Value::Object(map) => {
            for name in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = name.as_str() {
                    if !map.contains_key(name) {
                        errors.push(format!("{path}: missing required property {name}"));
                    }
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
            for (key, value) in map {
                match properties.and_then(|known| known.get(key)) {
                    Some(sub) => violations(sub, value, &format!("{path}/{key}"), errors),
                    None if closed => errors.push(format!("{path}: unexpected property {key}")),
                    None => {}
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    violations(item_schema, item, &format!("{path}/{index}"), errors);
                }
            }
        }
        _ => {}
    }
}

fn assert_conforms(schema_file: &str, instance: &Value) {
    let schema = load_schema(schema_file);
    let mut errors = Vec::new();
    violations(&schema, instance, "", &mut errors);
    assert!(
        errors.is_empty(),
        "{schema_file} rejected real output: {errors:#?}\n{instance:#}"
    );
}

fn assert_rejects(schema_file: &str, damaged: &Value) {
    let schema = load_schema(schema_file);
    let mut errors = Vec::new();
    violations(&schema, damaged, "", &mut errors);
    assert!(
        !errors.is_empty(),
        "{schema_file} accepted a damaged document: {damaged:#}"
    );
}

// --- real CLI runs -----------------------------------------------------------

fn write_manifests(directory: &Path) -> PathBuf {
    let manifests = directory.join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    let applets = [
        json!({"name": "ok.helper", "binary": HELPER, "accepts_stdin": false, "parameters": {}, "arguments": [{"literal": "--version"}]}),
        json!({"name": "fail.helper", "binary": HELPER, "accepts_stdin": false, "parameters": {}, "arguments": [{"literal": "--nope"}]}),
        json!({"name": "plain.helper", "binary": HELPER, "accepts_stdin": false, "parameters": {"text": {"type": "string"}},
               "arguments": [{"literal": "--version"}, {"when_present": "text", "append": [{"literal": "--text"}, {"parameter": "text"}]}]}),
        json!({"name": "net.helper", "binary": HELPER, "accepts_stdin": false, "parameters": {}, "arguments": [{"literal": "--version"}],
               "contract": {"version": 1, "backend": "command", "lifecycle": {"kind": "oneshot"},
                            "capabilities": ["network.loopback"], "effects": ["network.loopback"]}}),
        json!({"name": "cd", "binary": "cd", "builtin": "cd", "parameters": {"path": {"type": "string", "required": true}},
               "arguments": [{"parameter": "path"}]}),
    ];
    for applet in applets {
        let name = applet["name"].as_str().unwrap().to_owned();
        std::fs::write(manifests.join(format!("{name}.json")), applet.to_string()).unwrap();
    }
    manifests
}

fn run_execute(directory: &Path, arguments: &[&str]) -> Output {
    let manifests = write_manifests(directory);
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.args(arguments);
    if !arguments.contains(&"checkpoint") {
        command.arg("--manifest-dir").arg(&manifests);
    }
    command
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("start execute")
}

fn last_json_line(stderr: &[u8]) -> Value {
    let text = String::from_utf8(stderr.to_vec()).unwrap();
    let last = text.lines().last().expect("stderr has a final line");
    serde_json::from_str(last).expect("final stderr line is JSON")
}

// --- schemas -----------------------------------------------------------------

#[test]
fn error_schema_accepts_real_structured_errors_and_rejects_damage() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("fail.program"), "run fail.helper\n").unwrap();
    let command_failure = run_execute(directory.path(), &["fail.program", "--json-errors"]);
    let command_object = last_json_line(&command_failure.stderr);
    assert_conforms("docs/schemas/error.v1.schema.json", &command_object);

    std::fs::write(directory.path().join("bad.program"), "then then then\n").unwrap();
    let parse_failure = run_execute(directory.path(), &["bad.program", "--json-errors"]);
    let parse_object = last_json_line(&parse_failure.stderr);
    assert_conforms("docs/schemas/error.v1.schema.json", &parse_object);

    let mut unknown_code = command_object.clone();
    unknown_code["code"] = json!("made_up_code");
    assert_rejects("docs/schemas/error.v1.schema.json", &unknown_code);
    let mut extra = command_object.clone();
    extra["extra"] = json!(true);
    assert_rejects("docs/schemas/error.v1.schema.json", &extra);
    let mut missing = command_object;
    missing.as_object_mut().unwrap().remove("exit_code");
    assert_rejects("docs/schemas/error.v1.schema.json", &missing);
}

#[test]
fn checkpoint_status_schema_accepts_completed_and_failed_checkpoints() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("ok.program"), "run ok.helper\n").unwrap();
    std::fs::write(directory.path().join("fail.program"), "run fail.helper\n").unwrap();
    let ok_checkpoint = directory.path().join("ok-ckpt");
    let fail_checkpoint = directory.path().join("fail-ckpt");
    let ok_text = ok_checkpoint.to_string_lossy().into_owned();
    let fail_text = fail_checkpoint.to_string_lossy().into_owned();
    assert!(
        run_execute(directory.path(), &["ok.program", "--checkpoint", &ok_text])
            .status
            .success()
    );
    assert!(!run_execute(
        directory.path(),
        &["fail.program", "--checkpoint", &fail_text]
    )
    .status
    .success());

    for checkpoint in [&ok_text, &fail_text] {
        let output = run_execute(
            directory.path(),
            &["checkpoint", "status", checkpoint, "--json"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let object: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_conforms("docs/schemas/checkpoint-status.v1.schema.json", &object);
        let mut verified = object.clone();
        verified["journal"]["checksum_verified"] = json!(true);
        assert_rejects("docs/schemas/checkpoint-status.v1.schema.json", &verified);
        let mut action = object;
        action["resume_action"] = json!("resume_anything");
        assert_rejects("docs/schemas/checkpoint-status.v1.schema.json", &action);
    }
}

#[test]
fn explain_schema_accepts_a_rich_plan_and_rejects_damage() {
    let directory = tempfile::tempdir().unwrap();
    let source = "define a program named inner\n  run net.helper\n\
accept target as text\n\
use folder \"sub\"\n\
run net.helper and environment {\"PATH\": \"/tmp/bin\"}\n\
run plain.helper\n\
run plain.helper\n\
then save to \"out.txt\"\n\
then append to \"../../escape.txt\"\n\
run program inner\n";
    std::fs::write(directory.path().join("plan.program"), source).unwrap();
    let output = run_execute(directory.path(), &["explain", "plan.program", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let object: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_conforms("docs/schemas/explain.v1.schema.json", &object);

    for (field, value) in [
        ("program_stdout", json!(true)),
        ("binding", json!(7)),
        ("enforcement", json!("unchecked")),
    ] {
        let mut damaged = object.clone();
        damaged["steps"][0][field] = value;
        assert_rejects("docs/schemas/explain.v1.schema.json", &damaged);
    }
    let mut damaged = object.clone();
    damaged["execution_mode"] = json!("unchecked");
    assert_rejects("docs/schemas/explain.v1.schema.json", &damaged);

    let mut without_undeclared = object.clone();
    without_undeclared["steps"][0]
        .as_object_mut()
        .unwrap()
        .remove("undeclared");
    assert_rejects("docs/schemas/explain.v1.schema.json", &without_undeclared);
    let mut unknown_effect_type = object.clone();
    unknown_effect_type["summary"]["effects_declared"] = json!("yes");
    assert_rejects("docs/schemas/explain.v1.schema.json", &unknown_effect_type);
    let mut extra_summary = object;
    extra_summary["summary"]["surprise"] = json!(1);
    assert_rejects("docs/schemas/explain.v1.schema.json", &extra_summary);
}

#[test]
fn published_check_schema_accepts_real_check_output() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("ok.program"), "run ok.helper\n").unwrap();
    let output = run_execute(directory.path(), &["check", "ok.program", "--json"]);
    let object: Value = serde_json::from_slice(&output.stdout).expect("check --json object");
    assert_conforms("docs/check-result.schema.json", &object);
}
