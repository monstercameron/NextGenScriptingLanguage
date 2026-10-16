use flow::applets::{
    self, execute, AppletError, AppletErrorKind, ExecutionBudgetExceeded, ExecutionContext,
};
use flow::ast::{Invocation, Value as AstValue};
use flow::contracts::OutputType;
use flow::manifest::Registry;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

fn run(operation: &str, args: Value, input: &[u8]) -> Result<Value, AppletError> {
    let args = args.as_object().cloned().unwrap_or_default();
    execute(operation, &args, input)
}

fn read_file(path: &std::path::Path) -> Value {
    run("fs.read", json!({"path": path}), &[]).unwrap()
}

fn write_args(path: &std::path::Path, expected_hash: &str) -> Map<String, Value> {
    json!({
        "path": path,
        "expected_content_hash": expected_hash,
        "symlink_policy": "reject",
        "permissions_policy": "preserve",
    })
    .as_object()
    .unwrap()
    .clone()
}

#[test]
fn path_text_arithmetic_and_color_operations_are_explicit() {
    assert_eq!(
        run("path.dirname", json!({"path": "/opt/app/file.tar.gz"}), &[]).unwrap(),
        json!("/opt/app")
    );
    assert_eq!(
        run("path.dirname", json!({"path": "file"}), &[]).unwrap(),
        json!(".")
    );
    assert_eq!(
        run(
            "path.basename",
            json!({"path": "/opt/app/file.tar.gz", "suffix": ".gz"}),
            &[]
        )
        .unwrap(),
        json!("file.tar")
    );
    assert_eq!(
        run(
            "path.remove_suffix",
            json!({"path": "file.tar.gz", "suffix": ".tar.gz"}),
            &[]
        )
        .unwrap(),
        json!("file")
    );
    assert_eq!(
        run("text.trim_final_lf", json!({}), b"value\n\n").unwrap(),
        json!("value\n")
    );
    assert_eq!(
        run(
            "text.concat",
            json!({"parts": ["a", "b", "c"], "separator": ":"}),
            &[]
        )
        .unwrap(),
        json!("a:b:c")
    );
    assert_eq!(
        run("text.matches", json!({"needle": "aba"}), b"ababa").unwrap(),
        json!({"matches": true, "count": 2, "byte_offsets": [0, 2]})
    );
    assert_eq!(
        run("number.add", json!({"left": 2, "right": 3}), &[]).unwrap(),
        json!(5)
    );
    assert_eq!(
        run("number.add", json!({"left": 0.1, "right": 0.2}), &[]).unwrap(),
        json!(0.1f64 + 0.2f64)
    );
    assert_eq!(
        run("number.divide", json!({"left": 3, "right": 2}), &[]).unwrap(),
        json!(1.5)
    );
    assert_eq!(
        run("number.compare", json!({"left": 9, "right": 10}), &[]).unwrap(),
        json!(-1)
    );
    assert_eq!(
        run("number.to_text", json!({"value": 12.5}), &[]).unwrap(),
        json!("12.5")
    );
    assert_eq!(
        run("number.to_text", json!({"value": "12.5"}), &[])
            .unwrap_err()
            .exit_status(),
        2
    );
    assert_eq!(
        run(
            "number.parse_integer",
            json!({"text": "0010", "base": 8}),
            &[]
        )
        .unwrap(),
        json!(8)
    );
    assert_eq!(
        run(
            "number.parse_integer",
            json!({"trim_ascii_whitespace": true}),
            b"\t-42\r\n"
        )
        .unwrap(),
        json!(-42)
    );
    assert_eq!(
        run(
            "number.parse_integer",
            json!({"text": "08", "base": 8}),
            &[]
        )
        .unwrap_err()
        .exit_status(),
        2
    );
    assert_eq!(
        run(
            "color.rgb_to_hex",
            json!({"red": 15, "green": 160, "blue": 255}),
            &[]
        )
        .unwrap(),
        json!("#0fa0ff")
    );
    assert_eq!(
        run(
            "color.parse_rgb",
            json!({"default_hex": "#AABBCC"}),
            b" \t15, 160, 255\r\n"
        )
        .unwrap(),
        json!({"valid": true, "hex": "0fa0ff"})
    );
    assert_eq!(
        run(
            "color.parse_rgb",
            json!({"default_hex": "AABBCC"}),
            "15, 160,255".as_bytes()
        )
        .unwrap(),
        json!({"valid": false, "hex": "aabbcc"})
    );
    assert_eq!(
        run(
            "color.parse_rgb.result",
            json!({"default_hex": "AABBCC"}),
            b"15, 999, 255"
        )
        .unwrap_err()
        .exit_status(),
        2
    );
}

