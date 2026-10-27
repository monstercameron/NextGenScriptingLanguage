use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn invoke(dir: &Path, source: &str, command: &str, extra: &[&str]) -> Output {
    let program = dir.join("case.program");
    fs::write(&program, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir)
        .arg(command)
        .arg(program)
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn alternatives_are_checked_and_valid_forms_execute() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("template.txt"), "hello").unwrap();
    let prefix = "run json.parse with text \"{}\" as v\n";
    for source in [
        "run text.split with text \"a,b\" and separator \",\"\n".to_owned(),
        "run text.split with text \"abc\" and nul true\n".to_owned(),
        "run text.split with text \"abc\" and separator \",\" and nul false\n".to_owned(),
        "run time.sleep with milliseconds 0\n".to_owned(),
        "run time.sleep with seconds 0\n".to_owned(),
        "run time.sleep with seconds 0.001\n".to_owned(),
        "run time.sleep with milliseconds 0 and seconds 90000\n".to_owned(),
        "run time.sleep with milliseconds -1 and seconds 0\n".to_owned(),
        format!("{prefix}run text.render with values v and template \"hello\"\n"),
        format!("{prefix}run text.render with values v and template_path \"template.txt\"\n"),
    ] {
        for command in ["check", "run"] {
            let output = invoke(dir.path(), &source, command, &[]);
            assert!(
                output.status.success(),
                "{command}: {source}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn duration_upper_bounds_check_without_waiting_a_day() {
    let dir = tempfile::tempdir().unwrap();
    for source in [
        "run time.sleep with milliseconds 86400000\n",
        "run time.sleep with seconds 86400\n",
        "run time.sleep with milliseconds -1 and seconds 86400\n",
    ] {
        let output = invoke(dir.path(), source, "check", &[]);
        assert!(
            output.status.success(),
            "{source}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn impossible_alternatives_fail_static_check() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = "run json.parse with text \"{}\" as v\n";
    for source in [
        "run text.split with text \"abc\"\n".to_owned(),
        "run text.split with text \"abc\" and nul false\n".to_owned(),
        "run text.split with text \"abc\" and separator \"\"\n".to_owned(),
        "run text.split with text \"abc\" and separator \",\" and nul true\n".to_owned(),
        "run time.sleep\n".to_owned(),
        "run time.sleep with milliseconds -1\n".to_owned(),
        "run time.sleep with seconds -1\n".to_owned(),
        "run time.sleep with seconds -0.1\n".to_owned(),
        "run time.sleep with seconds 86400.001\n".to_owned(),
        "run time.sleep with milliseconds 86400001 and seconds 0\n".to_owned(),
        format!("{prefix}run text.render with values v\n"),
        format!(
            "{prefix}run text.render with values v and template \"a\" and template_path \"b\"\n"
        ),
    ] {
        for command in ["check", "run"] {
            let output = invoke(dir.path(), &source, command, &[]);
            assert!(!output.status.success(), "{command} accepted {source}");
        }
    }
}

#[test]
fn unknown_values_are_deferred_to_execution() {
    let dir = tempfile::tempdir().unwrap();
    let source =
        "accept separator as text\nrun text.split with text \"a,b\" and separator separator\n";
    let checked = invoke(dir.path(), source, "check", &[]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let valid = invoke(dir.path(), source, "run", &["--input", "separator=,"]);
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let invalid = invoke(dir.path(), source, "run", &["--input", "separator="]);
    assert!(!invalid.status.success());
    let boolean_source = "accept flag as boolean\nrun text.split with text \"a,b\" and separator \",\" and nul flag\n";
    assert!(invoke(dir.path(), boolean_source, "check", &[])
        .status
        .success());
    assert!(invoke(
        dir.path(),
        boolean_source,
        "run",
        &["--input", "flag=false"]
    )
    .status
    .success());
    assert!(
        !invoke(dir.path(), boolean_source, "run", &["--input", "flag=true"])
            .status
            .success()
    );
    let duration = "accept delay as integer\nrun time.sleep with milliseconds delay\n";
    assert!(invoke(dir.path(), duration, "check", &[]).status.success());
    assert!(invoke(dir.path(), duration, "run", &["--input", "delay=0"])
        .status
        .success());
    assert!(
        !invoke(dir.path(), duration, "run", &["--input", "delay=-1"])
            .status
            .success()
    );
    fs::write(dir.path().join("template.txt"), "hello").unwrap();
    let render = "accept location as text\nrun json.parse with text \"{}\" as v\nrun text.render with values v and template_path location\n";
    assert!(invoke(dir.path(), render, "check", &[]).status.success());
    assert!(invoke(
        dir.path(),
        render,
        "run",
        &["--input", "location=template.txt"]
    )
    .status
    .success());
    assert!(
        !invoke(dir.path(), render, "run", &["--input", "location="])
            .status
            .success()
    );
}

#[test]
fn malformed_alternative_rules_are_rejected_at_manifest_load() {
    use flow::manifest::Registry;
    use serde_json::json;
    for rules in [
        json!([{"message":"choose","any_of":[[{"test":"present","parameter":"missing"}]]}]),
        json!([{"message":"choose","any_of":[[]]}]),
        json!([{"message":"choose","any_of":[[{"test":"boolean","parameter":"value","value":true}]]}]),
        json!([{"message":"choose","any_of":[[{"test":"number_range","parameter":"value","minimum":2,"maximum":1}]]}]),
        json!([{"message":"choose","any_of":[[{"test":"number_range","parameter":"value","minimum":0,"maximum":9007199254740993_u64}]]}]),
    ] {
        let source = json!({"name":"fixture", "binary":"fixture", "parameters":{"value":{"type":"integer"}}, "arguments":[], "parameter_alternatives":rules}).to_string();
        assert!(Registry::from_sources(&[source]).is_err());
    }
}

#[test]
fn invalid_alternatives_also_fail_the_helper() {
    use serde_json::json;
    for (operation, args) in [
        ("text.split", json!({"text":"abc"})),
        (
            "text.split",
            json!({"text":"abc", "separator":"", "nul":false}),
        ),
        (
            "text.split",
            json!({"text":"abc", "separator":",", "nul":true}),
        ),
        ("time.sleep", json!({})),
        ("time.sleep", json!({"milliseconds":86400001, "seconds":0})),
        ("text.render", json!({"values":{}})),
        (
            "text.render",
            json!({"values":{}, "template":"a", "template_path":"b"}),
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_execute-applet"))
            .args([operation, "--args", &args.to_string()])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{operation}: {args}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn inspection_exposes_alternatives_in_parameter_schema() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests/core.text_split.json"),
    )
    .unwrap();
    let registry = flow::manifest::Registry::from_sources(&[source]).unwrap();
    let contract = registry.resolve("text.split").unwrap().contract();
    assert!(contract["parameter_alternatives"].as_array().unwrap().len() == 1);
    assert!(
        contract["parameters_schema"]["allOf"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
}

#[test]
fn custom_manifest_alternatives_are_checked_again_after_resolution() {
    use flow::ast::{Invocation, Value};
    use flow::manifest::Registry;
    use serde_json::json;
    use std::collections::BTreeMap;
    let source = json!({
        "name":"fixture", "binary":"fixture",
        "parameters":{"text":{"type":"string"}},
        "parameter_alternatives":[{"message":"text must be nonempty", "any_of":[[{"test":"nonempty", "parameter":"text"}]]}],
        "arguments":[{"json_arguments":true}]
    }).to_string();
    let registry = Registry::from_sources(&[source]).unwrap();
    let manifest = registry.resolve("fixture").unwrap();
    let invocation = Invocation {
        operation: "fixture".into(),
        direct: false,
        discard_stderr: false,
        parameters: BTreeMap::from([("text".into(), Value::Reference("input".into()))]),
        binding: None,
        cache: false,
        status_binding: None,
        result_binding: None,
        retry: None,
    };
    assert!(manifest.validate(&invocation).is_ok());
    assert!(manifest
        .build_arguments(&BTreeMap::from([("text".into(), Value::String("".into()))]))
        .is_err());
    assert!(manifest
        .build_arguments(&BTreeMap::from([(
            "text".into(),
            Value::String("ok".into())
        )]))
        .is_ok());
}

#[test]
fn bundled_catalog_applies_alternatives() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("missing.program");
    fs::write(&program, "run time.sleep\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .arg("check")
        .arg(program)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("bounded nonnegative"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
