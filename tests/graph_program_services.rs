use anyhow::Result;
use flow::ast::Value;
use flow::graph::{
    self, EdgeSpec, GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphRunOptions,
    GraphSession, NodeIo, PortRef, PortValue, RuntimeGraphExecutor,
};
use flow::manifest::Registry;
use flow::modules::{load_program_module_package, ProgramModuleSession};
use flow::runtime::RunOptions;
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const SERVICE_APPLET: &str = "test.snapshot_service";

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn count_output_schema() -> JsonValue {
    json!({
        "type": "record",
        "encoding": "json",
        "fields": { "count": { "type": "integer" } },
        "required": ["count"]
    })
}

fn count_value_schema() -> JsonValue {
    json!({
        "type": "record",
        "fields": { "count": { "type": "integer" } },
        "required": ["count"]
    })
}

fn service_fixture(source: &str) -> (TempDir, Registry) {
    let root = tempfile::tempdir().unwrap();
    let applets = root.path().join("applets");
    fs::create_dir_all(&applets).unwrap();
    fs::write(applets.join("service.program"), source).unwrap();
    let descriptor = json!({
        "name": SERVICE_APPLET,
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {
            "snapshot": { "type": "json", "required": true }
        },
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": "service.program",
                "entry": "main",
                "interface_version": 2
            },
            "lifecycle": { "kind": "program_service" },
            "output": count_output_schema(),
            "ports": {
                "inputs": {
                    "snapshot": {
                        "schema": { "type": "json" },
                        "delivery": "single"
                    }
                },
                "events": {
                    "updated": {
                        "schema": count_value_schema(),
                        "delivery": "events"
                    }
                }
            }
        }
    });
    fs::write(
        applets.join("service.json"),
        serde_json::to_vec_pretty(&descriptor).unwrap(),
    )
    .unwrap();
    let source_descriptor = json!({
        "name": "test.snapshot_source",
        "binary": "printf",
        "accepts_stdin": false,
        "arguments": [{ "literal": "{\"count\":11}" }],
        "parameters": {},
        "contract": {
            "version": 1,
            "output": count_output_schema()
        }
    });
    fs::write(
        applets.join("snapshot_source.json"),
        serde_json::to_vec_pretty(&source_descriptor).unwrap(),
    )
    .unwrap();
    let registry = Registry::load(&applets).unwrap();
    (root, registry)
}

#[cfg(unix)]
fn worker_manifest(name: &str, marker: &Path) -> JsonValue {
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("Python 3 is available for worker fixture tests");
    let worker_fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/json_lines_worker.py");
    json!({
        "name": name,
        "binary": python,
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "json_lines_worker",
            "lifecycle": {
                "kind": "json_lines",
                "startup_arguments": [
                    worker_fixture,
                    "--worker",
                    "sequence",
                    marker,
                    marker.with_extension("effects")
                ],
                "timeout_ms": 3000,
                "max_frame_bytes": 1048576,
                "max_stderr_bytes": 8192
            },
            "output": {"type": "integer", "encoding": "json"},
            "capabilities": [],
            "effects": []
        }
    })
}

#[cfg(unix)]
fn service_fixture_with_worker(source: &str, marker: &Path) -> (TempDir, Registry) {
    let (root, registry) = service_fixture(source);
    let worker = worker_manifest("test.worker", marker);
    let registry = registry
        .with_scoped_manifest_sources(&[worker.to_string()], &root.path().join("applets"))
        .unwrap();
    (root, registry)
}

#[cfg(unix)]
fn service_fixture_with_workers(
    source: &str,
    first_marker: &Path,
    second_marker: &Path,
) -> (TempDir, Registry) {
    let (root, registry) = service_fixture(source);
    let first = worker_manifest("test.worker_one", first_marker);
    let second = worker_manifest("test.worker_two", second_marker);
    let registry = registry
        .with_scoped_manifest_sources(
            &[first.to_string(), second.to_string()],
            &root.path().join("applets"),
        )
        .unwrap();
    (root, registry)
}