#[test]
fn arithmetic_contracts_refine_json_parameters_to_number() {
    let sources = [
        include_str!("../manifests/core.number_add.json"),
        include_str!("../manifests/core.number_subtract.json"),
        include_str!("../manifests/core.number_multiply.json"),
        include_str!("../manifests/core.number_divide.json"),
        include_str!("../manifests/core.number_compare.json"),
        include_str!("../manifests/core.number_to_text.json"),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let registry = Registry::from_sources(&sources).unwrap();

    for (name, expected_output) in [
        ("number.add", OutputType::Number),
        ("number.subtract", OutputType::Number),
        ("number.multiply", OutputType::Number),
        ("number.divide", OutputType::Number),
        ("number.compare", OutputType::Integer),
    ] {
        let manifest = registry.resolve(name).unwrap();
        let ports = manifest.ports().unwrap();
        assert_eq!(ports.inputs["left"].schema.kind, OutputType::Number);
        assert_eq!(ports.inputs["right"].schema.kind, OutputType::Number);
        assert_eq!(ports.outputs["value"].schema.kind, expected_output);

        let make_invocation = |left, right| Invocation {
            operation: name.to_owned(),
            direct: false,
            discard_stderr: false,
            parameters: BTreeMap::from([("left".to_owned(), left), ("right".to_owned(), right)]),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
        };
        let decimal = AstValue::Number(serde_json::Number::from_f64(2.5).unwrap());
        manifest
            .validate(&make_invocation(AstValue::Integer(1), decimal))
            .unwrap();
        for invalid in [
            AstValue::String("2.5".to_owned()),
            AstValue::Bool(true),
            AstValue::Null,
        ] {
            assert!(
                manifest
                    .validate(&make_invocation(invalid, AstValue::Integer(1)))
                    .is_err(),
                "{name} accepted a non-number JSON parameter"
            );
        }
    }

    let formatter = registry.resolve("number.to_text").unwrap();
    let ports = formatter.ports().unwrap();
    assert_eq!(ports.inputs["value"].schema.kind, OutputType::Number);
    assert_eq!(ports.outputs["value"].schema.kind, OutputType::Text);
    assert!(formatter.cacheable);
    let valid = Invocation {
        operation: "number.to_text".to_owned(),
        direct: false,
        discard_stderr: false,
        parameters: BTreeMap::from([(
            "value".to_owned(),
            AstValue::Number(serde_json::Number::from_f64(12.5).unwrap()),
        )]),
        binding: None,
        cache: false,
        status_binding: None,
        result_binding: None,
    };
    formatter.validate(&valid).unwrap();
    for invalid in [
        AstValue::String("12.5".to_owned()),
        AstValue::Bool(true),
        AstValue::Null,
    ] {
        let invalid_call = Invocation {
            parameters: BTreeMap::from([("value".to_owned(), invalid)]),
            ..valid.clone()
        };
        assert!(formatter.validate(&invalid_call).is_err());
    }
    flow::security::authorize_cache_purity(&registry, formatter).unwrap();
}

#[test]
fn structured_json_pointer_get_set_map_and_filter_are_declarative() {
    let document = json!({
        "a/b": {
            "~key": [
                {"id": 1, "active": true},
                {"id": 2, "active": false},
                {"id": 3, "active": true}
            ]
        },
        "preserved": "unrelated"
    });
    assert_eq!(
        run(
            "json.get",
            json!({"document": document, "pointer": "/a~1b/~0key/1/id"}),
            &[]
        )
        .unwrap(),
        json!(2)
    );

    let updated = run(
        "json.set",
        json!({
            "document": document,
            "pointer": "/a~1b/~0key/-",
            "value": {"id": 4, "active": true}
        }),
        &[],
    )
    .unwrap();
    assert_eq!(updated["preserved"], "unrelated");
    assert_eq!(updated["a/b"]["~key"][3]["id"], 4);

    assert_eq!(
        run(
            "json.map",
            json!({
                "document": updated,
                "pointer": "/a~1b/~0key",
                "select": "/id"
            }),
            &[]
        )
        .unwrap(),
        json!([1, 2, 3, 4])
    );
    let filtered = run(
        "json.filter",
        json!({
            "document": updated,
            "pointer": "/a~1b/~0key",
            "where": [
                {"pointer": "/active", "equals": true},
                {"pointer": "/id", "type": "number"}
            ]
        }),
        &[],
    )
    .unwrap();
    assert_eq!(
        filtered,
        json!([
            {"id": 1, "active": true},
            {"id": 3, "active": true},
            {"id": 4, "active": true}
        ])
    );
    assert_eq!(
        run(
            "json.filter",
            json!({
                "document": [
                    {"id": 1, "active": true},
                    {"id": 2},
                    {"id": 3, "active": false}
                ],
                "where": [
                    {"pointer": "/active", "exists": false}
                ]
            }),
            &[]
        )
        .unwrap(),
        json!([{"id": 2}])
    );
    assert_eq!(
        run(
            "json.filter",
            json!({
                "document": [
                    {"identifier": {"id": "remove-me"}},
                    {"identifier": {"id": "keep-me"}},
                    {"identifier": {}},
                    {}
                ],
                "where": [
                    {"pointer": "/identifier/id", "not_equals": "remove-me"}
                ]
            }),
            &[]
        )
        .unwrap(),
        json!([
            {"identifier": {"id": "keep-me"}},
            {"identifier": {}},
            {}
        ])
    );

    let missing = run(
        "json.get",
        json!({"document": updated, "pointer": "/absent"}),
        &[],
    )
    .unwrap_err();
    assert_eq!(missing.kind, AppletErrorKind::Absence);
    assert_eq!(
        run(
            "json.filter",
            json!({"document": updated, "where": [{"expr": "value => true"}]}),
            &[]
        )
        .unwrap_err()
        .kind,
        AppletErrorKind::Permanent
    );
}

#[test]
fn json_parse_and_stringify_preserve_json_string_contents() {
    let original = "quotes \" backslash \\ newline\n雪";
    let encoded = run("json.stringify", json!({"value": original}), &[]).unwrap();
    assert_eq!(
        encoded,
        json!("\"quotes \\\" backslash \\\\ newline\\n雪\"")
    );
    assert_eq!(
        run("json.parse", json!({"text": encoded}), &[]).unwrap(),
        json!(original)
    );
    assert_eq!(
        run(
            "json.parse",
            json!({}),
            b"{\"nullable\":null,\"items\":[1,true]}"
        )
        .unwrap(),
        json!({"nullable": null, "items": [1, true]})
    );
    assert_eq!(
        run("json.parse", json!({"text": "not-json"}), &[])
            .unwrap_err()
            .kind,
        AppletErrorKind::Permanent
    );
}

#[test]
fn atomic_write_uses_hash_cas_and_leaves_content_unchanged_on_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.txt");
    let lock = directory.path().join("shared.lock");
    let mut args = write_args(&path, "missing");
    args.insert("lock".into(), json!(lock));
    run("fs.write_atomic", Value::Object(args), b"first\n").unwrap();

    let read = read_file(&path);
    assert_eq!(read["content"], "first\n");
    let hash = read["content_hash"].as_str().unwrap();
    let mut conflict = write_args(&path, "stale-hash");
    conflict.insert("content".into(), json!("should not be written"));
    let error = execute("fs.write_atomic", &conflict, &[]).unwrap_err();
    assert_eq!(error.code, "conflict");
    assert_eq!(error.kind, AppletErrorKind::Retryable);
    assert_eq!(read_file(&path)["content"], "first\n");

    let uncertain = AppletError {
        code: "uncertain_effect",
        kind: AppletErrorKind::Uncertain,
        message: "replacement may have happened".into(),
    };
    assert_eq!(uncertain.kind, AppletErrorKind::Uncertain);
    assert_eq!(uncertain.exit_status(), 76);

    let mut update = write_args(&path, hash);
    update.insert("content".into(), json!("second\n"));
    let result = execute("fs.write_atomic", &update, &[]).unwrap();
    assert_eq!(result["changed"], true);
    assert_eq!(read_file(&path)["content"], "second\n");
}

