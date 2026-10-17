#![cfg(unix)]

use flow::ast::Value;
use flow::manifest::Registry;
use flow::{parser, runtime};
use serde_json::{json, Value as JsonValue};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: None,
    }
}

fn registry(manifests: &[JsonValue]) -> Registry {
    let sources = manifests
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    Registry::from_sources(&sources).unwrap()
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn argument_manifest(name: &str, binary: &str, contract: JsonValue) -> JsonValue {
    json!({
        "name": name,
        "binary": binary,
        "parameters": {"status": {"type": "integer", "required": true}},
        "arguments": [{"parameter": "status"}],
        "contract": contract
    })
}

#[test]
fn invalid_typed_output_schemas_fail_during_manifest_load() {
    let invalid = json!({
        "name": "invalid.integer",
        "binary": "/bin/true",
        "arguments": [],
        "contract": {
            "version": 1,
            "output": {"type": "integer", "encoding": "raw"}
        }
    });
    let source = serde_json::to_string(&invalid).unwrap();
    let error = Registry::from_sources(&[source]).err().unwrap();
    assert!(
        format!("{error:#}").contains("integer"),
        "unexpected manifest error: {error:#}"
    );
}

#[test]
fn record_outputs_feed_existing_take_and_condition_syntax() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("record-output");
    executable(
        &fixture,
        "#!/bin/sh\nprintf '{\"found\":true,\"message\":\"ready\"}\\n'\n",
    );
    let catalog = registry(&[json!({
        "name": "record.fixture",
        "binary": fixture,
        "arguments": [],
        "contract": {
            "version": 1,
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {
                    "found": {"type": "boolean"},
                    "message": {"type": "text"}
                },
                "required": ["found", "message"]
            }
        }
    })]);
    let program = parser::parse(
        "run record.fixture as result\ntake boolean at \"/found\" from result as found\ntake text at \"/message\" from result as message\nif found is true\n  run \"printf\" with arguments [\"%s\", message]\notherwise\n  run \"printf\" with arguments [\"missing\"]\n",
    )
    .unwrap();
    let result = runtime::run(&program, &catalog, &options(directory.path())).unwrap();
    assert_eq!(result.output, b"ready");
}

#[test]
fn finite_programs_can_use_typed_references_in_nested_record_parameters() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(&[json!({
        "name": "record.echo",
        "binary": "/usr/bin/printf",
        "parameters": {
            "config": {
                "type": "record",
                "required": true,
                "fields": {
                    "label": {"type": "string", "required": true},
                    "settings": {
                        "type": "record",
                        "required": true,
                        "fields": {"retry": {"type": "integer", "required": true}}
                    }
                }
            }
        },
        "arguments": [{"literal": "%s\\n"}, {"json_parameter": "config"}],
        "contract": {
            "version": 1,
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {
                    "label": {"type": "text"},
                    "settings": {
                        "type": "record",
                        "fields": {"retry": {"type": "integer"}},
                        "required": ["retry"]
                    }
                },
                "required": ["label", "settings"]
            }
        }
    })]);
    let source = "accept label as text\naccept retry as integer\nrun record.echo with config {\"label\": label, \"settings\": {\"retry\": retry}} as result\ntake text at \"/label\" from result as returned_label\ntake integer at \"/settings/retry\" from result as returned_retry\nthen \"printf\" with arguments [\"%s:%s\", returned_label, returned_retry]\n";
    let program = parser::parse(source).unwrap();
    let mut run_options = options(directory.path());
    run_options.inputs = BTreeMap::from([
        ("label".into(), Value::String("Ada".into())),
        ("retry".into(), Value::Integer(4)),
    ]);
    let result = runtime::run(&program, &catalog, &run_options).unwrap();
    assert_eq!(result.output, b"Ada:4");
}

