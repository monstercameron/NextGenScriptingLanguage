#![cfg(unix)]

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

fn event_schema(schema: JsonValue) -> JsonValue {
    json!({
        "events": {
            "result": {
                "schema": schema,
                "delivery": "events"
            }
        }
    })
}

fn command_manifest(
    name: &str,
    binary: &str,
    arguments: JsonValue,
    output: Option<JsonValue>,
    events: JsonValue,
    extra_contract: JsonValue,
) -> JsonValue {
    let mut contract = json!({
        "version": 1,
        "ports": events
    });
    if let Some(output) = output {
        contract["output"] = output;
    }
    if let Some(extra) = extra_contract.as_object() {
        for (name, value) in extra {
            contract[name] = value.clone();
        }
    }
    json!({
        "name": name,
        "binary": binary,
        "accepts_stdin": false,
        "parameters": {},
        "arguments": arguments,
        "contract": contract
    })
}

fn run_projection(root: &Path, manifest: JsonValue, operation: &str) -> Result<Vec<EventEnvelope>> {
    let manifest = serde_json::to_string(&manifest)?;
    let registry = Registry::from_sources(&[manifest])?;
    let source = format!(
        "graph projection\n  run {operation} as mapped\n  emit result from mapped to result\nend graph\n"
    );
    let plan = graph::parse(&source, &registry)?;
    plan.validate_with_registry(&registry)?;
    let executor = RuntimeGraphExecutor::new(
        registry,
        RunOptions {
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(Duration::from_secs(3)),
            ..RunOptions::new(root.to_path_buf())
        },
    );
    let session = GraphSession::start(plan, Arc::new(executor), GraphRunOptions::default())?;
    Ok(session.run_to_completion()?.outputs)
}

#[test]
fn raw_bytes_and_list_results_are_each_one_event() -> Result<()> {
    let root = tempfile::tempdir()?;
    let bytes_manifest = command_manifest(
        "fixture.raw",
        "/bin/echo",
        json!([{ "literal": "raw-result" }]),
        None,
        event_schema(json!({ "type": "bytes" })),
        json!({}),
    );
    let bytes = run_projection(root.path(), bytes_manifest, "fixture.raw")?;
    assert_eq!(bytes.len(), 1);
    assert_eq!(bytes[0].payload, PortValue::Bytes(b"raw-result\n".to_vec()));
    assert_eq!(bytes[0].sequence, 1);

    let list_schema = json!({
        "type": "list",
        "items": { "type": "text" }
    });
    let list_manifest = command_manifest(
        "fixture.list",
        "/bin/echo",
        json!([{ "literal": "[\"one\",\"two\"]" }]),
        Some(json!({
            "type": "list",
            "encoding": "json",
            "items": { "type": "text" }
        })),
        event_schema(list_schema),
        json!({}),
    );
    let list = run_projection(root.path(), list_manifest, "fixture.list")?;
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0].payload,
        PortValue::Value(Value::List(vec![
            Value::String("one".into()),
            Value::String("two".into())
        ]))
    );
    assert_eq!(list[0].sequence, 1);
    Ok(())
}

#[test]
fn failed_non_enveloped_results_emit_no_event_and_retain_effect_certainty() -> Result<()> {
    let root = tempfile::tempdir()?;
    let manifest = command_manifest(
        "fixture.failure",
        "/bin/sh",
        json!([{ "literal": "-c" }, { "literal": "exit 7" }]),
        Some(json!({ "type": "text", "encoding": "raw" })),
        event_schema(json!({ "type": "text" })),
        json!({
            "outcomes": [
                { "status": 0, "name": "ok", "kind": "success" },
                { "status": 7, "name": "failed", "kind": "failure" }
            ]
        }),
    );
    let raw = serde_json::to_string(&manifest)?;
    let registry = Registry::from_sources(&[raw])?;
    let plan = graph::parse(
        "graph failed_projection\n  run fixture.failure as mapped\n  emit result from mapped to result\nend graph\n",
        &registry,
    )?;
    plan.validate_with_registry(&registry)?;
    let executor = RuntimeGraphExecutor::new(
        registry,
        RunOptions {
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(Duration::from_secs(3)),
            ..RunOptions::new(root.path().to_path_buf())
        },
    );
    let session = GraphSession::start(plan, Arc::new(executor), GraphRunOptions::default())?;
    let error = session.run_to_completion().unwrap_err();
    assert!(format!("{error:#}").contains("outcome (7)"));
    assert_eq!(
        flow::runtime::execution_result::effect_certainty(&error),
        flow::contracts::EffectCertainty::Uncertain
    );
    Ok(())
}

#[test]
fn failure_result_envelope_is_emitted_as_one_typed_event() -> Result<()> {
    let root = tempfile::tempdir()?;
    let envelope_schema = json!({
        "type": "record",
        "fields": {
            "value": { "type": "text", "nullable": true },
            "outcome": { "type": "text" },
            "kind": { "type": "text" },
            "effect_certainty": { "type": "text" },
            "status": { "type": "integer" }
        },
        "required": ["value", "outcome", "kind", "effect_certainty", "status"]
    });
    let manifest = command_manifest(
        "fixture.envelope",
        "/bin/sh",
        json!([{ "literal": "-c" }, { "literal": "exit 7" }]),
        Some(json!({ "type": "text", "encoding": "raw" })),
        event_schema(envelope_schema),
        json!({
            "result_envelope": true,
            "outcomes": [
                { "status": 0, "name": "ok", "kind": "success" },
                { "status": 7, "name": "failed", "kind": "failure" }
            ]
        }),
    );
    let events = run_projection(root.path(), manifest, "fixture.envelope")?;
    assert_eq!(events.len(), 1);
    let PortValue::Value(Value::Record(envelope)) = &events[0].payload else {
        panic!("projection must preserve the typed envelope as one value");
    };
    assert_eq!(envelope.get("value"), Some(&Value::Null));
    assert_eq!(
        envelope.get("outcome"),
        Some(&Value::String("failed".into()))
    );
    assert_eq!(envelope.get("kind"), Some(&Value::String("failure".into())));
    assert_eq!(
        envelope.get("effect_certainty"),
        Some(&Value::String("uncertain".into()))
    );
    assert_eq!(envelope.get("status"), Some(&Value::Integer(7)));
    Ok(())
}

#[test]
fn event_schema_mismatch_is_rejected_before_the_applet_can_make_effects() -> Result<()> {
    let root = tempfile::tempdir()?;
    let marker = root.path().join("should-not-exist");
    let manifest = command_manifest(
        "fixture.touch",
        "/usr/bin/touch",
        json!([{ "literal": marker.to_string_lossy() }]),
        Some(json!({
            "type": "record",
            "encoding": "json",
            "fields": { "text": { "type": "text" } },
            "required": ["text"]
        })),
        event_schema(json!({ "type": "text" })),
        json!({}),
    );
    let raw = serde_json::to_string(&manifest)?;
    let registry = Registry::from_sources(&[raw])?;
    let error = graph::parse(
        "graph invalid_projection\n  run fixture.touch as mapped\n  emit result from mapped to result\nend graph\n",
        &registry,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("canonical result schema"));
    assert!(!marker.exists());
    Ok(())
}