#[cfg(unix)]
#[test]
fn atomic_write_honors_explicit_permission_policy() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mode.txt");
    fs::write(&path, "before").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let hash = read_file(&path)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut preserve = write_args(&path, &hash);
    preserve.insert("content".into(), json!("preserved"));
    execute("fs.write_atomic", &preserve, &[]).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o640
    );

    let hash = read_file(&path)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut exact = write_args(&path, &hash);
    exact.insert("content".into(), json!("exact"));
    exact.insert("permissions_policy".into(), json!("exact"));
    exact.insert("mode".into(), json!(0o644));
    execute("fs.write_atomic", &exact, &[]).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o644
    );

    let hash = read_file(&path)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut private = write_args(&path, &hash);
    private.insert("content".into(), json!("private"));
    private.insert("permissions_policy".into(), json!("private"));
    execute("fs.write_atomic", &private, &[]).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn atomic_write_applies_each_symlink_policy_to_the_final_component() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target.txt");
    let preserve_link = directory.path().join("preserve.txt");
    let reject_link = directory.path().join("reject.txt");
    let replace_link = directory.path().join("replace.txt");
    let broken_link = directory.path().join("broken.txt");
    let missing_target = directory.path().join("missing-target.txt");
    fs::write(&target, "original").unwrap();
    symlink(&target, &preserve_link).unwrap();
    symlink(&target, &reject_link).unwrap();
    symlink(&target, &replace_link).unwrap();
    symlink(&missing_target, &broken_link).unwrap();

    let hash = read_file(&preserve_link)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut preserve = write_args(&preserve_link, &hash);
    preserve.insert("symlink_policy".into(), json!("preserve"));
    preserve.insert("content".into(), json!("preserved target"));
    let receipt = execute("fs.write_atomic", &preserve, &[]).unwrap();
    assert!(fs::symlink_metadata(&preserve_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&target).unwrap(), "preserved target");
    assert_eq!(receipt["snapshot"], read_file(&preserve_link)["snapshot"]);

    let mut preserve_broken = write_args(&broken_link, "missing");
    preserve_broken.insert("symlink_policy".into(), json!("preserve"));
    preserve_broken.insert("content".into(), json!("unreachable"));
    assert_eq!(
        execute("fs.write_atomic", &preserve_broken, &[])
            .unwrap_err()
            .kind,
        AppletErrorKind::Absence
    );
    assert!(fs::symlink_metadata(&broken_link)
        .unwrap()
        .file_type()
        .is_symlink());

    let mut replace_broken = write_args(&broken_link, "missing");
    replace_broken.insert("symlink_policy".into(), json!("replace"));
    replace_broken.insert("content".into(), json!("replaced dangling link"));
    execute("fs.write_atomic", &replace_broken, &[]).unwrap();
    assert!(!fs::symlink_metadata(&broken_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(&broken_link).unwrap(),
        "replaced dangling link"
    );

    let current_hash = read_file(&reject_link)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut reject = write_args(&reject_link, &current_hash);
    reject.insert("content".into(), json!("rejected"));
    assert_eq!(
        execute("fs.write_atomic", &reject, &[]).unwrap_err().kind,
        AppletErrorKind::Permanent
    );
    assert!(fs::symlink_metadata(&reject_link)
        .unwrap()
        .file_type()
        .is_symlink());

    let mut replace = write_args(&replace_link, &current_hash);
    replace.insert("symlink_policy".into(), json!("replace"));
    replace.insert("content".into(), json!("replaced link"));
    execute("fs.write_atomic", &replace, &[]).unwrap();
    assert!(!fs::symlink_metadata(&replace_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&replace_link).unwrap(), "replaced link");
    assert_eq!(fs::read_to_string(&target).unwrap(), "preserved target");
}