fn service_with_v1_dependency_fixture() -> (TempDir, Registry) {
    let root = tempfile::tempdir().unwrap();
    let catalog = root.path().join("catalog");
    let service_dir = catalog.join("packages/service");
    let dependency_dir = catalog.join("packages/dependency");
    fs::create_dir_all(service_dir.join("src")).unwrap();
    fs::create_dir_all(dependency_dir.join("src")).unwrap();

    let dependency_source =
        "define a program named echo\n  accept payload as json\n  then encode payload as json\n";
    fs::write(
        dependency_dir.join("src/library.program"),
        dependency_source,
    )
    .unwrap();
    let dependency_package = json!({
        "format_version": 1,
        "name": "example.queue_record",
        "version": "1.0.0",
        "interface_version": 1,
        "source": "src/library.program",
        "source_sha256": sha256(dependency_source.as_bytes()),
        "exports": {
            "echo": {
                "entry": "echo",
                "interface_version": 1,
                "inputs": [
                    { "name": "payload", "type": "json", "required": true }
                ],
                "output": count_output_schema(),
                "lifecycle": { "kind": "one_shot" },
                "capabilities": []
            }
        },
        "dependencies": {}
    });
    let dependency_manifest_path = dependency_dir.join("program.modules.json");
    let dependency_manifest_bytes = serde_json::to_vec_pretty(&dependency_package).unwrap();
    fs::write(&dependency_manifest_path, &dependency_manifest_bytes).unwrap();

    let service_source =
        "accept snapshot as json\nevery 20 milliseconds\n  run dep.echo with payload snapshot\n";
    fs::write(service_dir.join("src/service.program"), service_source).unwrap();
    let service_package = json!({
        "format_version": 1,
        "name": "example.scheduled_queue",
        "version": "1.0.0",
        "interface_version": 2,
        "source": "src/service.program",
        "source_sha256": sha256(service_source.as_bytes()),
        "exports": {
            "poll": {
                "entry": "main",
                "interface_version": 2,
                "inputs": [
                    { "name": "snapshot", "type": "json", "required": true }
                ],
                "output": count_output_schema(),
                "lifecycle": { "kind": "schedule" },
                "ports": {
                    "inputs": {
                        "snapshot": {
                            "schema": { "type": "json" },
                            "delivery": "single"
                        }
                    },
                    "events": {
                        "updated": {
                            "schema": count_value_schema(),
                            "delivery": "events"
                        }
                    }
                }
            }
        },
        "dependencies": {
            "dep": {
                "manifest": "../dependency/program.modules.json",
                "source_sha256": sha256(dependency_source.as_bytes()),
                "manifest_sha256": sha256(&dependency_manifest_bytes),
                "version": "^1.0.0",
                "interface_min": 1,
                "interface_max_exclusive": 2
            }
        }
    });
    let service_manifest_path = service_dir.join("program.modules.json");
    fs::write(
        &service_manifest_path,
        serde_json::to_vec_pretty(&service_package).unwrap(),
    )
    .unwrap();

    let loaded = load_program_module_package(&service_manifest_path, &catalog, None).unwrap();
    fs::write(
        service_dir.join("program.modules.lock.json"),
        serde_json::to_vec_pretty(&loaded.lock_snapshot(&catalog).unwrap()).unwrap(),
    )
    .unwrap();
    let applet = json!({
        "name": "test.scheduled_queue",
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {
            "snapshot": { "type": "json", "required": true }
        },
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": "packages/service/src/service.program",
                "entry": "poll",
                "interface_version": 2,
                "package_manifest": "packages/service/program.modules.json",
                "package_lock": "packages/service/program.modules.lock.json"
            },
            "lifecycle": { "kind": "program_service" },
            "output": count_output_schema(),
            "ports": {
                "inputs": {
                    "snapshot": {
                        "schema": { "type": "json" },
                        "delivery": "single"
                    }
                },
                "events": {
                    "updated": {
                        "schema": count_value_schema(),
                        "delivery": "events"
                    }
                }
            }
        }
    });
    fs::write(
        catalog.join("scheduled_queue.json"),
        serde_json::to_vec_pretty(&applet).unwrap(),
    )
    .unwrap();
    (root, Registry::load(&catalog).unwrap())
}

fn options(cwd: &Path) -> RunOptions {
    RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: None,
    }
}

fn service_plan(
    registry: &Registry,
    snapshot: &str,
    extra_node: Option<&str>,
) -> flow::graph::GraphPlan {
    let extra = extra_node.unwrap_or("");
    let source = format!(
        "graph service_test\n  run {SERVICE_APPLET} with snapshot {snapshot} as service\n{extra}end graph\n"
    );
    graph::parse(&source, registry).unwrap()
}