#[test]
fn typed_path_outputs_are_semantically_checked_and_feed_path_operations() {
    let directory = tempfile::tempdir().unwrap();
    let child = directory.path().join("nested");
    fs::create_dir(&child).unwrap();
    let parent_manifest = json!({
        "name": "path.dirname",
        "binary": "@execute-applet",
        "parameters": {"path": {"type": "path", "required": true}},
        "arguments": [
            {"literal": "path.dirname"},
            {"literal": "--args"},
            {"json_arguments": true}
        ],
        "contract": {
            "version": 1,
            "backend": "in_process",
            "output": {"type": "path", "encoding": "json"}
        }
    });
    let basename_manifest = json!({
        "name": "path.basename",
        "binary": "@execute-applet",
        "parameters": {"path": {"type": "path", "required": true}},
        "arguments": [
            {"literal": "path.basename"},
            {"literal": "--args"},
            {"json_arguments": true}
        ],
        "contract": {
            "version": 1,
            "backend": "in_process",
            "output": {"type": "text", "encoding": "json"}
        }
    });
    let catalog = registry(&[parent_manifest, basename_manifest]);
    let expected_parent = fs::canonicalize(&child).unwrap();
    let source = format!(
        "accept folder as text\nuse folder folder\nrun \"pwd\" as actual\nrun path.dirname with path {:?} as parent\nrun path.basename with path parent as basename\nthen \"printf\" with arguments [\"%s%s\", actual, basename]\n",
        child.join("file.txt").to_string_lossy()
    );
    let mut run_options = options(directory.path());
    run_options.inputs.insert(
        "folder".into(),
        Value::String(child.to_string_lossy().into_owned()),
    );
    let result = runtime::run(&parser::parse(&source).unwrap(), &catalog, &run_options).unwrap();
    assert_eq!(
        result.output,
        format!("{}\nnested", expected_parent.display()).as_bytes()
    );
}

#[test]
fn in_process_json_output_binding_preserves_result_envelope_and_stdout_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let mut manifest: JsonValue =
        serde_json::from_str(include_str!("../manifests/core.number_add.json")).unwrap();
    manifest["contract"]["result_envelope"] = json!(true);
    manifest["contract"]
        .as_object_mut()
        .unwrap()
        .remove("ports");
    let catalog = registry(&[manifest]);
    let program = parser::parse(
        "run number.add with left 2 and right 3 as wrapped and keep result as execution\ntake integer at \"/value\" from wrapped as answer\nthen encode execution as json\n",
    )
    .unwrap();

    let result = runtime::run(&program, &catalog, &options(directory.path())).unwrap();
    let execution: JsonValue = serde_json::from_slice(&result.output).unwrap();
    assert_eq!(execution["value"]["value"], json!(5));
    assert_eq!(execution["stdout"], json!(b"5\n"));
    assert_eq!(execution["stdout_bytes"], json!(2));
}

#[test]
fn cli_deep_in_process_json_binding_matches_cache_hit_byte_decoding() {
    let directory = tempfile::tempdir().unwrap();
    let manifest_directory = directory.path().join("manifests");
    let mut manifest_builder = fs::DirBuilder::new();
    manifest_builder
        .mode(0o700)
        .create(&manifest_directory)
        .unwrap();
    let cache_directory = directory.path().join("cache");
    let mut cache_builder = fs::DirBuilder::new();
    cache_builder.mode(0o700).create(&cache_directory).unwrap();
    let mut manifest: JsonValue =
        serde_json::from_str(include_str!("../manifests/core.json_get.json")).unwrap();
    manifest["cacheable"] = json!(true);
    assert_eq!(manifest["binary"], "@execute-applet");
    assert_eq!(manifest["contract"]["backend"], "in_process");
    fs::write(
        manifest_directory.join("json_get.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let workflow = directory.path().join("workflow.program");
    fs::write(
        &workflow,
        "accept payload as json\nrun json.get with document payload and pointer \"\" as result and cache\nthen encode result as json\n",
    )
    .unwrap();
    let payload = (0..70).fold(JsonValue::Null, |nested, _| JsonValue::Array(vec![nested]));
    let payload_argument = format!("payload={}", serde_json::to_string(&payload).unwrap());
    let expected_output = serde_json::to_vec(&payload).unwrap();

    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .arg("run")
            .arg(&workflow)
            .arg("--manifest-dir")
            .arg(&manifest_directory)
            .arg("--cache-dir")
            .arg(&cache_directory)
            .arg("--input")
            .arg(&payload_argument)
            .arg("--trace")
            .output()
            .unwrap()
    };

    let cache_miss = run();
    assert!(
        cache_miss.status.success(),
        "first CLI run failed: {}",
        String::from_utf8_lossy(&cache_miss.stderr)
    );
    assert_eq!(cache_miss.stdout, expected_output);
    let miss_trace = String::from_utf8_lossy(&cache_miss.stderr);
    assert!(
        miss_trace.contains("\"event\":\"cache_store\""),
        "first CLI run did not store the cache entry: {miss_trace}"
    );
    assert!(!miss_trace.contains("\"event\":\"cache_hit\""));
    assert!(!miss_trace.contains("\"event\":\"spawn\""));

    let cache_hit = run();
    assert!(
        cache_hit.status.success(),
        "second CLI run failed: {}",
        String::from_utf8_lossy(&cache_hit.stderr)
    );
    assert_eq!(cache_hit.stdout, expected_output);
    let hit_trace = String::from_utf8_lossy(&cache_hit.stderr);
    assert_eq!(hit_trace.matches("\"event\":\"cache_hit\"").count(), 1);
    assert!(!hit_trace.contains("\"event\":\"spawn\""));
}

#[test]
fn absence_and_failure_envelopes_route_as_distinct_declared_outcomes() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("outcome");
    executable(
        &fixture,
        "#!/bin/sh\nprintf '{\"ok\":true}\\n'\nexit \"$1\"\n",
    );
    let contract = json!({
        "version": 1,
        "output": {
            "type": "record",
            "encoding": "json",
            "fields": {"ok": {"type": "boolean"}},
            "required": ["ok"]
        },
        "result_envelope": true,
        "outcomes": [
            {"status": 0, "name": "success", "kind": "success"},
            {"status": 3, "name": "absent", "kind": "absence"},
            {"status": 4, "name": "failed", "kind": "failure"}
        ]
    });
    let catalog = registry(&[argument_manifest(
        "outcome.fixture",
        fixture.to_str().unwrap(),
        contract,
    )]);

    for (status, expected_kind) in [(3, "absence"), (4, "failure")] {
        let source = format!(
            "run outcome.fixture with status {status} as result and keep status as code\ntake text at \"/kind\" from result as kind\nif kind is \"{expected_kind}\"\n  run \"printf\" with arguments [\"%s:%s\", code, kind]\notherwise\n  run \"printf\" with arguments [\"wrong\"]\n"
        );
        let result = runtime::run(
            &parser::parse(&source).unwrap(),
            &catalog,
            &options(directory.path()),
        )
        .unwrap();
        assert_eq!(
            result.output,
            format!("{status}:{expected_kind}").as_bytes()
        );
    }

    let unretained = parser::parse("run outcome.fixture with status 3 as result\n").unwrap();
    let error = runtime::run(&unretained, &catalog, &options(directory.path()))
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("3"));
}