#[cfg(unix)]
#[test]
fn checked_unlink_removes_the_leaf_symlink_and_preserves_its_target() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target.txt");
    let link = directory.path().join("shortcut.txt");
    fs::write(&target, "keep me").unwrap();
    symlink(&target, &link).unwrap();

    let stale = read_file(&link)["snapshot"].as_str().unwrap().to_owned();
    fs::write(&target, "changed but keep me").unwrap();
    let mut args = json!({
        "path": link,
        "expected_snapshot": stale,
        "lock_timeout_ms": 1000,
    })
    .as_object()
    .unwrap()
    .clone();
    let conflict = execute("fs.unlink_checked", &args, &[]).unwrap_err();
    assert_eq!(conflict.code, "conflict");
    assert_eq!(conflict.exit_status(), 75);
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());

    let current = read_file(&link)["snapshot"].as_str().unwrap().to_owned();
    args.insert("expected_snapshot".into(), json!(current));
    let receipt = execute("fs.unlink_checked", &args, &[]).unwrap();
    assert_eq!(receipt["removed"], true);
    assert_eq!(receipt["symlink"], target.to_string_lossy().as_ref());
    assert!(!link.exists());
    assert_eq!(fs::read_to_string(&target).unwrap(), "changed but keep me");
}

#[test]
fn checked_unlink_removes_regular_files_and_rejects_directories() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file.txt");
    fs::write(&file, "remove me").unwrap();
    let snapshot = read_file(&file)["snapshot"].as_str().unwrap().to_owned();
    let args = json!({"path": file, "expected_snapshot": snapshot})
        .as_object()
        .unwrap()
        .clone();
    assert_eq!(
        execute("fs.unlink_checked", &args, &[]).unwrap()["removed"],
        true
    );
    assert!(!file.exists());

    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let args = json!({"path": nested, "expected_snapshot": "unused"})
        .as_object()
        .unwrap()
        .clone();
    let error = execute("fs.unlink_checked", &args, &[]).unwrap_err();
    assert_eq!(error.kind, AppletErrorKind::Permanent);
    assert!(nested.is_dir());
}