fn start_session(
    plan: flow::graph::GraphPlan,
    registry: &Registry,
    root: &Path,
    mut graph_options: GraphRunOptions,
) -> GraphSession {
    let module_session =
        Arc::new(ProgramModuleSession::preload_invocations(registry, plan.invocations()).unwrap());
    let executor =
        RuntimeGraphExecutor::with_module_session(registry.clone(), options(root), module_session);
    graph_options
        .deadline
        .get_or_insert_with(|| Instant::now().checked_add(Duration::from_secs(5)).unwrap());
    GraphSession::start(plan, Arc::new(executor), graph_options).unwrap()
}

#[test]
fn scheduled_program_service_emits_successive_typed_events() {
    let source = "accept snapshot as json\nevery 20 milliseconds\n  then encode snapshot as json\n";
    let (root, registry) = service_fixture(source);
    let plan = service_plan(&registry, r#"{"count":7}"#, None);
    plan.validate_with_registry(&registry).unwrap();
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());

    let first = session.recv_output().unwrap().unwrap();
    let second = session.recv_output().unwrap().unwrap();
    assert_eq!(first.port.node, "service");
    assert_eq!(first.port.port, "updated");
    assert!(matches!(
        &first.payload,
        PortValue::Value(Value::Record(record)) if record.get("count") == Some(&Value::Integer(7))
    ));
    assert!(second.sequence > first.sequence);
    assert_eq!(second.payload, first.payload);
    session.cancel_and_join().unwrap();
}

#[test]
fn scheduled_service_resolves_declared_graph_inputs_before_startup() {
    let source = "accept snapshot as json\nevery 20 milliseconds\n  then encode snapshot as json\n";
    let (root, registry) = service_fixture(source);
    let unresolved = graph::parse(
        "accept requested as json\ngraph configured_service\n  run test.snapshot_service with snapshot requested as service\nend graph\n",
        &registry,
    )
    .unwrap();
    unresolved.validate_with_registry(&registry).unwrap();
    assert!(unresolved
        .with_input_values(&BTreeMap::new(), true)
        .is_err());

    let snapshot = Value::Record(BTreeMap::from([("count".into(), Value::Integer(23))]));
    let plan = unresolved
        .with_input_values(
            &BTreeMap::from([("requested".into(), snapshot.clone())]),
            true,
        )
        .unwrap();
    plan.validate_with_registry(&registry).unwrap();
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());

    let event = session.recv_output().unwrap().unwrap();
    assert_eq!(event.port.node, "service");
    assert_eq!(event.port.port, "updated");
    assert_eq!(event.payload, PortValue::Value(snapshot));
    session.cancel_and_join().unwrap();
}

#[test]
fn scheduled_service_reads_required_single_input_from_graph_edge() {
    let source = "accept snapshot as json\nevery 20 milliseconds\n  then encode snapshot as json\n";
    let (root, registry) = service_fixture(source);
    let plan = graph::parse(
        "graph service_input\n  run test.snapshot_source as seed\n  run test.snapshot_service as service\n  bind seed.stdout to service.snapshot with capacity 1 lossless\nend graph\n",
        &registry,
    )
    .unwrap();
    plan.validate_with_registry(&registry).unwrap();
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());

    let event = loop {
        let event = session.recv_output().unwrap().unwrap();
        if event.port.node == "service" && event.port.port == "updated" {
            break event;
        }
    };
    assert!(
        matches!(
            &event.payload,
            PortValue::Value(Value::Record(record)) if record.get("count") == Some(&Value::Integer(11))
        ),
        "unexpected service event payload: {:?}",
        event.payload
    );
    session.cancel_and_join().unwrap();
}

