#![cfg(unix)]

use flow::modules::{ProgramModuleSession, PROGRAM_MODULE_INTERFACE_VERSION};
use flow::{manifest::Registry, runtime};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

fn write_program_module(catalog: &Path, name: &str, source: &str) {
    fs::create_dir_all(catalog).unwrap();
    let source_name = format!("{}.program", name.replace('.', "_"));
    fs::write(catalog.join(&source_name), source).unwrap();
    let manifest = serde_json::json!({
        "name": name,
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": source_name,
                "entry": "entry",
                "interface_version": PROGRAM_MODULE_INTERFACE_VERSION
            },
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "json"}
        }
    });
    fs::write(
        catalog.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn write_failing_applet(catalog: &Path, script: &Path) {
    let manifest = serde_json::json!({
        "name": "fixture.fail",
        "binary": script,
        "accepts_stdin": false,
        "parameters": {
            "started": {"type": "string", "required": true},
            "other": {"type": "string", "required": true}
        },
        "arguments": [
            {"parameter": "started"},
            {"parameter": "other"}
        ],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw"},
            "capabilities": [],
            "effects": []
        }
    });
    fs::write(
        catalog.join("fixture.fail.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn write_bad_interpreter_applet(catalog: &Path, binary: &Path) {
    let manifest = serde_json::json!({
        "name": "fixture.bad-interpreter",
        "binary": binary,
        "accepts_stdin": true,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw"},
            "capabilities": [],
            "effects": []
        }
    });
    fs::write(
        catalog.join("fixture.bad-interpreter.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn write_failing_task_applet(catalog: &Path) {
    let manifest = serde_json::json!({
        "name": "fixture.task-fail",
        "binary": "/usr/bin/false",
        "accepts_stdin": false,
        "parameters": {"value": {"type": "string", "required": true}},
        "arguments": [{"parameter": "value"}],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw"},
            "capabilities": [],
            "effects": []
        }
    });
    fs::write(
        catalog.join("fixture.task-fail.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: Some(Duration::from_secs(10)),
        ..runtime::RunOptions::new(cwd.to_path_buf())
    }
}

fn invoke_module(catalog: &Path, cwd: &Path) -> anyhow::Result<runtime::AppletInvocationResult> {
    let registry = Registry::load(catalog).unwrap();
    let caller = flow::parser::parse("run test.module\n").unwrap();
    let session = ProgramModuleSession::preload_program(&caller, &registry).unwrap();
    runtime::invoke_applet_with_module_session(
        &registry,
        "test.module",
        BTreeMap::new(),
        Vec::new(),
        &options(cwd),
        Arc::new(AtomicBool::new(false)),
        Arc::new(session),
    )
}

fn invoke_failure(catalog: &Path, cwd: &Path) -> anyhow::Error {
    invoke_module(catalog, cwd).unwrap_err()
}

#[test]
fn module_output_failure_reports_the_native_process_that_already_ran() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_program_module(
        &catalog,
        "test.module",
        "define a program named entry\n  run \"printf\" with arguments [\"not-json\"]\n",
    );

    let error = invoke_failure(&catalog, temp.path());
    let invocation = error
        .downcast_ref::<runtime::AppletInvocationError>()
        .expect("failed applet calls retain typed process accounting");
    assert_eq!(invocation.launched_processes(), 1, "printf spawned once");
    assert!(format!("{error:#}").contains("validating program module output"));
}

#[test]
fn module_body_failure_reports_all_successful_spawns_before_the_failed_step() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_program_module(
        &catalog,
        "test.module",
        "define a program named entry\n  run \"printf\" with arguments [\"started\"]\n  then \"false\"\n",
    );

    let error = invoke_failure(&catalog, temp.path());
    let invocation = error
        .downcast_ref::<runtime::AppletInvocationError>()
        .expect("failed applet calls retain typed process accounting");
    assert_eq!(
        invocation.launched_processes(),
        2,
        "the first command and the failing command both spawned"
    );
}

#[test]
fn awaited_background_spawn_is_included_when_a_module_later_fails() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_program_module(
        &catalog,
        "test.module",
        "define a program named entry\n  spawn as completed\n    run \"true\"\n  await completed\n  then \"false\"\n",
    );

    let error = invoke_failure(&catalog, temp.path());
    let invocation = error
        .downcast_ref::<runtime::AppletInvocationError>()
        .expect("failed applet calls retain typed process accounting");
    assert_eq!(
        invocation.launched_processes(),
        2,
        "the completed job child and subsequent failing command both spawned"
    );
}

#[test]
fn failed_task_module_with_an_awaited_job_is_counted_once() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    fs::create_dir_all(&catalog).unwrap();
    write_failing_task_applet(&catalog);
    write_program_module(
        &catalog,
        "test.module",
        "define a program named entry\n  spawn as completed\n    run \"true\"\n  await completed\n  parallel at most 1 as results\n    run fixture.task-fail with value \"ok\" as child\n",
    );

    let error = invoke_failure(&catalog, temp.path());
    let invocation = error
        .downcast_ref::<runtime::AppletInvocationError>()
        .expect("failed applet calls retain typed process accounting");
    assert_eq!(
        invocation.launched_processes(),
        2,
        "the awaited job and failing task child both spawned"
    );
    assert!(format!("{error:#}").contains("task group failed at task"));
}