#[test]
fn jsonc_set_and_remove_touch_active_root_keys_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.jsonc");
    let lock = directory.path().join("settings.lock");
    let original = "{\n  // \"theme\": \"commented out\"\n  \"nested\": { \"theme\": \"nested\", },\n  \"theme\": \"old\", // keep this comment\n  \"escaped\\\"key\": 1,\n  \"theme\": \"last\",\n}\n";
    fs::write(&path, original).unwrap();
    let hash = read_file(&path)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut args = write_args(&path, &hash);
    args.insert("lock".into(), json!(lock));
    args.insert("key".into(), json!("theme"));
    args.insert("value".into(), json!("new \"theme\"\n雪"));
    execute("jsonc.set", &args, &[]).unwrap();
    let updated = fs::read_to_string(&path).unwrap();
    assert!(updated.contains("// \"theme\": \"commented out\""));
    assert!(updated.contains("\"theme\": \"nested\","));
    assert!(updated.contains("\"theme\": \"old\""));
    assert!(updated.contains("\"theme\": \"new \\\"theme\\\"\\n雪\","));
    assert!(updated.contains("\"escaped\\\"key\": 1"));

    let hash = read_file(&path)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut remove = write_args(&path, &hash);
    remove.insert("lock".into(), json!(lock));
    remove.insert("key".into(), json!("theme"));
    let result = execute("jsonc.remove", &remove, &[]).unwrap();
    assert_eq!(result["changed"], true);
    let removed = fs::read_to_string(&path).unwrap();
    assert!(removed.contains("\"theme\": \"nested\","));
    assert!(!removed.contains("\"theme\": \"old\""));
    assert!(!removed.contains("\"theme\": \"new"));
    assert!(removed.contains("// keep this comment"));

    let hash = read_file(&path)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut remove_missing = write_args(&path, &hash);
    remove_missing.insert("lock".into(), json!(lock));
    remove_missing.insert("key".into(), json!("missing"));
    let receipt = execute("jsonc.remove", &remove_missing, &[]).unwrap();
    assert_eq!(receipt["changed"], false);
    assert_eq!(fs::read_to_string(&path).unwrap(), removed);
}