#[cfg(unix)]
#[test]
fn package_service_accepts_a_path_typed_single_input_from_a_graph_edge() {
    let root = tempfile::tempdir().unwrap();
    let catalog = root.path().join("catalog");
    let package_dir = catalog.join("packages/path_service");
    fs::create_dir_all(package_dir.join("src")).unwrap();

    let source = "accept location as text\nevery 20 milliseconds\n  then encode location as json\n";
    fs::write(package_dir.join("src/service.program"), source).unwrap();
    let output = json!({"type": "text", "encoding": "json"});
    let event_schema = json!({"type": "text"});
    let input_schema = json!({"type": "path"});
    let package = json!({
        "format_version": 1,
        "name": "example.path_service",
        "version": "1.0.0",
        "interface_version": 2,
        "source": "src/service.program",
        "source_sha256": sha256(source.as_bytes()),
        "exports": {
            "main": {
                "entry": "main",
                "interface_version": 2,
                "inputs": [{"name": "location", "type": "text", "required": true}],
                "output": output,
                "lifecycle": {"kind": "schedule"},
                "ports": {
                    "inputs": {
                        "location": {"schema": input_schema, "delivery": "single"}
                    },
                    "events": {
                        "updated": {"schema": event_schema, "delivery": "events"}
                    }
                }
            }
        },
        "dependencies": {}
    });
    let package_path = package_dir.join("program.modules.json");
    fs::write(&package_path, serde_json::to_vec_pretty(&package).unwrap()).unwrap();
    let loaded = load_program_module_package(&package_path, &catalog, None).unwrap();
    let lock_path = package_dir.join("program.modules.lock.json");
    fs::write(
        &lock_path,
        serde_json::to_vec_pretty(&loaded.lock_snapshot(&catalog).unwrap()).unwrap(),
    )
    .unwrap();

    let location = root.path().join("queue snapshot.json");
    let service = json!({
        "name": "test.path_service",
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {"location": {"type": "path", "required": true}},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": "packages/path_service/src/service.program",
                "entry": "main",
                "interface_version": 2,
                "package_manifest": "packages/path_service/program.modules.json",
                "package_lock": "packages/path_service/program.modules.lock.json"
            },
            "lifecycle": {"kind": "program_service"},
            "output": output,
            "ports": {
                "inputs": {
                    "location": {"schema": input_schema, "delivery": "single"}
                },
                "events": {
                    "updated": {"schema": event_schema, "delivery": "events"}
                }
            }
        }
    });
    fs::write(
        catalog.join("path_service.json"),
        serde_json::to_vec_pretty(&service).unwrap(),
    )
    .unwrap();
    let path_source = json!({
        "name": "test.path_source",
        "binary": "printf",
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [{"literal": location.to_string_lossy()}],
        "contract": {"version": 1, "output": {"type": "path", "encoding": "raw"}}
    });
    fs::write(
        catalog.join("path_source.json"),
        serde_json::to_vec_pretty(&path_source).unwrap(),
    )
    .unwrap();

    let registry = Registry::load(&catalog).unwrap();
    let plan = graph::parse(
        "graph package_path_input\n  run test.path_source as seed\n  run test.path_service as service\n  bind seed.stdout to service.location with capacity 1 lossless\nend graph\n",
        &registry,
    )
    .unwrap();
    plan.validate_with_registry(&registry).unwrap();
    let terminals = plan.output_terminals();
    assert_eq!(terminals.len(), 2);
    assert!(terminals.contains(&PortRef::new("seed", "status")));
    assert!(terminals.contains(&PortRef::new("service", "updated")));
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());
    let mut saw_source_status = false;
    let mut saw_service_event = false;
    for _ in 0..8 {
        let Some(event) = session.recv_output().unwrap() else {
            break;
        };
        match (event.port.node.as_str(), event.port.port.as_str()) {
            ("seed", "status") => {
                assert!(
                    !saw_source_status,
                    "source status should be a single output"
                );
                assert_eq!(event.payload, PortValue::Value(Value::Integer(0)));
                saw_source_status = true;
            }
            ("service", "updated") => {
                assert_eq!(
                    event.payload,
                    PortValue::Value(Value::String(location.to_string_lossy().into_owned()))
                );
                saw_service_event = true;
            }
            port => panic!("unexpected terminal output: {port:?}"),
        }
        if saw_source_status && saw_service_event {
            break;
        }
    }
    assert!(saw_source_status, "source status terminal was not observed");
    assert!(saw_service_event, "typed service event was not observed");
    session.cancel_and_join().unwrap();
}

#[cfg(unix)]
#[test]
fn graph_metrics_keep_process_count_when_finite_module_decode_fails() {
    let root = tempfile::tempdir().unwrap();
    let catalog = root.path().join("applets");
    fs::create_dir_all(&catalog).unwrap();
    fs::write(
        catalog.join("failure.program"),
        "define a program named entry\n  run \"printf\" with arguments [\"not-json\"]\n",
    )
    .unwrap();
    let applet = json!({
        "name": "test.failing_module",
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": "failure.program",
                "entry": "entry",
                "interface_version": 1
            },
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "json"}
        }
    });
    fs::write(
        catalog.join("failing_module.json"),
        serde_json::to_vec_pretty(&applet).unwrap(),
    )
    .unwrap();
    let registry = Registry::load(&catalog).unwrap();
    let plan = graph::parse(
        "graph failed_module_accounting\n  run test.failing_module as failing\nend graph\n",
        &registry,
    )
    .unwrap();
    plan.validate_with_registry(&registry).unwrap();
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());

    let error = session.recv_output().unwrap_err();
    assert!(format!("{error:#}").contains("validating program module output"));
    assert_eq!(session.context().metrics.launched_processes(), 1);
    session.cancel_and_join().unwrap();
}

