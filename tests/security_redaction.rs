#![cfg(unix)]

use flow::{manifest::Registry, parser, runtime};
use serde_json::json;
use std::time::Duration;

#[test]
fn failed_child_echoing_declared_environment_secret_redacts_errors_but_keeps_raw_capture() {
    let directory = tempfile::tempdir().unwrap();
    let sentinel = "fixture-secret-that-must-not-appear-in-errors";
    let source = json!({
        "name": "secret.echo",
        "binary": "/bin/sh",
        "arguments": [
            {"literal": "-c"},
            {"literal": "printf '%s' \"$FLOW_SENTINEL\" >&2; exit 7"}
        ],
        "stdio": {"stderr": "capture"},
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "environment": {"set": {"FLOW_SENTINEL": sentinel}}
        }
    });
    let registry = Registry::from_sources(&[source.to_string()]).unwrap();
    registry.set_host_state(
        flow::security::DataFlowPolicy::from_json(
            br#"{"version":1,"secrets":{"environment":{"secret.echo":["FLOW_SENTINEL"]}}}"#,
        )
        .unwrap(),
    );
    let program = parser::parse("run secret.echo\n").unwrap();
    let options = runtime::RunOptions {
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(5)),
        ..runtime::RunOptions::new(directory.path().to_owned())
    };

    let error = match runtime::run(&program, &registry, &options) {
        Err(error) => error,
        Ok(_) => panic!("child should have returned a non-success status"),
    };
    let failure = error
        .downcast_ref::<runtime::CommandFailed>()
        .expect("native child failure remains typed");
    assert!(failure.diagnostic.contains("[REDACTED]"));
    assert!(!failure.diagnostic.contains(sentinel));
    assert_eq!(failure.stderr.as_deref(), Some(sentinel.as_bytes()));
    let display = format!("{error:#}");
    assert!(display.contains("[REDACTED]"));
    assert!(!display.contains(sentinel));

    let stream = flow::stream::parse("stream from secret.echo\nshow each line\n").unwrap();
    let stream_error =
        match runtime::run_stream(&stream, &registry, &options, 1024, |_, _, _| Ok(())) {
            Err(error) => error,
            Ok(_) => panic!("stream child should have returned a non-success status"),
        };
    let stream_failure = stream_error
        .downcast_ref::<runtime::CommandFailed>()
        .expect("stream child failure remains typed");
    assert!(stream_failure.diagnostic.contains("[REDACTED]"));
    assert!(!format!("{stream_error:#}").contains(sentinel));
    assert_eq!(stream_failure.stderr.as_deref(), Some(sentinel.as_bytes()));
}