#[test]
fn helper_command_and_in_process_backends_have_identical_typed_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let helper = env!("CARGO_BIN_EXE_execute-applet");
    let params = BTreeMap::from([
        (
            "parts".into(),
            Value::List(vec![
                Value::String("north".into()),
                Value::String("star".into()),
            ]),
        ),
        ("separator".into(), Value::String("/".into())),
    ]);
    let manifest = json!({
        "name": "text.concat",
        "binary": "@execute-applet",
        "parameters": {
            "parts": {"type": "list", "items": "string", "required": true},
            "separator": {"type": "string"}
        },
        "arguments": [
            {"literal": "text.concat"},
            {"literal": "--args"},
            {"json_arguments": true}
        ],
        "contract": {
            "version": 1,
            "backend": "in_process",
            "output": {"type": "text", "encoding": "json"}
        }
    });
    let catalog = registry(&[manifest]);
    let in_process_result = runtime::invoke_applet(
        &catalog,
        "text.concat",
        params,
        Vec::new(),
        &options(directory.path()),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let external = std::process::Command::new(helper)
        .args([
            "text.concat",
            "--args",
            r#"{"parts":["north","star"],"separator":"/"}"#,
        ])
        .output()
        .unwrap();
    assert!(external.status.success());
    // In-process text results cross the boundary as raw UTF-8; the helper declares JSON.
    let helper_value: String = serde_json::from_slice(&external.stdout).unwrap();
    assert_eq!(in_process_result.output, helper_value.as_bytes());
    assert_eq!(in_process_result.output, b"north/star");
    assert_eq!(
        in_process_result.value,
        Some(Value::String("north/star".into()))
    );
    assert_eq!(in_process_result.status, 0);
}

#[test]
fn statically_known_output_type_fails_before_earlier_effects() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("effect-ran");
    let producer = directory.path().join("producer");
    executable(
        &producer,
        &format!(
            "#!/bin/sh\nprintf ran > {:?}\nprintf 'not an integer'\n",
            marker
        ),
    );
    let catalog = registry(&[
        json!({
            "name": "producer.fixture",
            "binary": producer,
            "arguments": [],
            "contract": {
                "version": 1,
                "output": {"type": "text", "encoding": "raw"}
            }
        }),
        json!({
            "name": "integer.consumer",
            "binary": "/bin/true",
            "parameters": {"value": {"type": "integer", "required": true}},
            "arguments": [{"parameter": "value"}]
        }),
    ]);
    let program =
        parser::parse("run producer.fixture as result\nrun integer.consumer with value result\n")
            .unwrap();
    let error = runtime::run(&program, &catalog, &options(directory.path()))
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("expects integer"));
    assert!(
        !marker.exists(),
        "static typed binding validation must precede effects"
    );
}