#[cfg(unix)]
#[test]
fn graph_service_cancellation_reaps_only_its_retained_json_lines_worker() {
    let marker_root = tempfile::tempdir().unwrap();
    let marker = marker_root.path().join("worker.pids");
    let source = "accept snapshot as json\nevery 20 milliseconds\n  run test.worker\n  then encode snapshot as json\n";
    let (root, registry) = service_fixture_with_worker(source, &marker);
    let invocation_options = RunOptions {
        inputs: BTreeMap::new(),
        working_directory: root.path().to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: Some(Duration::from_secs(5)),
    };

    let global = flow::runtime::invoke_applet(
        &registry,
        "test.worker",
        BTreeMap::new(),
        Vec::new(),
        &invocation_options,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(global.output, b"1");
    let global_pid = fs::read_to_string(&marker)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();

    let plan = service_plan(&registry, r#"{"count":1}"#, None);
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());
    let event = session.recv_output().unwrap().unwrap();
    assert!(matches!(
        event.payload,
        PortValue::Value(Value::Record(ref record))
            if record.get("count") == Some(&Value::Integer(1))
    ));
    let pids = fs::read_to_string(&marker)
        .unwrap()
        .lines()
        .map(|line| line.parse::<libc::pid_t>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 2, "graph service owns a separate worker");
    let service_pid = pids[1];
    assert_ne!(service_pid, global_pid);

    session.cancel_and_join().unwrap();
    assert_process_reaped(service_pid);
    assert!(
        process_exists(global_pid),
        "graph shutdown killed the caller worker"
    );
}

#[cfg(unix)]
#[test]
fn service_finally_keeps_module_worker_context_and_exact_process_accounting() {
    let marker_root = tempfile::tempdir().unwrap();
    let worker_marker = marker_root.path().join("worker.pids");
    let command_pid_file = marker_root.path().join("cleanup-child.pid");
    let command = format!(
        "sleep 30 & echo $! > '{}'; wait; printf '{{\"count\":1}}'",
        command_pid_file.display()
    );
    let source = format!(
        "accept snapshot as json\nevery 10 milliseconds\n  try\n    run \"/bin/sh\" with arguments [\"-c\", {}]\n  finally\n    run test.worker\n  then encode snapshot as json\n",
        serde_json::to_string(&command).unwrap()
    );
    let (root, registry) = service_fixture_with_worker(&source, &worker_marker);
    let invocation_options = RunOptions {
        inputs: BTreeMap::new(),
        working_directory: root.path().to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: Some(Duration::from_secs(5)),
    };
    let global = flow::runtime::invoke_applet(
        &registry,
        "test.worker",
        BTreeMap::new(),
        Vec::new(),
        &invocation_options,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(global.output, b"1");
    let global_pid = fs::read_to_string(&worker_marker)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();

    let plan = service_plan(&registry, r#"{"count":1}"#, None);
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());
    let child_pid = wait_for_pid_file(&command_pid_file, Instant::now() + Duration::from_secs(3))
        .expect("service handler should start before graph cancellation");
    session.cancel_and_join().unwrap();

    let worker_pids = fs::read_to_string(&worker_marker)
        .unwrap()
        .lines()
        .map(|line| line.parse::<libc::pid_t>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        worker_pids.len(),
        2,
        "finally resolves and starts its worker"
    );
    let service_worker_pid = worker_pids[1];
    assert_process_reaped(child_pid);
    assert_process_reaped(service_worker_pid);
    assert!(
        process_exists(global_pid),
        "registry-owned worker should survive"
    );
    assert_eq!(
        session.context().metrics.launched_processes(),
        2,
        "the cancelled native command and finally worker both spawned"
    );

    let reused = flow::runtime::invoke_applet(
        &registry,
        "test.worker",
        BTreeMap::new(),
        Vec::new(),
        &invocation_options,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(reused.output, b"2");
    assert_eq!(reused.launched_processes, 0);
    assert!(process_exists(global_pid));
}

#[cfg(unix)]
#[test]
fn service_scoped_worker_pool_preserves_registry_capacity() {
    let marker_root = tempfile::tempdir().unwrap();
    let first_marker = marker_root.path().join("first-worker.pids");
    let second_marker = marker_root.path().join("second-worker.pids");
    let source = "accept snapshot as json\nevery 10 milliseconds\n  run test.worker_one\n  then test.worker_two\n  then encode snapshot as json\n";
    let (root, registry) = service_fixture_with_workers(source, &first_marker, &second_marker);
    registry.set_host_state(flow::runtime::worker_backend::WorkerPool::with_capacity(1).unwrap());
    let plan = service_plan(&registry, r#"{"count":1}"#, None);
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());

    let error = session.recv_output().unwrap_err();
    assert!(
        format!("{error:#}").contains("persistent worker pool capacity 1 is full"),
        "{error:#}"
    );
    assert_eq!(
        fs::read_to_string(&first_marker).unwrap().lines().count(),
        1,
        "first worker identity should consume the configured slot"
    );
    assert!(
        !second_marker.exists(),
        "second worker must be denied before it is spawned"
    );
    assert_eq!(session.context().metrics.launched_processes(), 1);
    session.cancel_and_join().unwrap();
}

struct WaitForCancellation {
    service_executor: RuntimeGraphExecutor,
    peer_stopped: Arc<AtomicBool>,
}

impl GraphExecutor for WaitForCancellation {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        if node.id == "peer" {
            while !io.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            self.peer_stopped.store(true, Ordering::Release);
            return Ok(());
        }
        self.service_executor.execute(node, io, context)
    }
}

#[test]
fn invalid_decoded_service_event_fails_the_graph_and_cancels_peers() {
    let source = "accept snapshot as json\nevery 10 milliseconds\n  then encode snapshot as json\n";
    let (root, registry) = service_fixture(source);
    let plan = service_plan(
        &registry,
        r#"{"unexpected":"field"}"#,
        Some("  every 5 seconds as peer\n"),
    );
    let module_session =
        Arc::new(ProgramModuleSession::preload_invocations(&registry, plan.invocations()).unwrap());
    let service_executor = RuntimeGraphExecutor::with_module_session(
        registry.clone(),
        options(root.path()),
        module_session,
    );
    let peer_stopped = Arc::new(AtomicBool::new(false));
    let executor = WaitForCancellation {
        service_executor,
        peer_stopped: Arc::clone(&peer_stopped),
    };
    let graph_options = GraphRunOptions {
        deadline: Some(Instant::now() + Duration::from_secs(3)),
        ..GraphRunOptions::default()
    };
    let error = GraphSession::start(plan, Arc::new(executor), graph_options)
        .unwrap()
        .run_to_completion()
        .unwrap_err();
    assert!(format!("{error:#}").contains("count"));
    assert!(peer_stopped.load(Ordering::Acquire));
}

#[test]
fn scheduled_package_handler_can_invoke_a_pinned_v1_dependency_alias() {
    let (root, registry) = service_with_v1_dependency_fixture();
    let plan = graph::parse(
        "graph package_service\n  run test.scheduled_queue with snapshot {\"count\":9} as service\nend graph\n",
        &registry,
    )
    .unwrap();
    plan.validate_with_registry(&registry).unwrap();
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());
    let event = session.recv_output().unwrap().unwrap();
    assert!(matches!(
        &event.payload,
        PortValue::Value(Value::Record(record)) if record.get("count") == Some(&Value::Integer(9))
    ));
    session.cancel_and_join().unwrap();
}

struct HoldAfterFirstEvent {
    service_executor: RuntimeGraphExecutor,
    first_event: mpsc::SyncSender<flow::graph::EventEnvelope>,
}

impl GraphExecutor for HoldAfterFirstEvent {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        if node.id == "sink" {
            let event = io.recv("updated")?.ok_or_else(|| {
                anyhow::anyhow!("service event input closed before its first value")
            })?;
            self.first_event
                .send(event)
                .map_err(|_| anyhow::anyhow!("test event receiver closed"))?;
            while !io.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            return Ok(());
        }
        self.service_executor.execute(node, io, context)
    }
}

