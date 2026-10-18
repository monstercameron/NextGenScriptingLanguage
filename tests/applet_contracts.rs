use flow::manifest::Registry;
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn cli(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn value(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn manifest() -> Value {
    json!({
        "name": "z.contract", "phrases": ["contract phrase", "--alias"],
        "binary": "printf", "accepts_stdin": true, "cacheable": true,
        "parameters": {
            "text": {"type": "string", "required": true},
            "count": {"type": "integer", "required": true},
            "enabled": {"type": "boolean", "required": true},
            "tags": {"type": "list", "items": "string", "required": true, "min_items": 1},
            "optional": {"type": "integer"}
        },
        "arguments": [
            {"literal": "%s|%s|%s|%s|%s|%s"}, {"parameter": "text"},
            {"parameter": "count"}, {"parameter": "enabled"}, {"spread": "tags"},
            {"when_present": "optional", "append": [{"parameter": "optional"}]}
        ]
    })
}

fn write_manifest(path: &Path, manifest: &Value) {
    fs::write(path, serde_json::to_vec(manifest).unwrap()).unwrap();
}

#[test]
fn every_shipped_in_process_applet_has_an_intent_phrase() {
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    for entry in fs::read_dir(manifests).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let value: Value = serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        let name = value["name"].as_str().unwrap();
        if value["contract"]["backend"] == "in_process" {
            assert!(
                value["phrases"]
                    .as_array()
                    .is_some_and(|phrases| !phrases.is_empty()),
                "{name} lacks intent phrases"
            );
        }
    }
}

#[test]
fn discovery_is_deterministic_resolves_aliases_and_exposes_validated_types_and_argv() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = directory.path().join("catalog");
    fs::create_dir(&catalog).unwrap();
    write_manifest(&catalog.join("first.json"), &manifest());
    write_manifest(
        &catalog.join("last.json"),
        &json!({
            "name": "a.unavailable", "binary": "execute-no-such-applet-7c02a6",
            "arguments": [], "uncertain_on_failure": true
        }),
    );
    let path = catalog.to_str().unwrap();
    let result = value(cli(
        directory.path(),
        &["applets", "list", "--manifest-dir", path],
    ));
    assert_eq!(result["protocol"], "execute.applets/1");
    assert_eq!(result["validation"], "manifest_static");
    let contracts = result["applets"].as_array().unwrap();
    assert_eq!(
        contracts
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a.unavailable", "z.contract"]
    );
    assert_eq!(contracts[0]["execution"]["availability"], "not_checked");
    assert_eq!(contracts[0]["execution"]["uncertain_on_failure"], true);
    assert_eq!(contracts[0]["io"]["stdin"], "not_accepted");
    let inspect = value(cli(
        directory.path(),
        &["applets", "inspect", "z.contract", "--manifest-dir", path],
    ));
    assert_eq!(inspect["protocol"], "execute.applet/1");
    assert_eq!(inspect["applet"], contracts[1]);
    for args in [
        vec![
            "applets",
            "inspect",
            "contract phrase",
            "--manifest-dir",
            path,
        ],
        vec![
            "applets",
            "inspect",
            "--manifest-dir",
            path,
            "--",
            "--alias",
        ],
    ] {
        assert_eq!(value(cli(directory.path(), &args)), inspect);
    }
    let contract = &inspect["applet"];
    assert_eq!(
        contract["parameters_schema"]["required"],
        json!(["count", "enabled", "tags", "text"])
    );
    assert_eq!(contract["parameters_schema"]["additionalProperties"], false);
    let properties = &contract["parameters_schema"]["properties"];
    assert_eq!(
        properties["count"],
        json!({"type": "integer", "minimum": i64::MIN, "maximum": i64::MAX})
    );
    assert_eq!(properties["enabled"], json!({"type": "boolean"}));
    assert_eq!(
        properties["tags"],
        json!({"type": "array", "items": {"type": "string", "pattern": "^[^\\u0000]*$"}, "minItems": 1})
    );
    assert_eq!(
        contract["argv_template"],
        json!([
            {"kind":"literal", "literal":"%s|%s|%s|%s|%s|%s"},
            {"kind":"parameter", "parameter":"text"},
            {"kind":"parameter", "parameter":"count"},
            {"kind":"parameter", "parameter":"enabled"},
            {"kind":"spread", "spread":"tags"},
            {"kind":"when_present", "when_present":"optional", "append":[{"kind":"parameter", "parameter":"optional"}]}
        ])
    );
    assert_eq!(contract["io"]["stdout"], "bytes");
    assert!(contract["io"]["output_schema"].is_null());
    assert_eq!(contract["execution"]["cacheable"], true);
    assert_eq!(contract["execution"]["sandbox"], "not_enforced");
    assert_eq!(contract["execution"]["shell_expansion"], false);
    assert_eq!(
        value(cli(
            directory.path(),
            &["applets", "list", "--manifest-dir", path]
        )),
        result
    );
}

