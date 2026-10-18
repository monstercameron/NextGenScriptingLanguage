#![cfg(unix)]

use flow::{manifest::Registry, parser, runtime};
use serde_json::json;
use std::{collections::BTreeMap, path::Path, time::Duration};

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        max_output_bytes: 16,
        command_timeout: Some(Duration::from_secs(10)),
        ..runtime::RunOptions::new(cwd.to_owned())
    }
}

fn registry(manifests: &[serde_json::Value]) -> Registry {
    let sources = manifests
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>();
    Registry::from_sources(&sources).unwrap()
}

fn applet(
    name: &str,
    binary: &str,
    arguments: &[&str],
    stdio: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut manifest = json!({
        "name": name,
        "binary": binary,
        "arguments": arguments.iter().map(|value| json!({"literal": value})).collect::<Vec<_>>(),
    });
    if let Some(stdio) = stdio {
        manifest["stdio"] = stdio;
    }
    manifest
}

#[test]
fn stdout_discard_is_bounded_by_the_os_null_device_not_the_capture_limit() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(&[applet(
        "flood",
        "head",
        &["-c", "1048576", "/dev/zero"],
        Some(json!({"stdout": "discard"})),
    )]);
    let program = parser::parse("run flood\n").unwrap();
    let result = runtime::run(&program, &catalog, &options(directory.path())).unwrap();
    assert!(result.output.is_empty());
    assert_eq!(result.launched_processes, 1);
}

#[test]
fn pipeline_intermediate_stdout_stays_a_direct_pipe_when_terminal_stdout_is_discarded() {
    let directory = tempfile::tempdir().unwrap();
    let source = applet("source", "printf", &["%100000s", "x"], None);
    let mut sink = applet("sink", "cat", &[], Some(json!({"stdout": "discard"})));
    sink["accepts_stdin"] = json!(true);
    let catalog = registry(&[source, sink]);
    let program = parser::parse("run source\npipe to sink\n").unwrap();
    let result = runtime::run(&program, &catalog, &options(directory.path())).unwrap();
    assert!(result.output.is_empty());
    assert_eq!(result.launched_processes, 2);
}

#[test]
fn stderr_discard_does_not_turn_large_diagnostics_into_an_output_limit_failure() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(&[applet(
        "stderr-flood",
        "sh",
        &["-c", "head -c 1048576 /dev/zero >&2"],
        Some(json!({"stderr": "discard"})),
    )]);
    let program = parser::parse("run stderr-flood\n").unwrap();
    let result = runtime::run(&program, &catalog, &options(directory.path())).unwrap();
    assert!(result.output.is_empty());
    assert_eq!(result.launched_processes, 1);
}

#[test]
fn inherited_stdout_is_absent_from_the_runtime_result() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(&[applet(
        "inherited",
        "printf",
        &["inherited stdout"],
        Some(json!({"stdout": "inherit"})),
    )]);
    let program = parser::parse("run inherited\n").unwrap();
    let result = runtime::run(&program, &catalog, &options(directory.path())).unwrap();
    assert!(result.output.is_empty());
    assert_eq!(result.launched_processes, 1);
}

#[test]
fn absent_stdio_fields_preserve_legacy_capture_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(&[applet("captured", "printf", &["legacy"], None)]);
    let program = parser::parse("run captured\n").unwrap();
    let result = runtime::run(&program, &catalog, &options(directory.path())).unwrap();
    assert_eq!(result.output, b"legacy");
}

#[test]
fn outcome_classification_requires_explicit_status_capture_and_keeps_nonzero_success() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(&[
        json!({
            "name":"nonzero-success",
            "binary":"sh",
            "arguments":[{"literal":"-c"},{"literal":"printf typed; exit 7"}],
            "contract":{
                "version":1,
                "output":{"type":"text","encoding":"raw"},
                "outcomes":[{"status":7,"name":"complete","kind":"success"}]
            }
        }),
        json!({
            "name":"absence",
            "binary":"sh",
            "arguments":[{"literal":"-c"},{"literal":"printf absent; exit 4"}],
            "contract":{
                "version":1,
                "outcomes":[
                    {"status":0,"name":"success","kind":"success"},
                    {"status":4,"name":"missing","kind":"absence"}
                ]
            }
        }),
    ]);

    let typed = parser::parse(
        "run nonzero-success as value\nthen \"printf\" with arguments [\"%s\", value]\n",
    )
    .unwrap();
    let result = runtime::run(&typed, &catalog, &options(directory.path())).unwrap();
    assert_eq!(result.output, b"typed");

    let unhandled_absence = parser::parse("run absence\n").unwrap();
    let error = runtime::run(&unhandled_absence, &catalog, &options(directory.path()))
        .err()
        .unwrap();
    let failed = error.downcast_ref::<runtime::CommandFailed>().unwrap();
    assert_eq!(failed.status.code(), Some(4));

    let retained_absence = parser::parse(
        "run absence and keep status as code\nthen \"printf\" with arguments [\"%s\", code]\n",
    )
    .unwrap();
    let result = runtime::run(&retained_absence, &catalog, &options(directory.path())).unwrap();
    assert_eq!(result.output, b"4");
}
