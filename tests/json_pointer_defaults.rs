use flow::applets::{self, AppletErrorKind};
use serde_json::{json, Value};
use std::{fs, process::Command};

#[test]
fn omitted_pointer_means_each_item_and_invalid_explicit_pointers_still_fail() {
    for (operation, expected) in [
        ("json.sort_by", json!([1, 1, 3])),
        ("json.unique_by", json!([3, 1])),
        ("json.group_by", json!([[1, 1], [3]])),
    ] {
        let args = json!({"document":[3,1,1], "numeric":true});
        let mut args = args.as_object().unwrap().clone();
        let omitted = applets::execute(operation, &args, b"").unwrap();
        args.insert("pointer".into(), json!(""));
        let explicit = applets::execute(operation, &args, b"").unwrap();
        assert_eq!(omitted, expected, "{operation}");
        assert_eq!(explicit, expected, "{operation}");
        for bad in [json!(null), json!(3), json!("invalid"), json!("/~2")] {
            args.insert("pointer".into(), bad);
            let error = applets::execute(operation, &args, b"").unwrap_err();
            assert_eq!(error.kind, AppletErrorKind::Permanent, "{operation}");
            assert_eq!(error.code, "invalid_request");
        }
    }
}

#[test]
fn check_and_runtime_agree_for_omitted_and_explicit_empty_pointers() {
    let dir = tempfile::tempdir().unwrap();
    for (operation, expected) in [
        ("json.sort_by", json!([1, 1, 3])),
        ("json.unique_by", json!([3, 1])),
        ("json.group_by", json!([[1, 1], [3]])),
    ] {
        for pointer in ["", " and pointer \"\""] {
            let numeric = if operation == "json.sort_by" {
                " and numeric true"
            } else {
                ""
            };
            let source = format!("run json.parse with text \"[3,1,1]\" as n\nrun {operation} with document n{numeric}{pointer}\n");
            let path = dir.path().join("case.program");
            fs::write(&path, source).unwrap();
            for action in ["check", "run"] {
                let output = Command::new(env!("CARGO_BIN_EXE_execute"))
                    .arg(action)
                    .arg(&path)
                    .arg("--manifest-dir")
                    .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{operation}, {pointer:?}, {action}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if action == "run" {
                    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(actual, expected, "{operation}, {pointer:?}");
                }
            }
        }
    }
}

#[test]
fn has_defaults_to_root_and_delete_requires_a_non_root_path() {
    let document = json!({"document":{"a":1}});
    let mut args = document.as_object().unwrap().clone();
    assert_eq!(
        applets::execute("json.has", &args, b"").unwrap(),
        json!(true)
    );
    args.insert("path".into(), json!(""));
    assert_eq!(
        applets::execute("json.has", &args, b"").unwrap(),
        json!(true)
    );
    let error = applets::execute("json.delete", &args, b"").unwrap_err();
    assert!(error.message.contains("cannot remove the document root"));
    args.insert("path".into(), json!("/a"));
    assert_eq!(
        applets::execute("json.delete", &args, b"").unwrap(),
        json!({})
    );
    args.insert("path".into(), json!(null));
    assert_eq!(
        applets::execute("json.has", &args, b"").unwrap_err().code,
        "invalid_request"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.program");
    let invoke = |operation: &str, arg: &str, action: &str| {
        fs::write(
            &path,
            format!(
                "run json.parse with text \"[1]\" as n\nrun {operation} with document n{arg}\n"
            ),
        )
        .unwrap();
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(action)
            .arg(&path)
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
            .output()
            .unwrap()
    };
    for arg in ["", " and path \"\""] {
        for action in ["check", "run"] {
            let output = invoke("json.has", arg, action);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if action == "run" {
                assert_eq!(
                    serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                    json!(true)
                );
            }
        }
    }
    let missing = invoke("json.delete", "", "check");
    assert!(!missing.status.success());
    let valid = invoke("json.delete", " and path \"/0\"", "run");
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let root = invoke("json.delete", " and path \"\"", "run");
    assert!(!root.status.success());
    assert!(String::from_utf8_lossy(&root.stderr).contains("cannot remove the document root"));
}

#[test]
fn non_stdin_workflows_require_their_documents_and_operands() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("missing.program");
    let check = |source: String| {
        fs::write(&path, source).unwrap();
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("check")
            .arg(&path)
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
            .output()
            .unwrap()
    };
    for op in [
        "delete",
        "entries",
        "first",
        "group_by",
        "has",
        "keys",
        "last",
        "length",
        "omit",
        "pick",
        "rename_keys",
        "sort_by",
        "sum",
        "unique_by",
        "values",
    ] {
        let output = check(format!("run json.{op}\n"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains("document"),
            "json.{op}: {stderr}"
        );
    }
    for (operation, missing, arguments) in [
        ("json.concat", "left", "right a"),
        ("json.concat", "right", "left a"),
        ("json.merge", "left", "right o"),
        ("json.merge", "right", "left o"),
        ("json.pick", "keys", "document o"),
        ("json.omit", "keys", "document o"),
        ("json.rename_keys", "renames", "document o"),
    ] {
        let source = format!("run json.parse with text \"[1]\" as a\nrun json.parse with text \"{{}}\" as o\nrun {operation} with {arguments}\n");
        let output = check(source);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(missing),
            "{operation}: {stderr}"
        );
    }
    // The library API still accepts stdin even where a workflow manifest cannot.
    assert_eq!(
        applets::execute("json.length", &Default::default(), b"[1,2]").unwrap(),
        json!(2)
    );
    // Stdin-capable workflows retain their optional document parameter.
    let manifest: Value = serde_json::from_slice(
        &fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/manifests/core.json_get.json"
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["accepts_stdin"], true);
    assert_ne!(manifest["parameters"]["document"]["required"], true);
    fs::write(&path, "run json.parse with text \"[1,2]\"\nthen json.get\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!([1, 2])
    );
}