fn wait_for_marker_count(path: &Path, expected: usize, deadline: Instant) -> usize {
    loop {
        let count = fs::read_to_string(path)
            .map(|contents| contents.lines().count())
            .unwrap_or(0);
        if count >= expected {
            return count;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} completed service handlers; observed {count}"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn cancelling_a_service_blocked_by_a_full_lossless_edge_queue_joins_it() {
    let marker_root = tempfile::tempdir().unwrap();
    let marker = marker_root.path().join("completed-handlers");
    let command = format!(
        "printf 'tick\\n' >> '{}'; printf '{{\"count\":3}}'",
        marker.display()
    );
    let source = format!(
        "accept snapshot as json\nevery 10 milliseconds\n  run \"/bin/sh\" with arguments [\"-c\", {}]\n",
        serde_json::to_string(&command).unwrap()
    );
    let (root, registry) = service_fixture(&source);
    let mut plan = service_plan(&registry, r#"{"count":3}"#, None);
    let service = plan
        .nodes()
        .iter()
        .find(|node| node.id == "service")
        .unwrap();
    let event_schema = service.events["updated"].clone();
    plan.add_node(GraphNode {
        id: "sink".into(),
        kind: GraphNodeKind::Timer { interval_ms: 1 },
        inputs: BTreeMap::from([("updated".into(), event_schema)]),
        outputs: BTreeMap::new(),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("service", "updated"),
        PortRef::new("sink", "updated"),
        1,
    ));
    plan.validate().unwrap();

    let module_session =
        Arc::new(ProgramModuleSession::preload_invocations(&registry, plan.invocations()).unwrap());
    let service_executor = RuntimeGraphExecutor::with_module_session(
        registry.clone(),
        options(root.path()),
        module_session,
    );
    let (first_event_tx, first_event_rx) = mpsc::sync_channel(1);
    let executor = HoldAfterFirstEvent {
        service_executor,
        first_event: first_event_tx,
    };
    let mut graph_options = GraphRunOptions::default();
    graph_options
        .deadline
        .get_or_insert_with(|| Instant::now() + Duration::from_secs(5));
    let mut session = GraphSession::start(plan, Arc::new(executor), graph_options).unwrap();

    let first = first_event_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("the sink should consume and report the first service event");
    assert!(matches!(
        &first.payload,
        PortValue::Value(Value::Record(record)) if record.get("count") == Some(&Value::Integer(3))
    ));

    // The sink now holds its input. Handler one was consumed, handler two fills
    // the one-slot edge queue, and handler three reaches the producer while the
    // queue is full. A fourth launch would prove that the lossless send did not
    // block the scheduled service.
    assert_eq!(
        wait_for_marker_count(&marker, 3, Instant::now() + Duration::from_secs(3)),
        3
    );
    let metrics = Arc::clone(&session.context().metrics);
    assert_eq!(metrics.launched_processes(), 3);
    let (fourth_launch_tx, fourth_launch_rx) = mpsc::sync_channel(1);
    let launch_monitor = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < deadline {
            let launched = metrics.launched_processes();
            if launched > 3 {
                let _ = fourth_launch_tx.send(launched);
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
    });
    assert!(
        fourth_launch_rx
            .recv_timeout(Duration::from_millis(200))
            .is_err(),
        "the service launched another handler while its lossless edge queue was full"
    );
    launch_monitor.join().unwrap();
    assert_eq!(session.context().metrics.launched_processes(), 3);

    session.cancel_and_join().unwrap();
}

#[test]
fn post_spawn_decode_failure_retains_the_exact_service_process_count() {
    let source = "accept snapshot as json\nevery 10 milliseconds\n  run \"printf\" with arguments [\"{\\\"wrong\\\":true}\"]\n";
    let (root, registry) = service_fixture(source);
    let plan = service_plan(&registry, r#"{"count":4}"#, None);
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());
    let error = session.recv_output().unwrap_err();
    assert!(format!("{error:#}").contains("count"));
    assert_eq!(session.context().metrics.launched_processes(), 1);
    session.cancel_and_join().unwrap();
}

#[cfg(unix)]
fn wait_for_pid_file(path: &Path, deadline: Instant) -> Option<libc::pid_t> {
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return Some(pid);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    None
}

#[cfg(unix)]
fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: kill(pid, 0) performs an existence/permission check and sends no signal.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
fn assert_process_reaped(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_exists(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_exists(pid),
        "native child {pid} remained after graph cancellation"
    );
}

#[cfg(unix)]
#[test]
fn cancellation_reaps_service_native_children_and_accounts_for_active_launches() {
    let pid_root = tempfile::tempdir().unwrap();
    let pid_file: PathBuf = pid_root.path().join("child.pid");
    let command = format!(
        "sleep 30 & echo $! > '{}'; wait; printf '{{\"count\":1}}'",
        pid_file.display()
    );
    let source = format!(
        "accept snapshot as json\nevery 10 milliseconds\n  run \"/bin/sh\" with arguments [\"-c\", {}]\n  then encode snapshot as json\n",
        serde_json::to_string(&command).unwrap()
    );
    let (root, registry) = service_fixture(&source);
    let plan = service_plan(&registry, r#"{"count":1}"#, None);
    let mut session = start_session(plan, &registry, root.path(), GraphRunOptions::default());
    let pid = wait_for_pid_file(&pid_file, Instant::now() + Duration::from_secs(3))
        .expect("service should launch its native command and write the child PID");
    assert!(session.context().metrics.launched_processes() >= 1);

    session.cancel_and_join().unwrap();
    assert_process_reaped(pid);
}

#[cfg(unix)]
#[test]
fn graph_deadline_reaps_service_native_children() {
    let pid_root = tempfile::tempdir().unwrap();
    let pid_file = pid_root.path().join("deadline-child.pid");
    let command = format!(
        "sleep 30 & echo $! > '{}'; wait; printf '{{\"count\":1}}'",
        pid_file.display()
    );
    let source = format!(
        "accept snapshot as json\nevery 10 milliseconds\n  run \"/bin/sh\" with arguments [\"-c\", {}]\n  then encode snapshot as json\n",
        serde_json::to_string(&command).unwrap()
    );
    let (root, registry) = service_fixture(&source);
    let plan = service_plan(&registry, r#"{"count":1}"#, None);
    let graph_options = GraphRunOptions {
        deadline: Some(Instant::now() + Duration::from_millis(300)),
        ..GraphRunOptions::default()
    };
    let mut session = start_session(plan, &registry, root.path(), graph_options);
    let pid = wait_for_pid_file(&pid_file, Instant::now() + Duration::from_secs(3))
        .expect("service should launch its native command before the deadline");
    let error = loop {
        match session.recv_output() {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("long-running service unexpectedly closed its event output"),
            Err(error) => break error,
        }
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("deadline") || message.contains("timed out"),
        "{message}"
    );
    session.cancel_and_join().unwrap();
    assert_process_reaped(pid);
}

#[test]
fn service_contract_validation_precedes_graph_effects() {
    let marker_root = tempfile::tempdir().unwrap();
    let marker = marker_root.path().join("must-not-exist");
    let command = format!("/usr/bin/touch {}", marker.display());
    let source = format!(
        "accept snapshot as json\nevery 10 milliseconds\n  run \"/bin/sh\" with arguments [\"-c\", {}]\n  then encode snapshot as json\n",
        serde_json::to_string(&command).unwrap()
    );
    let (root, registry) = service_fixture(&source);
    let valid = service_plan(&registry, r#"{"count":1}"#, None);
    let mut encoded: JsonValue = serde_json::from_str(&valid.to_json().unwrap()).unwrap();
    encoded["nodes"][0]["events"]["updated"]["schema"]["fields"]["count"]["type"] = json!("text");
    let invalid: flow::graph::GraphPlan = serde_json::from_value(encoded).unwrap();
    let error = graph::run(
        invalid,
        &registry,
        &options(root.path()),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("event ports must match"));
    assert!(!marker.exists());
}

#[test]
fn checked_in_queue_monitor_package_composes_as_a_scheduled_graph_node() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/modules/queue_monitor");
    let package = load_program_module_package(
        &root.join("package/program.modules.json"),
        &root,
        Some(&root.join("package/program.modules.lock.json")),
    )
    .unwrap();
    let export = package.export("poll").unwrap();
    assert!(matches!(
        &export.entry,
        flow::modules::LoadedProgramEntry::ProgramService
    ));

    let registry = Registry::load(&root).unwrap();
    let graph_source = fs::read_to_string(root.join("queue_monitor.graph.program")).unwrap();
    let plan = graph::parse(&graph_source, &registry).unwrap();
    plan.validate_with_registry(&registry).unwrap();
    let session = ProgramModuleSession::preload_invocations(&registry, plan.invocations()).unwrap();
    let example_manifest = registry.resolve("examples.queue_monitor.poll").unwrap();
    assert!(session
        .get_for_manifest(&registry, example_manifest)
        .is_some_and(|loaded| loaded.is_program_service()));
}
