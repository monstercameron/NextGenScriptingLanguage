use flow::manifest::Registry;
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
fn integer_domain_constraints_preserve_exact_unsigned_values() {
    let dir = tempfile::tempdir().unwrap();
    let invalid_sources = [
        "run list.chunk with items [1,2] and size 0\n",
        "run list.range with start 0 and stop 2 and step 0\n",
        "run list.range with start 18446744073709551615 and stop 2\n",
        "run list.range with start 0 and stop 2 and step 18446744073709551615\n",
        "run list.top with items [1,2] and n -1\n",
        "run text.pad with text \"x\" and width -1\n",
        "run text.truncate with text \"abc\" and width -1\n",
        "run text.repeat with text \"x\" and count -1\n",
    ];
    for source in invalid_sources {
        for command in ["check", "run"] {
            let output = invoke(dir.path(), source, command, &[]);
            assert!(
                !output.status.success(),
                "{command} accepted invalid integer domain: {source}"
            );
        }
    }
    for (source, should_pass) in [
        ("run text.pad with text \"x\" and width 16777216\n", true),
        (
            "run text.pad with text \"x\" and width 9007199254740993\n",
            false,
        ),
    ] {
        let output = invoke(dir.path(), source, "check", &[]);
        assert_eq!(
            output.status.success(),
            should_pass,
            "unexpected text.pad bound result for {source}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let signed_reference = "accept n as integer\nrun list.chunk with items [1,2] and size n\n";
    let checked = invoke(dir.path(), signed_reference, "check", &[]);
    assert!(
        checked.status.success(),
        "in-range signed workflow references should be deferred to runtime: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let valid_signed_reference = invoke(dir.path(), signed_reference, "run", &["--input", "n=1"]);
    assert!(
        valid_signed_reference.status.success(),
        "positive signed workflow reference was rejected: {}",
        String::from_utf8_lossy(&valid_signed_reference.stderr)
    );
    let invalid_signed_reference =
        invoke(dir.path(), signed_reference, "run", &["--input", "n=-1"]);
    assert!(
        !invalid_signed_reference.status.success(),
        "negative signed workflow reference entered an unsigned parameter"
    );

    for (source, supplied) in [
        (
            "accept n as integer\nrun list.chunk with items [1,2] and size n\n",
            "n=0",
        ),
        (
            "accept n as integer\nrun list.range with start 0 and stop 2 and step n\n",
            "n=0",
        ),
        (
            "accept n as integer\nrun list.top with items [1,2] and n n\n",
            "n=-1",
        ),
        (
            "accept n as integer\nrun text.pad with text \"x\" and width n\n",
            "n=-1",
        ),
        (
            "accept n as integer\nrun text.truncate with text \"abc\" and width n\n",
            "n=-1",
        ),
        (
            "accept n as integer\nrun text.repeat with text \"x\" and count n\n",
            "n=-1",
        ),
    ] {
        let output = invoke(dir.path(), source, "run", &["--input", supplied]);
        assert!(
            !output.status.success(),
            "runtime accepted dynamic integer outside the manifest domain: {source}"
        );
    }

    let above_binary64 = "9007199254740993";
    let mut valid_sources = vec![format!(
        "run list.range with start 2 and stop 0 and step -{above_binary64}\n"
    )];
    if usize::MAX as u128 >= above_binary64.parse::<u128>().unwrap() {
        valid_sources.extend([
            format!("run list.chunk with items [1,2] and size {above_binary64}\n"),
            format!("run list.top with items [1,2] and n {above_binary64}\n"),
            format!("run text.truncate with text \"abc\" and width {above_binary64}\n"),
            format!("run text.repeat with text \"\" and count {above_binary64}\n"),
        ]);
    }
    let max_usize = usize::MAX.to_string();
    valid_sources.extend([
        format!("run list.chunk with items [1,2] and size {max_usize}\n"),
        format!("run list.top with items [1,2] and n {max_usize}\n"),
        format!("run text.truncate with text \"abc\" and width {max_usize}\n"),
        format!("run text.repeat with text \"\" and count {max_usize}\n"),
    ]);
    if usize::MAX as u128 >= u64::MAX as u128 {
        valid_sources.extend([
            "run list.chunk with items [1,2] and size 18446744073709551615\n".to_owned(),
            "run list.top with items [1,2] and n 18446744073709551615\n".to_owned(),
            "run text.truncate with text \"abc\" and width 18446744073709551615\n".to_owned(),
            "run text.repeat with text \"\" and count 18446744073709551615\n".to_owned(),
        ]);
    }
    for source in valid_sources {
        for command in ["check", "run"] {
            let output = invoke(dir.path(), &source, command, &[]);
            assert!(
                output.status.success(),
                "{command} rejected exact large integer: {source}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    if (usize::MAX as u128) < u64::MAX as u128 {
        let too_large_for_usize = (usize::MAX as u128 + 1).to_string();
        let source = format!("run list.chunk with items [1,2] and size {too_large_for_usize}\n");
        let checked = invoke(dir.path(), &source, "check", &[]);
        assert!(
            !checked.status.success(),
            "static validation accepted a value outside usize on this target"
        );
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
fn applet_nonempty_parameters_reject_known_empty_and_keep_optional_fill_default() {
    let dir = tempfile::tempdir().unwrap();
    let empty_cases = [
        (
            "run text.pad with text \"x\" and width 3 and fill \"\"\n",
            "fill",
        ),
        (
            "run text.replace with text \"abc\" and old \"\" and new \"x\"\n",
            "old",
        ),
        ("run fs.glob with pattern \"\"\n", "pattern"),
        ("run text.count_lines_by with delimiter \"\"\n", "delimiter"),
    ];

    for (source, parameter) in empty_cases {
        for command in ["check", "run"] {
            let output = invoke(dir.path(), source, command, &[]);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains(parameter),
                "{command} accepted empty {parameter}: {stderr}"
            );
        }
    }

    // `fill` stays optional and the applet's existing default remains in effect.
    let without_fill = "run text.pad with text \"x\" and width 3\n";
    for command in ["check", "run"] {
        let output = invoke(dir.path(), without_fill, command, &[]);
        assert!(
            output.status.success(),
            "{command} rejected absent fill: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if command == "run" {
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim_end_matches('\n'),
                "x  "
            );
        }
    }
}

#[test]
fn applet_nonempty_parameters_still_validate_dynamic_values_at_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "fill",
            "run text.pad with text \"x\" and width 3 and fill fill\n",
        ),
        (
            "old",
            "run text.replace with text \"abc\" and old old and new \"x\"\n",
        ),
        ("pattern", "run fs.glob with pattern pattern\n"),
        (
            "delimiter",
            "run text.count_lines_by with delimiter delimiter\n",
        ),
    ];

    for (parameter, source) in cases {
        let source = format!("accept {parameter} as text\n{source}");
        let checked = invoke(dir.path(), &source, "check", &[]);
        assert!(
            checked.status.success(),
            "static check rejected dynamic {parameter}: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
        let supplied = format!("{parameter}=");
        let output = invoke(dir.path(), &source, "run", &["--input", supplied.as_str()]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success()
                && stderr.contains(parameter)
                && (stderr.contains("nonempty") || stderr.contains("cannot be empty")),
            "runtime accepted empty dynamic {parameter}: {stderr}"
        );
    }
}

#[test]
fn optional_numeric_ranges_reject_literals_and_resolved_values() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "number.parse_integer",
            "base",
            "base must be an integer from 2 through 36",
            "run number.parse_integer with text \"10\"",
            2,
            36,
            1,
            37,
        ),
        (
            "number.parse_integer.result",
            "base",
            "base must be an integer from 2 through 36",
            "run number.parse_integer.result with text \"10\"",
            2,
            36,
            1,
            37,
        ),
        (
            "number.format",
            "precision",
            "precision must be an integer from 0 through 15",
            "run number.format with value 1",
            0,
            15,
            -1,
            16,
        ),
        (
            "number.format_size",
            "precision",
            "precision must be an integer from 0 through 6",
            "run number.format_size with value 100",
            0,
            6,
            -1,
            7,
        ),
    ];

    for (operation, parameter, message, prefix, minimum, maximum, below, above) in cases {
        for invalid in [below, above] {
            let source = format!("{prefix} and {parameter} {invalid}\n");
            for command in ["check", "run"] {
                let output = invoke(dir.path(), &source, command, &[]);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    !output.status.success() && stderr.contains(message),
                    "{command} accepted {operation} {parameter}={invalid}: {stderr}"
                );
            }
        }

        // The option remains optional, and both inclusive endpoints are valid.
        for source in [
            format!("{prefix}\n"),
            format!("{prefix} and {parameter} {minimum}\n"),
            format!("{prefix} and {parameter} {maximum}\n"),
        ] {
            for command in ["check", "run"] {
                let output = invoke(dir.path(), &source, command, &[]);
                assert!(
                    output.status.success(),
                    "{command} rejected valid {operation} call {source:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        // References cannot be range-checked statically, but runtime resolves
        // their values before invoking the applet and rejects out-of-range data.
        let dynamic_source = format!("accept option as integer\n{prefix} and {parameter} option\n");
        for invalid in [below, above] {
            let supplied = format!("option={invalid}");
            let output = invoke(
                dir.path(),
                &dynamic_source,
                "run",
                &["--input", supplied.as_str()],
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains(message),
                "runtime accepted referenced {operation} {parameter}={invalid}: {stderr}"
            );
        }
        for valid in [minimum, maximum] {
            let supplied = format!("option={valid}");
            let output = invoke(
                dir.path(),
                &dynamic_source,
                "run",
                &["--input", supplied.as_str()],
            );
            assert!(
                output.status.success(),
                "runtime rejected referenced {operation} {parameter}={valid}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn malformed_alternative_rules_are_rejected_at_manifest_load() {
    use flow::manifest::Registry;
    use serde_json::json;
    for rules in [
        json!([{"message":"choose","any_of":[[{"test":"present","parameter":"missing"}]]}]),
        json!([{"message":"choose","any_of":[[]]}]),
        json!([{"message":"choose","any_of":[[{"test":"boolean","parameter":"value","value":true}]]}]),
        json!([{"message":"choose","any_of":[[{"test":"string_not_equals","parameter":"value","value":"special"}]]}]),
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
        sensitive_binding: false,
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
fn conditional_string_requirements_defer_references_and_project_to_schema() {
    use flow::ast::{Invocation, Value};
    use flow::manifest::Registry;
    use serde_json::json;
    use std::collections::BTreeMap;

    let source = json!({
        "name":"conditional.fixture", "binary":"fixture",
        "parameters":{
            "policy":{"type":"string","required":true},
            "value":{"type":"integer"}
        },
        "parameter_alternatives":[{
            "message":"value is required for the special policy",
            "any_of":[
                [{"test":"string_not_equals","parameter":"policy","value":"special"}],
                [{"test":"present","parameter":"value"}]
            ]
        }],
        "arguments":[{"json_arguments":true}]
    })
    .to_string();
    let registry = Registry::from_sources(&[source]).unwrap();
    let manifest = registry.resolve("conditional.fixture").unwrap();
    let contract = manifest.contract();
    assert_eq!(
        contract["parameters_schema"]["allOf"][0]["anyOf"][0]["allOf"][0]["properties"]["policy"]
            ["not"]["const"],
        "special"
    );

    let invocation = |policy, value| Invocation {
        operation: "conditional.fixture".into(),
        direct: false,
        discard_stderr: false,
        parameters: BTreeMap::from_iter(std::iter::once(("policy".into(), policy)).chain(value)),
        binding: None,
        cache: false,
        sensitive_binding: false,
        status_binding: None,
        result_binding: None,
        retry: None,
    };
    assert!(manifest
        .validate(&invocation(Value::String("ordinary".into()), None))
        .is_ok());
    assert!(manifest
        .validate(&invocation(Value::String("special".into()), None))
        .is_err());
    assert!(manifest
        .validate(&invocation(Value::Reference("dynamic_policy".into()), None))
        .is_ok());
    assert!(manifest
        .validate(&invocation(
            Value::String("special".into()),
            Some(("value".into(), Value::Integer(1)))
        ))
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

#[test]
fn number_min_max_require_nonempty_numeric_lists_but_defer_json_values() {
    let dir = tempfile::tempdir().unwrap();
    let registry =
        Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")).unwrap();
    for operation in ["number.min", "number.max"] {
        let manifest = registry.resolve(operation).unwrap();
        assert!(!manifest.accepts_stdin);
        assert_eq!(
            manifest.contract()["parameters_schema"]["properties"]["values"],
            serde_json::json!({
                "type":"array", "items":{"type":"number"}, "minItems":1
            })
        );
        for values in ["{}", "[]", "[1, \"x\"]"] {
            let source = format!("run {operation} with values {values} as result\n");
            let output = invoke(dir.path(), &source, "check", &[]);
            assert!(
                !output.status.success(),
                "{operation} accepted invalid literal list {values}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        for values in ["[1, 2]", "[1.25, 2]"] {
            let source = format!("run {operation} with values {values} as result\n");
            let output = invoke(dir.path(), &source, "check", &[]);
            assert!(
                output.status.success(),
                "{operation} rejected numeric list {values}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let dynamic =
            format!("accept values as json\nrun {operation} with values values as result\n");
        let output = invoke(dir.path(), &dynamic, "check", &[]);
        assert!(
            output.status.success(),
            "{operation} rejected dynamic JSON input: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let invalid_dynamic = format!(
            "run json.parse with text \"[1, \\\"x\\\"]\" as values\nrun {operation} with values values\n"
        );
        let checked = invoke(dir.path(), &invalid_dynamic, "check", &[]);
        assert!(
            checked.status.success(),
            "{operation} rejected a deferred JSON shape: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
        let run = invoke(dir.path(), &invalid_dynamic, "run", &[]);
        assert!(
            !run.status.success(),
            "{operation} did not validate dynamic values at execution"
        );
    }
}
