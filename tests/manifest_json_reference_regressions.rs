//! Typed integer outputs are JSON values and can flow into JSON parameters.
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

fn fixture() -> &'static str {
    env!("CARGO_BIN_EXE_native-fixture")
}

fn write_manifest(directory: &Path, name: &str, value: Value) {
    fs::write(
        directory.join(format!("{name}.json")),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}

fn command(root: &Path, action: &str, source: &str) -> std::process::Output {
    fs::write(root.join("case.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(root)
        .args([action, "case.program", "--manifest-dir"])
        .arg(root.join("manifests"))
        .output()
        .unwrap()
}

fn producer(name: &str, output: Value, serialized_value: String) -> Value {
    json!({
        "name": name,
        "binary": fixture(),
        "arguments": [{"literal":"write"}, {"literal":serialized_value}],
        "contract": {
            "version": 1,
            "backend": "command",
            "output": output
        }
    })
}

fn consumer(name: &str, parameter: Value, input_schema: Value) -> Value {
    json!({
        "name": name,
        "binary": fixture(),
        "parameters": {"payload": parameter},
        "arguments": [{"literal":"write"}, {"json_arguments":true}],
        "contract": {
            "version": 1,
            "backend": "command",
            "output": {"type":"json", "encoding":"json"},
            "ports": {
                "inputs": {
                    "payload": {"schema":input_schema, "delivery":"single"}
                }
            }
        }
    })
}

#[test]
fn unsigned_and_usize_scalar_and_list_results_flow_into_json_parameters() {
    let root = tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    let usize_max = usize::MAX as u64;
    let cases = [
        (
            "scalar_unsigned",
            "issue.scalar_unsigned",
            json!({"type":"unsigned_integer", "encoding":"json"}),
            json!(u64::MAX),
            "issue.consume_scalar",
            json!({"type":"json", "required":true}),
            json!({"type":"json", "scalar_only":true}),
            json!({"payload":u64::MAX}),
        ),
        (
            "scalar_usize",
            "issue.scalar_usize",
            json!({"type":"usize", "encoding":"json"}),
            json!(usize_max),
            "issue.consume_scalar",
            json!({"type":"json", "required":true}),
            json!({"type":"json", "scalar_only":true}),
            json!({"payload":usize_max}),
        ),
        (
            "list_unsigned",
            "issue.list_unsigned",
            json!({"type":"list", "encoding":"json", "items":{"type":"unsigned_integer"}}),
            json!([0, u64::MAX]),
            "issue.consume_list",
            json!({"type":"list", "items":"json", "required":true}),
            json!({"type":"list", "items":{"type":"json"}}),
            json!({"payload":[0, u64::MAX]}),
        ),
        (
            "list_usize",
            "issue.list_usize",
            json!({"type":"list", "encoding":"json", "items":{"type":"usize"}}),
            json!([0, usize_max]),
            "issue.consume_list",
            json!({"type":"list", "items":"json", "required":true}),
            json!({"type":"list", "items":{"type":"json"}}),
            json!({"payload":[0, usize_max]}),
        ),
    ];

    for (file, producer_name, output, value, consumer_name, parameter, schema, expected) in cases {
        write_manifest(
            &manifests,
            file,
            producer(
                producer_name,
                output,
                serde_json::to_string(&value).unwrap(),
            ),
        );
        let consumer = consumer(consumer_name, parameter, schema);
        write_manifest(&manifests, consumer_name, consumer);
        let source = format!(
            "run {producer_name} as typed\nrun {consumer_name} with payload typed as result\nthen encode result as json\n"
        );

        let check = command(root.path(), "check", &source);
        assert!(
            check.status.success(),
            "check rejected {producer_name}: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        let run = command(root.path(), "run", &source);
        assert!(
            run.status.success(),
            "run rejected {producer_name}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let actual: Value = serde_json::from_slice(&run.stdout).unwrap();
        assert_eq!(actual, expected, "{producer_name}");
    }
}

#[test]
fn non_json_parameter_types_still_reject_unsigned_references() {
    let root = tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    write_manifest(
        &manifests,
        "scalar",
        producer(
            "issue.scalar_unsigned",
            json!({"type":"unsigned_integer", "encoding":"json"}),
            "5".into(),
        ),
    );
    write_manifest(
        &manifests,
        "text",
        json!({
            "name":"issue.consume_text",
            "binary":fixture(),
            "parameters":{"payload":{"type":"string", "required":true}},
            "arguments":[{"literal":"write"}, {"parameter":"payload"}]
        }),
    );

    let check = command(
        root.path(),
        "check",
        "run issue.scalar_unsigned as typed\nrun issue.consume_text with payload typed\n",
    );
    assert!(!check.status.success());
    assert!(
        String::from_utf8_lossy(&check.stderr).contains("expects string"),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
}

#[test]
fn scalar_json_input_schema_still_checks_shape_after_reference_resolution() {
    let root = tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    write_manifest(
        &manifests,
        "scalar",
        producer(
            "issue.json_object",
            json!({"type":"json", "encoding":"json"}),
            r#"{"nested":true}"#.into(),
        ),
    );
    write_manifest(
        &manifests,
        "scalar_consumer",
        consumer(
            "issue.scalar_consumer",
            json!({"type":"json", "required":true}),
            json!({"type":"json", "scalar_only":true}),
        ),
    );
    let source = "run issue.json_object as typed\nrun issue.scalar_consumer with payload typed\n";
    let check = command(root.path(), "check", source);
    assert!(
        check.status.success(),
        "static check should defer JSON shape: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    let run = command(root.path(), "run", source);
    assert!(
        !run.status.success(),
        "runtime skipped the scalar-only schema"
    );
    let error = String::from_utf8_lossy(&run.stderr);
    assert!(
        error.contains("scalar") || error.contains("schema"),
        "{error}"
    );
}