#[test]
fn partial_background_pipeline_spawn_is_included_when_module_fails() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    fs::create_dir_all(&catalog).unwrap();
    let binary = catalog.join("bad-interpreter");
    fs::write(&binary, "#!/execute-test-missing-interpreter\nexit 0\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    write_bad_interpreter_applet(&catalog, &binary);
    write_program_module(
        &catalog,
        "test.module",
        "define a program named entry\n  spawn as partial\n    run \"true\"\n    pipe to fixture.bad-interpreter\n  await partial\n",
    );

    let error = invoke_failure(&catalog, temp.path());
    let invocation = error
        .downcast_ref::<runtime::AppletInvocationError>()
        .expect("failed applet calls retain typed process accounting");
    assert_eq!(
        invocation.launched_processes(),
        1,
        "the first stage remains counted when the next stage cannot spawn"
    );
    assert!(format!("{error:#}").contains("starting background job"));
}

#[test]
fn nested_module_body_failure_keeps_process_count_from_its_earlier_step() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_program_module(
        &catalog,
        "test.module",
        "define a program named helper\n  run \"printf\" with arguments [\"started\"]\n  then \"false\"\ndefine a program named entry\n  run program helper\n",
    );

    let error = invoke_failure(&catalog, temp.path());
    let invocation = error
        .downcast_ref::<runtime::AppletInvocationError>()
        .expect("failed applet calls retain typed process accounting");
    assert_eq!(invocation.launched_processes(), 2);
}

#[test]
fn nested_module_success_counts_each_spawn_once() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_program_module(
        &catalog,
        "test.module",
        "define a program named helper\n  run \"printf\" with arguments [\"null\"]\ndefine a program named entry\n  run program helper\n  then \"printf\" with arguments [\"\\\"ok\\\"\"]\n",
    );

    let result = invoke_module(&catalog, temp.path()).unwrap();
    assert_eq!(result.launched_processes, 2);
    assert_eq!(result.value, Some(flow::ast::Value::String("ok".into())));
}

#[test]
fn parallel_module_tasks_report_each_child_spawn_once_on_failure() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    fs::create_dir_all(&catalog).unwrap();
    let script = catalog.join("fail-after-peer-starts.sh");
    fs::write(
        &script,
        "#!/bin/sh\ntouch \"$1\"\nwhile [ ! -f \"$2\" ]; do :; done\nexit 7\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    write_failing_applet(&catalog, &script);
    write_program_module(
        &catalog,
        "test.module",
        &format!(
            "define a program named helper\n  parallel at most 2 as results\n    run fixture.fail with started {:?} and other {:?} as first\n    run fixture.fail with started {:?} and other {:?} as second\ndefine a program named entry\n  run program helper\n",
            temp.path().join("first-started"),
            temp.path().join("second-started"),
            temp.path().join("second-started"),
            temp.path().join("first-started"),
        ),
    );

    let error = invoke_failure(&catalog, temp.path());
    let invocation = error
        .downcast_ref::<runtime::AppletInvocationError>()
        .expect("failed applet calls retain typed process accounting");
    assert_eq!(
        invocation.launched_processes(),
        2,
        "both task children spawned"
    );
    assert!(temp.path().join("first-started").exists());
    assert!(temp.path().join("second-started").exists());
    assert!(
        format!("{error:#}").contains("task group failed at task"),
        "nested task-group failure classification should remain visible"
    );
}

#[test]
fn nested_finite_helper_spawns_are_included_when_outer_module_decode_fails() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_program_module(
        &catalog,
        "test.module",
        "define a program named helper\n  run \"printf\" with arguments [\"null\"]\ndefine a program named entry\n  run program helper\n  then \"printf\" with arguments [\"not-json\"]\n",
    );

    let error = invoke_failure(&catalog, temp.path());
    let invocation = error
        .downcast_ref::<runtime::AppletInvocationError>()
        .expect("failed applet calls retain typed process accounting");
    assert_eq!(invocation.launched_processes(), 2);
    assert!(format!("{error:#}").contains("validating program module output"));
}

#[test]
fn caught_failed_module_keeps_its_spawn_count_when_root_finishes() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_program_module(
        &catalog,
        "test.module",
        "define a program named entry\n  run \"printf\" with arguments [\"not-json\"]\n",
    );
    let registry = Registry::load(&catalog).unwrap();
    let caller = flow::parser::parse(
        "try\n  run test.module\ncatch failure\n  then encode failure as json\nrun \"printf\" with arguments [\"completed\"]\n",
    )
    .unwrap();

    let result = runtime::run(&caller, &registry, &options(temp.path())).unwrap();
    assert_eq!(result.output, b"completed");
    assert_eq!(
        result.launched_processes, 2,
        "the failed module and successful root command both spawned once"
    );
}

#[test]
fn finally_can_resolve_a_preloaded_module_and_counts_all_spawns() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_program_module(
        &catalog,
        "test.module",
        "define a program named entry\n  run \"printf\" with arguments [\"\\\"module\\\"\"]\n",
    );
    let registry = Registry::load(&catalog).unwrap();
    let caller = flow::parser::parse(
        "try\n  run \"false\"\ncatch failure\n  then encode failure as json\nfinally\n  run test.module\nrun \"printf\" with arguments [\"completed\"]\n",
    )
    .unwrap();

    let result = runtime::run(&caller, &registry, &options(temp.path())).unwrap();
    assert_eq!(result.output, b"completed");
    assert_eq!(
        result.launched_processes, 3,
        "the failed command, finally module, and root command each spawned once"
    );
}