#[test]
fn jsonc_set_inserts_root_keys_and_keeps_comments_and_trailing_comma_style() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.jsonc");
    let original =
        "{\n  // retained comment\n  // \"fresh\": \"comment only\"\n  \"other\": 1,\n}\n";
    fs::write(&path, original).unwrap();
    let hash = read_file(&path)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut args = write_args(&path, &hash);
    args.insert("key".into(), json!("fresh"));
    args.insert("value".into(), json!("value"));
    execute("jsonc.set", &args, &[]).unwrap();
    let updated = fs::read_to_string(&path).unwrap();
    assert!(updated.contains("// retained comment"));
    assert!(updated.contains("// \"fresh\": \"comment only\""));
    assert!(updated.contains("\"other\": 1,"));
    assert!(updated.contains("\"fresh\": \"value\","));
    assert!(updated.ends_with("}\n"));
}

#[test]
fn jsonc_edit_content_stages_the_same_structural_edits_without_file_effects() {
    let original = "{\n  // keep this\n  \"other\": 1,\n}\n";
    let set = run(
        "jsonc.edit_content",
        json!({"content": original, "action": "set", "key": "theme", "value": "dark\"\n雪"}),
        &[],
    )
    .unwrap();
    let staged = set.as_str().unwrap();
    assert!(staged.contains("// keep this"));
    assert!(staged.contains("\"theme\": \"dark\\\"\\n雪\","));
    let removed = run(
        "jsonc.edit_content",
        json!({"content": staged, "action": "remove", "key": "theme"}),
        &[],
    )
    .unwrap();
    assert!(!removed.as_str().unwrap().contains("\"theme\""));
    assert!(removed.as_str().unwrap().contains("// keep this"));
    assert_eq!(
        run(
            "jsonc.edit_content",
            json!({"content": "{\"broken\":}", "action": "set", "key": "x", "value": 1}),
            &[]
        )
        .unwrap_err()
        .kind,
        AppletErrorKind::Permanent
    );
}

#[test]
fn malformed_jsonc_fails_before_file_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broken.jsonc");
    let original = "{\n  \"active\": ,\n}\n";
    fs::write(&path, original).unwrap();
    let hash = read_file(&path)["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut args = write_args(&path, &hash);
    args.insert("key".into(), json!("active"));
    args.insert("value".into(), json!("replacement"));
    let error = execute("jsonc.set", &args, &[]).unwrap_err();
    assert_eq!(error.kind, AppletErrorKind::Permanent);
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn explicit_working_directory_is_used_for_relative_filesystem_paths() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("readme.txt"), "base\n").unwrap();
    let result = applets::execute_in_cwd(
        "fs.read",
        &json!({"path": "readme.txt"}).as_object().unwrap().clone(),
        &[],
        directory.path(),
    )
    .unwrap();
    assert_eq!(result["content"], "base\n");

    let mut write = write_args(std::path::Path::new("written.txt"), "missing");
    write.insert("lock".into(), json!("locks/custom.lock"));
    write.insert("content".into(), json!("created\n"));
    fs::create_dir(directory.path().join("locks")).unwrap();
    applets::execute_in_cwd("fs.write_atomic", &write, &[], directory.path()).unwrap();
    assert_eq!(
        fs::read_to_string(directory.path().join("written.txt")).unwrap(),
        "created\n"
    );
    assert!(directory.path().join("locks/custom.lock").is_file());
}

