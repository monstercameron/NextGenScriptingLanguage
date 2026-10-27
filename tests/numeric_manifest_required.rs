use serde_json::{json, Value};
use std::{fs, process::Command};

#[test]
fn numeric_checks_require_operands_and_valid_runs_keep_optional_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("case.program");
    type Argument = (&'static str, &'static str, Value);
    type Case = (&'static str, Vec<Argument>);
    let cases: Vec<Case> = vec![
        ("abs", vec![("value", "-3", json!(-3))]),
        ("ceil", vec![("value", "3", json!(3))]),
        ("floor", vec![("value", "3", json!(3))]),
        ("round", vec![("value", "3", json!(3))]),
        ("sign", vec![("value", "-3", json!(-3))]),
        ("trunc", vec![("value", "3", json!(3))]),
        ("format", vec![("value", "3", json!(3))]),
        ("format_size", vec![("value", "3", json!(3))]),
        (
            "clamp",
            vec![
                ("value", "3", json!(3)),
                ("min", "1", json!(1)),
                ("max", "2", json!(2)),
            ],
        ),
        (
            "divmod",
            vec![("left", "7", json!(7)), ("right", "2", json!(2))],
        ),
        (
            "int_divide",
            vec![("left", "7", json!(7)), ("right", "2", json!(2))],
        ),
        (
            "modulo",
            vec![("left", "7", json!(7)), ("right", "2", json!(2))],
        ),
        ("min", vec![("values", "v", json!([3, 1]))]),
        ("max", vec![("values", "v", json!([3, 1]))]),
    ];
    for (op, args) in cases {
        let invoke = |action: &str, omitted: Option<&str>| {
            let arguments: Vec<String> = args
                .iter()
                .filter(|(name, _, _)| Some(*name) != omitted)
                .map(|(name, expression, _)| format!("{name} {expression}"))
                .collect();
            let line = if arguments.is_empty() {
                format!("run number.{op}\n")
            } else {
                format!("run number.{op} with {}\n", arguments.join(" and "))
            };
            fs::write(
                &file,
                format!("run json.parse with text \"[3,1]\" as v\n{line}"),
            )
            .unwrap();
            Command::new(env!("CARGO_BIN_EXE_execute"))
                .arg(action)
                .arg(&file)
                .arg("--manifest-dir")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
                .output()
                .unwrap()
        };
        for (name, _, _) in &args {
            let output = invoke("check", Some(name));
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && error.contains(name),
                "number.{op} missing {name}: {error}"
            );
        }
        let check = invoke("check", None);
        assert!(
            check.status.success(),
            "number.{op}: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        let run = invoke("run", None);
        assert!(
            run.status.success(),
            "number.{op}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let map = args
            .iter()
            .map(|(key, _, value)| ((*key).to_owned(), value.clone()))
            .collect();
        let expected = flow::applets::execute(&format!("number.{op}"), &map, b"").unwrap();
        if let Some(text) = expected.as_str() {
            assert_eq!(
                String::from_utf8_lossy(&run.stdout).trim_end_matches('\n'),
                text,
                "number.{op}"
            );
        } else {
            assert_eq!(
                serde_json::from_slice::<Value>(&run.stdout).unwrap(),
                expected,
                "number.{op}"
            );
        }
    }
}

#[test]
fn rounding_and_format_controls_remain_optional_and_usable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("controls.program");
    for (source, expected) in [
        (
            "run number.round with value 123 and digits -1\n",
            json!(120),
        ),
        (
            "run number.format with value 123 and precision 2 and grouping true and percent true\n",
            json!("123.00%"),
        ),
        (
            "run number.format_size with value 1000 and system \"si\" and precision 0\n",
            json!("1 kB"),
        ),
    ] {
        fs::write(&path, source).unwrap();
        for action in ["check", "run"] {
            let out = Command::new(env!("CARGO_BIN_EXE_execute"))
                .arg(action)
                .arg(&path)
                .arg("--manifest-dir")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{source}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            if action == "run" {
                if let Some(text) = expected.as_str() {
                    assert_eq!(
                        String::from_utf8_lossy(&out.stdout).trim_end_matches('\n'),
                        text
                    );
                } else {
                    assert_eq!(
                        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
                        expected
                    );
                }
            }
        }
    }
}

#[test]
fn default_catalog_checks_and_runs_numeric_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("default.program");
    let invoke = |source: &str, action: &str| {
        fs::write(&path, source).unwrap();
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(dir.path())
            .arg(action)
            .arg(&path)
            .output()
            .unwrap()
    };
    for source in [
        "run number.round with digits 1\n",
        "run number.clamp with value 3 and max 5\n",
        "run number.divmod with left 7\n",
        "run number.min\n",
    ] {
        let output = invoke(source, "check");
        assert!(!output.status.success(), "{source}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires parameter"));
    }
    for (source, expected) in [
        (
            "run number.round with value 125 and digits -1\n",
            json!(130),
        ),
        (
            "run number.divmod with left 7 and right 2\n",
            json!({"quotient":3,"remainder":1}),
        ),
    ] {
        for action in ["check", "run"] {
            let output = invoke(source, action);
            assert!(
                output.status.success(),
                "{source}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if action == "run" {
                assert_eq!(
                    serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                    expected
                );
            }
        }
    }
}