#[test]
fn runtime_builtin_is_identified_without_claiming_an_external_cd_process() {
    let directory = tempfile::tempdir().unwrap();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests/cd.json");
    let inspected = value(cli(
        directory.path(),
        &[
            "applets",
            "inspect",
            "cd",
            "--manifest-dir",
            path.to_str().unwrap(),
        ],
    ));
    let contract = &inspected["applet"];
    assert_eq!(contract["backend"], "builtin");
    assert_eq!(contract["builtin"], "cd");
    assert_eq!(contract["execution"]["availability"], "runtime_builtin");
    assert_eq!(contract["execution"]["effect"], "working_directory");
    assert_eq!(contract["io"]["stdout"], "empty");
    assert_eq!(contract["io"]["stdin"], "not_accepted");
}

#[test]
fn invalid_requests_and_invalid_catalogs_fail_without_partial_json() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("applet.json");
    write_manifest(&path, &manifest());
    let path = path.to_str().unwrap();
    for args in [
        vec!["applets"],
        vec!["applets", "unknown"],
        vec!["applets", "inspect"],
        vec!["applets", "list", "unexpected"],
        vec!["applets", "list", "--json"],
        vec!["applets", "list", "--manifest-dir"],
        vec!["applets", "list", "--manifest-dir", "--help"],
        vec![
            "applets",
            "list",
            "--manifest-dir",
            path,
            "--manifest-dir",
            path,
        ],
        vec!["applets", "inspect", "unknown", "--manifest-dir", path],
        vec!["applets", "list", "--manifest-dir", "missing.json"],
    ] {
        let output = cli(directory.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
    fs::write(path, "{\"name\":\"bad\"}").unwrap();
    let output = cli(
        directory.path(),
        &["applets", "list", "--manifest-dir", path],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let help = cli(directory.path(), &["applets", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("without executing applets"));
}

#[test]
fn contract_projection_preserves_checkpoint_serialization_and_tracks_catalog_changes() {
    let mut source = manifest();
    let registry = Registry::from_sources(&[source.to_string()]).unwrap();
    let manifest = registry.resolve("z.contract").unwrap();
    let before = serde_json::to_vec(manifest).unwrap();
    let first = manifest.contract();
    assert_eq!(serde_json::to_vec(manifest).unwrap(), before);
    assert!(!String::from_utf8(before)
        .unwrap()
        .contains("parameters_schema"));
    source["parameters"]["tags"]["min_items"] = 3.into();
    let changed = Registry::from_sources(&[source.to_string()]).unwrap();
    let second = changed.resolve("z.contract").unwrap().contract();
    assert_ne!(first, second);
    assert_eq!(
        second["parameters_schema"]["properties"]["tags"]["minItems"],
        3
    );
}

#[cfg(unix)]
#[test]
fn discovery_does_not_execute_even_an_available_applet() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("marker");
    fs::write(&binary, "#!/bin/sh\nprintf effect > forbidden\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let manifest = directory.path().join("applet.json");
    write_manifest(
        &manifest,
        &json!({"name":"marker", "binary":binary, "arguments":[]}),
    );
    for args in [
        vec!["applets", "list"],
        vec!["applets", "inspect", "marker"],
    ] {
        let mut args = args;
        args.extend(["--manifest-dir", manifest.to_str().unwrap()]);
        let _ = value(cli(directory.path(), &args));
        assert!(!directory.path().join("forbidden").exists());
    }
}

#[cfg(unix)]
#[test]
fn inspected_argument_template_matches_real_native_execution() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("applet.json");
    write_manifest(&path, &manifest());
    let source = "run z.contract with text \"雪; $HOME\" and count -7 and enabled false and tags [\"a b\", \"*\"] and optional 9\n";
    fs::write(directory.path().join("test.program"), source).unwrap();
    let program = flow::parser::parse(source).unwrap();
    let flow::ast::StepKind::Invoke { call, .. } = &program.steps[0].kind else {
        panic!("native call")
    };
    let registry = Registry::load(&path).unwrap();
    let manifest = registry.resolve("z.contract").unwrap();
    manifest.validate(call).unwrap();
    assert_eq!(
        manifest.build_arguments(&call.parameters).unwrap(),
        [
            "%s|%s|%s|%s|%s|%s",
            "雪; $HOME",
            "-7",
            "false",
            "a b",
            "*",
            "9"
        ]
    );
    let output = cli(
        directory.path(),
        &["test.program", "--manifest-dir", path.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, "雪; $HOME|-7|false|a b|*|9".as_bytes());
}
