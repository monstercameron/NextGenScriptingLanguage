//! Source-manifest contract checks for list shaping and top selection.
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn invoke(root: &Path, source: &str, command: &str) -> Output {
    fs::write(root.join("case.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(root)
        .args([command, "case.program", "--manifest-dir"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .output()
        .unwrap()
}

#[test]
fn each_required_shaping_argument_is_rejected_before_execution() {
    let root = tempfile::tempdir().unwrap();
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("list.range", &[("start", "1"), ("stop", "4")]),
        ("list.enumerate", &[("items", "[1,2,3]")]),
        ("list.first", &[("items", "[1,2,3]")]),
        ("list.last", &[("items", "[1,2,3]")]),
        ("list.rest", &[("items", "[1,2,3]")]),
        ("list.reverse", &[("items", "[1,2,3]")]),
        ("list.concat", &[("items", "[[1],[2,3]]")]),
        ("list.flatten", &[("items", "[[1,[2]],3]")]),
        (
            "list.slice",
            &[("items", "[1,2,3]"), ("offset", "1"), ("length", "1")],
        ),
        ("list.chunk", &[("items", "[1,2,3]"), ("size", "2")]),
        ("list.zip", &[("left", "[1,2]"), ("right", "[3,4]")]),
    ];
    for (name, arguments) in cases {
        for (omitted, _) in *arguments {
            let present = arguments
                .iter()
                .filter(|(key, _)| key != omitted)
                .map(|(key, value)| format!("{key} {value}"))
                .collect::<Vec<_>>()
                .join(" and ");
            let source = if present.is_empty() {
                format!("run {name}\n")
            } else {
                format!("run {name} with {present}\n")
            };
            for command in ["check", "run"] {
                let output = invoke(root.path(), &source, command);
                assert!(!output.status.success(), "{command} accepted {source}");
                let diagnostic = String::from_utf8_lossy(&output.stderr);
                assert!(
                    diagnostic.contains(&format!("requires parameter \"{omitted}\"")),
                    "{command}: {source}: {diagnostic}"
                );
            }
        }
    }
}

#[test]
fn complete_shaping_calls_check_and_run_and_optional_defaults_work() {
    let root = tempfile::tempdir().unwrap();
    let cases: &[(&str, Value)] = &[
        ("list.range with start 1 and stop 4", json!([1, 2, 3])),
        (
            "list.range with start 1 and stop 6 and step 2",
            json!([1, 3, 5]),
        ),
        (
            "list.enumerate with items [1,2]",
            json!([{"index":0,"item":1},{"index":1,"item":2}]),
        ),
        ("list.first with items [1,2,3]", json!(1)),
        ("list.last with items [1,2,3]", json!(3)),
        ("list.rest with items [1,2,3]", json!([2, 3])),
        ("list.reverse with items [1,2,3]", json!([3, 2, 1])),
        ("list.concat with items [[1],[2,3]]", json!([1, 2, 3])),
        ("list.flatten with items [[1,[2]],3]", json!([1, 2, 3])),
        (
            "list.flatten with items [[1,[2]],3] and depth 1",
            json!([1, [2], 3]),
        ),
        (
            "list.slice with items [1,2,3] and offset 1 and length 1",
            json!([2]),
        ),
        (
            "list.chunk with items [1,2,3] and size 2",
            json!([[1, 2], [3]]),
        ),
        (
            "list.zip with left [1,2] and right [3,4]",
            json!([[1, 3], [2, 4]]),
        ),
    ];
    for (call, expected) in cases {
        let source = format!("run {call} as result\nthen encode result as json\n");
        for command in ["check", "run"] {
            let output = invoke(root.path(), &source, command);
            assert!(
                output.status.success(),
                "{command} {call}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if command == "run" {
                let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(&actual, expected, "{call}");
            }
        }
    }
}

#[test]
fn top_uses_root_by_default_and_accepts_explicit_key() {
    let root = tempfile::tempdir().unwrap();
    for (source, expected) in [
        ("run list.top with items [3,1,2] and n 2 as result\nthen encode result as json\n", json!([3,2])),
        ("run json.parse with text \"[{\\\"score\\\":1},{\\\"score\\\":3},{\\\"score\\\":2}]\" as values\nrun list.top with items values and key \"/score\" and n 2 as result\nthen encode result as json\n", json!([{"score":3},{"score":2}])),
    ] {
        for command in ["check", "run"] {
            let output = invoke(root.path(), source, command);
            assert!(output.status.success(), "{command}: {}", String::from_utf8_lossy(&output.stderr));
            if command == "run" {
                let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(actual, expected);
            }
        }
    }
}

#[test]
fn bundled_registry_checks_required_parameters_and_top_default() {
    let root = tempfile::tempdir().unwrap();
    for (source, expected_success) in [
        ("run list.range with stop 4\n", false),
        ("run list.top with items [3,1,2]\n", false),
        ("run list.top with items [3,1,2] and n 2\n", true),
        ("run list.count\n", true),
    ] {
        fs::write(root.path().join("case.program"), source).unwrap();
        for command in ["check", "run"] {
            let output = Command::new(env!("CARGO_BIN_EXE_execute"))
                .current_dir(root.path())
                .args([command, "case.program"])
                .output()
                .unwrap();
            assert_eq!(
                output.status.success(),
                expected_success,
                "{command} {source}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
