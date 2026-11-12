#![cfg(feature = "graph")]

use anyhow::Result;
use flow::ast::Value;
use flow::graph::{
    self, EventEnvelope, GraphRunOptions, GraphSession, PortValue, RuntimeGraphExecutor,
};
use flow::manifest::Registry;
use flow::runtime::RunOptions;
use serde_json::{json, Value as JsonValue};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[path = "common/portable_shell.rs"]
mod portable_shell;

fn applet(
    name: &str,
    binary: &str,
    accepts_stdin: bool,
    parameters: JsonValue,
    arguments: JsonValue,
    contract: JsonValue,
) -> JsonValue {
    json!({
        "name": name,
        "binary": portable_shell::shell_binary(binary),
        "accepts_stdin": accepts_stdin,
        "parameters": parameters,
        "arguments": arguments,
        "contract": contract
    })
}

/// A source that prints a fixture file verbatim, so the bytes it emits are
/// exact and no shell quoting of JSON is involved.
fn file_source(path: &str, contract: JsonValue) -> JsonValue {
    applet(
        "fixture.source",
        "/bin/cat",
        false,
        json!({}),
        json!([{ "literal": path }]),
        contract,
    )
}

/// A sink that echoes its `message` parameter and emits the result as an event.
fn echo_sink(inputs: JsonValue) -> JsonValue {
    applet(
        "fixture.sink",
        "/bin/echo",
        false,
        json!({ "message": { "type": "string", "required": true } }),
        json!([{ "parameter": "message" }]),
        json!({
            "version": 1,
            "output": { "type": "text", "encoding": "raw" },
            "ports": {
                "inputs": inputs,
                "events": { "result": { "schema": { "type": "text" }, "delivery": "events" } }
            }
        }),
    )
}

fn write_fixture(root: &Path, name: &str, content: &str) -> Result<String> {
    let path = root.join(name);
    std::fs::write(&path, content)?;
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn run(root: &Path, manifests: &[JsonValue], source: &str) -> Result<Vec<EventEnvelope>> {
    let sources = manifests
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    let registry = Registry::from_sources(&sources)?;
    let plan = graph::parse(source, &registry)?;
    plan.validate_with_registry(&registry)?;
    let executor = RuntimeGraphExecutor::new(
        registry,
        RunOptions {
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(Duration::from_secs(5)),
            ..RunOptions::new(root.to_path_buf())
        },
    );
    let session = GraphSession::start(plan, Arc::new(executor), GraphRunOptions::default())?;
    Ok(session.run_to_completion()?.outputs)
}

/// Events the sink node published on its result port, in order.
fn sink_results(outputs: &[EventEnvelope]) -> Vec<&PortValue> {
    outputs
        .iter()
        .filter(|event| event.port.node == "sink" && event.port.port == "result")
        .map(|event| &event.payload)
        .collect()
}

fn text_of(payload: &PortValue) -> String {
    match payload {
        PortValue::Value(Value::String(text)) => text.trim_end().to_owned(),
        other => panic!("expected text payload, got {other:?}"),
    }
}

#[test]
fn projected_stdout_port_honors_its_declared_pointer() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = write_fixture(
        root.path(),
        "record.json",
        r#"{"message":"hello","code":7}"#,
    )?;
    let source = file_source(
        &path,
        json!({
            "version": 1,
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": { "message": { "type": "text" }, "code": { "type": "integer" } },
                "required": ["message", "code"]
            },
            "ports": {
                "outputs": {
                    "stdout": { "schema": { "type": "text" }, "delivery": "single", "pointer": "/message" }
                }
            }
        }),
    );
    let sink =
        echo_sink(json!({ "message": { "schema": { "type": "text" }, "delivery": "single" } }));
    let outputs = run(
        root.path(),
        &[source, sink],
        "graph projected_stdout\n  run fixture.source as source\n  run fixture.sink as sink\n  emit result from sink to result\n  bind source.stdout to sink.message with capacity 1 lossless\nend graph\n",
    )?;
    let results = sink_results(&outputs);
    assert_eq!(results.len(), 1, "{outputs:?}");
    assert_eq!(text_of(results[0]), "hello");
    Ok(())
}

#[test]
fn implicit_stdout_port_forwards_the_whole_output() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = write_fixture(root.path(), "text.txt", "hello")?;
    let source = file_source(
        &path,
        json!({ "version": 1, "output": { "type": "text", "encoding": "raw" } }),
    );
    let sink =
        echo_sink(json!({ "message": { "schema": { "type": "text" }, "delivery": "single" } }));
    let outputs = run(
        root.path(),
        &[source, sink],
        "graph implicit_stdout\n  run fixture.source as source\n  run fixture.sink as sink\n  emit result from sink to result\n  bind source.stdout to sink.message with capacity 1 lossless\nend graph\n",
    )?;
    let results = sink_results(&outputs);
    assert_eq!(results.len(), 1, "{outputs:?}");
    assert_eq!(text_of(results[0]), "hello");
    Ok(())
}

#[test]
fn named_bytes_port_emits_raw_output_without_a_decoded_value() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = write_fixture(root.path(), "blob.bin", "raw-bytes")?;
    let source = applet(
        "fixture.blob",
        "/bin/cat",
        false,
        json!({}),
        json!([{ "literal": path }]),
        json!({
            "version": 1,
            "output": { "type": "bytes", "encoding": "raw" },
            "ports": {
                "outputs": {
                    "blob": { "schema": { "type": "bytes" }, "delivery": "single" }
                }
            }
        }),
    );
    let sink = applet(
        "fixture.bytes_sink",
        "/bin/cat",
        true,
        json!({}),
        json!([]),
        json!({
            "version": 1,
            "output": { "type": "bytes", "encoding": "raw" },
            "ports": {
                "inputs": {
                    "stdin": { "schema": { "type": "bytes" }, "delivery": "single", "encoding": "raw" }
                },
                "events": { "result": { "schema": { "type": "bytes" }, "delivery": "events" } }
            }
        }),
    );
    let outputs = run(
        root.path(),
        &[source, sink],
        "graph named_bytes\n  run fixture.blob as source\n  run fixture.bytes_sink as sink\n  emit result from sink to result\n  bind source.blob to sink.stdin with capacity 1 lossless\nend graph\n",
    )?;
    let results = sink_results(&outputs);
    assert_eq!(results.len(), 1, "{outputs:?}");
    assert_eq!(results[0], &PortValue::Bytes(b"raw-bytes".to_vec()));
    Ok(())
}

#[test]
fn records_port_delivers_each_list_item_to_a_records_consumer() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = write_fixture(root.path(), "items.json", r#"["one","two"]"#)?;
    let source = file_source(
        &path,
        json!({
            "version": 1,
            "output": { "type": "list", "encoding": "json", "items": { "type": "text" } },
            "ports": {
                "outputs": {
                    "items": { "schema": { "type": "text" }, "delivery": "records" }
                }
            }
        }),
    );
    let sink =
        echo_sink(json!({ "message": { "schema": { "type": "text" }, "delivery": "records" } }));
    let outputs = run(
        root.path(),
        &[source, sink],
        "graph list_records\n  run fixture.source as source\n  run fixture.sink as sink\n  emit result from sink to result\n  bind source.items to sink.message with capacity 4 lossless\nend graph\n",
    )?;
    let texts = sink_results(&outputs)
        .into_iter()
        .map(text_of)
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        vec!["one".to_owned(), "two".to_owned()],
        "{outputs:?}"
    );
    Ok(())
}
