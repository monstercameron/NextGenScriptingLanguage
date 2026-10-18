#![cfg(unix)]

use flow::{manifest::Registry, parser, runtime};
use std::time::Duration;

#[test]
fn applet_listener_uses_its_child_scoped_environment_overlay() {
    let directory = tempfile::tempdir().unwrap();
    let variable = "EXECUTE_SERVICE_LISTENER_CANARY";
    let source = serde_json::json!({
        "name": "fixture.listener",
        "binary": "/bin/sh",
        "arguments": [
            {"literal": "-c"},
            {"literal": format!("printf '%s\\n' \"${variable}\"")}
        ],
        "contract": {
            "version": 1,
            "environment": {"set": {(variable): "listener-value"}}
        }
    });
    let registry = Registry::from_sources(&[source.to_string()]).unwrap();
    let workflow = parser::parse(
        "listen to fixture.listener\nwhenever a line arrives as event\n  then \"cat\"\n",
    )
    .unwrap();
    let options = runtime::RunOptions {
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(2)),
        total_timeout: Some(Duration::from_secs(2)),
        ..runtime::RunOptions::new(directory.path().to_owned())
    };
    let mut output = Vec::new();
    let result = runtime::run_service(
        &workflow,
        &registry,
        &options,
        &runtime::ServiceLimits {
            max_runs: Some(1),
            max_record_bytes: 1024,
        },
        |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(output, b"listener-value\n");
    assert_eq!(result.runs, 1);
}
