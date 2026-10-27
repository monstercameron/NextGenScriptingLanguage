//! Static checking and execution agree for JSON-valued iteration inputs.
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn execute(root: &Path, source: &str, args: &[&str]) -> Output {
    fs::write(root.join("loop.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(root)
        .args(args)
        .arg("loop.program")
        .output()
        .unwrap()
}

#[test]
fn declared_json_array_default_passes_check_and_runs() {
    let root = tempfile::tempdir().unwrap();
    let source = "accept doc as json with default [{\"n\":\"a\"},{\"n\":\"b\"}]\nfor each item in doc\n  take text at \"/n\" from item as n\n  run \"printf\" with arguments [\"[%s]\", n]\n";
    let check = execute(root.path(), source, &["check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let run = execute(root.path(), source, &["run"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(run.stdout.ends_with(b"[b]"), "{:?}", run.stdout);
    let json_check = execute(root.path(), source, &["check", "--json"]);
    let report: Value = serde_json::from_slice(&json_check.stdout).unwrap();
    assert_eq!(report["ok"], true, "{report}");
}

#[test]
fn non_array_json_default_is_rejected_by_check_and_run() {
    let root = tempfile::tempdir().unwrap();
    let source = "accept doc as json with default {\"n\":\"a\"}\nfor each item in doc\n  then encode item as json\n";
    let json_check = execute(root.path(), source, &["check", "--json"]);
    assert!(!json_check.status.success());
    let report: Value = serde_json::from_slice(&json_check.stdout).unwrap();
    assert_eq!(report["diagnostics"].as_array().unwrap().len(), 1);
    assert_eq!(report["diagnostics"][0]["code"], "check.type_mismatch");
    assert_eq!(report["diagnostics"][0]["span"]["line"], 2);
    assert_eq!(report["diagnostics"][0]["span"]["precision"], "line");
    assert!(report["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("for each requires a JSON array or list of text"));
    fs::write(root.path().join("loop.program"), source).unwrap();
    let annotation = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(root.path())
        .args(["check", "loop.program", "--format", "github"])
        .output()
        .unwrap();
    assert!(!annotation.status.success());
    let text = String::from_utf8_lossy(&annotation.stdout);
    assert!(
        text.contains("line=2") && text.contains("title=check.type_mismatch"),
        "{text}"
    );
    for command in ["check", "run"] {
        let result = execute(root.path(), source, &[command]);
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("for each requires a JSON array or list of text"),
            "{command}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn unknown_json_input_is_checked_when_its_value_is_available() {
    let root = tempfile::tempdir().unwrap();
    let source = "accept doc as json\nfor each item in doc\n  then encode item as json\n";
    let check = execute(root.path(), source, &["check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    for (input, expected) in [("doc=[1,2]", true), ("doc={\"n\":1}", false)] {
        for command in ["check", "run"] {
            fs::write(root.path().join("loop.program"), source).unwrap();
            let result = Command::new(env!("CARGO_BIN_EXE_execute"))
                .current_dir(root.path())
                .args([command, "loop.program", "--input", input])
                .output()
                .unwrap();
            assert_eq!(
                result.status.success(),
                expected,
                "{command}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            if expected && command == "run" {
                assert_eq!(
                    serde_json::from_slice::<Value>(&result.stdout).unwrap(),
                    json!(2)
                );
            }
        }
    }
}

#[test]
fn explicit_input_overrides_non_array_default() {
    let root = tempfile::tempdir().unwrap();
    let source = "accept doc as json with default {\"n\":\"a\"}\nfor each item in doc\n  then encode item as json\n";
    fs::write(root.path().join("loop.program"), source).unwrap();
    for command in ["check", "run"] {
        let result = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(root.path())
            .args([command, "loop.program", "--input", "doc=[1,2]"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn json_applet_output_can_feed_a_loop_and_other_types_are_still_rejected() {
    let root = tempfile::tempdir().unwrap();
    let source = "run json.parse with text \"[1,2]\" as doc\nfor each item in doc\n  then encode item as json\n";
    for command in ["check", "run"] {
        let result = execute(root.path(), source, &[command]);
        assert!(
            result.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    for source in [
        "for each item in true\n  then encode item as json\n",
        "accept doc as text with default \"abc\"\nfor each item in doc\n  then encode item as json\n",
    ] {
        for command in ["check", "run"] {
            let result = execute(root.path(), source, &[command]);
            assert!(!result.status.success(), "{command} accepted {source}");
        }
    }
}

#[test]
fn list_sort_preserves_text_items_through_loop_bindings_and_argv() {
    let root = tempfile::tempdir().unwrap();
    let source = "run list.sort with items [\"b\", \"a\"] as sorted\nfor each x in sorted\n  run \"printf\" with arguments [\"%s\", x]\n";
    let check = execute(root.path(), source, &["check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let run = execute(root.path(), source, &["run"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"ab");
}

#[test]
fn list_sort_preserves_integer_items_through_loop_bindings_and_argv() {
    let root = tempfile::tempdir().unwrap();
    let source = "run list.sort with items [2, 1] as sorted\nfor each x in sorted\n  run \"printf\" with arguments [\"%s\", x]\n";
    let check = execute(root.path(), source, &["check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let run = execute(root.path(), source, &["run"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"12");
}

#[test]
fn unsupported_json_array_items_keep_conservative_loop_types() {
    let cases = ["[true, false]", "[1.5, 2.5]", "[]", "[[1], [2]]"];
    for items in cases {
        let root = tempfile::tempdir().unwrap();
        let source =
            format!("for each x in {items}\n  run \"printf\" with arguments [\"%s\", x]\n");
        let check = execute(root.path(), &source, &["check"]);
        assert!(
            check.status.success(),
            "check failed for {items}: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        let run = execute(root.path(), &source, &["run"]);
        assert!(
            run.status.success(),
            "run failed for {items}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}