#[test]
fn helper_cli_and_worker_share_the_typed_json_and_error_protocol() {
    let helper = env!("CARGO_BIN_EXE_execute-applet");
    let direct = Command::new(helper)
        .args(["path.dirname", "--args", r#"{"path":"/opt/app/file"}"#])
        .output()
        .unwrap();
    assert_eq!(direct.status.code(), Some(0));
    assert_eq!(direct.stdout, b"\"/opt/app\"\n");
    assert!(direct.stderr.is_empty());

    let error = Command::new(helper)
        .args(["not.an_operation", "--args", "{}"])
        .output()
        .unwrap();
    assert_eq!(error.status.code(), Some(2));
    assert!(error.stdout.is_empty());
    assert!(error.stderr.ends_with(b"\n"));

    let mut worker = Command::new(helper)
        .arg("--worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut worker_input = worker.stdin.take().unwrap();
    for request in [
        json!({
            "id": 1,
            "arguments": ["text.trim_final_lf", "--args", "{}"],
            "input": b"one\n",
        }),
        json!({
            "id": 2,
            "arguments": ["not.an_operation", "--args", "{}"],
            "input": [],
        }),
    ] {
        serde_json::to_writer(&mut worker_input, &request).unwrap();
        worker_input.write_all(b"\n").unwrap();
    }
    drop(worker_input);
    let output = worker.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let responses = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["status"], 0);
    assert_eq!(responses[0]["output"], json!(b"\"one\"\n"));
    assert_eq!(responses[0].get("stderr"), None);
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[1]["status"], 2);
    assert_eq!(responses[1]["output"], json!([]));
    assert!(responses[1]["stderr"].as_str().unwrap().ends_with('\n'));
}

#[test]
fn json_collection_transforms_stop_on_scoped_budget_checkpoints() {
    for (operation, budget, expected_kind) in [
        (
            "json.map",
            ExecutionBudgetExceeded::Cancelled,
            AppletErrorKind::Cancelled,
        ),
        (
            "json.filter",
            ExecutionBudgetExceeded::Timeout,
            AppletErrorKind::Timeout,
        ),
    ] {
        let items = (0..1024).collect::<Vec<_>>();
        let document =
            json!({"items": items.iter().map(|value| json!({"value": value})).collect::<Vec<_>>()});
        let arguments = if operation == "json.map" {
            json!({"document": document, "pointer": "/items", "select": "/value"})
        } else {
            json!({
                "document": document,
                "pointer": "/items",
                "where": [{"pointer": "/value", "exists": true}]
            })
        }
        .as_object()
        .unwrap()
        .clone();
        let checkpoints = AtomicUsize::new(0);
        let check = || {
            let current = checkpoints.fetch_add(1, Ordering::Relaxed) + 1;
            // The document validation has finished by checkpoint 20 for this
            // fixture, so this interrupts the collection work itself.
            if current == 23 {
                Err(budget)
            } else {
                Ok(())
            }
        };
        let context = ExecutionContext::new(&check);
        let error = applets::execute_with_context(
            operation,
            &arguments,
            &[],
            std::path::Path::new("."),
            &context,
        )
        .expect_err("the controlled checkpoint should stop the transform");

        assert_eq!(error.kind, expected_kind, "operation {operation}");
        assert_eq!(checkpoints.load(Ordering::Relaxed), 23);
    }
}

#[test]
fn text_and_jsonc_loops_observe_scoped_cancellation() {
    let text_checkpoints = AtomicUsize::new(0);
    let text_check = || {
        if text_checkpoints.fetch_add(1, Ordering::Relaxed) + 1 == 2 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let text_context = ExecutionContext::new(&text_check);
    let text_error = applets::execute_with_context(
        "text.matches",
        &json!({"text": "a".repeat(1024), "needle": "b"})
            .as_object()
            .unwrap()
            .clone(),
        &[],
        std::path::Path::new("."),
        &text_context,
    )
    .expect_err("text scanning should poll its scoped budget");
    assert_eq!(text_error.kind, AppletErrorKind::Cancelled);
    assert_eq!(text_checkpoints.load(Ordering::Relaxed), 2);

    let jsonc_checkpoints = AtomicUsize::new(0);
    let jsonc_check = || {
        if jsonc_checkpoints.fetch_add(1, Ordering::Relaxed) + 1 == 6 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let jsonc_context = ExecutionContext::new(&jsonc_check);
    let content = format!("/*{}*/{{\"keep\":true}}", "x".repeat(1024));
    let jsonc_error = applets::execute_with_context(
        "jsonc.edit_content",
        &json!({"content": content, "key": "keep", "action": "remove"})
            .as_object()
            .unwrap()
            .clone(),
        &[],
        std::path::Path::new("."),
        &jsonc_context,
    )
    .expect_err("JSONC scanning should poll its scoped budget");
    assert_eq!(jsonc_error.kind, AppletErrorKind::Cancelled);
    assert_eq!(jsonc_checkpoints.load(Ordering::Relaxed), 6);
}
