//! `~/` in manifest path-typed parameters expands to the home directory. Argv text that
//! is not a path parameter stays literal, and so does `~user`.

use flow::ast::Value;
use flow::capabilities::expand_home_path;
use flow::manifest::Registry;
use std::collections::BTreeMap;
use std::path::Path;

fn expanded(path: &str) -> String {
    expand_home_path(path)
        .unwrap()
        .into_os_string()
        .into_string()
        .unwrap()
}

fn reader_registry() -> Registry {
    Registry::from_sources(&[serde_json::json!({
        "name": "reader.path",
        "binary": "reader",
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {
            "file": { "type": "path", "required": true },
            "label": { "type": "string", "required": true }
        },
        "arguments": [
            { "literal": "--tag=~/literal" },
            { "parameter": "file" },
            { "parameter": "label" }
        ]
    })
    .to_string()])
    .unwrap()
}

fn values(file: &str, label: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("file".to_owned(), Value::String(file.to_owned())),
        ("label".to_owned(), Value::String(label.to_owned())),
    ])
}

#[test]
fn a_path_parameter_expands_and_argv_text_stays_literal() {
    let registry = reader_registry();
    let manifest = registry.resolve("reader.path").unwrap();
    let argv = manifest
        .build_arguments(&values("~/notes.txt", "~/label"))
        .unwrap();
    assert_eq!(
        argv[0], "--tag=~/literal",
        "literal argv text is never expanded"
    );
    assert_eq!(
        argv[1],
        expanded("~/notes.txt"),
        "the path parameter is expanded"
    );
    assert_eq!(argv[2], "~/label", "a string parameter is not a path");
}

#[test]
fn paths_without_a_home_shorthand_are_unchanged() {
    let registry = reader_registry();
    let manifest = registry.resolve("reader.path").unwrap();
    let argv = manifest
        .build_arguments(&values("relative/file.txt", "x"))
        .unwrap();
    assert_eq!(argv[1], "relative/file.txt");
    let argv = manifest
        .build_arguments(&values("~user/file.txt", "x"))
        .unwrap();
    assert_eq!(argv[1], "~user/file.txt", "~user is not a home shorthand");
}

#[test]
fn the_path_predicate_follows_the_declared_type() {
    let registry = reader_registry();
    let manifest = registry.resolve("reader.path").unwrap();
    assert!(manifest.is_path_parameter("file"));
    assert!(!manifest.is_path_parameter("label"));
    assert!(!manifest.is_path_parameter("missing"));
}

#[test]
fn in_process_path_parameters_expand_in_the_json_handed_to_the_applet() {
    // The shipped fs.* applets receive their parameters as JSON through dispatch.
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let registry = Registry::load_optional(&manifests).unwrap();
    let manifest = registry.resolve("fs.ensure_dir").unwrap();
    let parameters = manifest
        .parameters_json(&BTreeMap::from([(
            "path".to_owned(),
            Value::String("~/work/build".to_owned()),
        )]))
        .unwrap();
    assert_eq!(parameters["path"], expanded("~/work/build"));
    let mode = manifest
        .parameters_json(&BTreeMap::from([
            ("path".to_owned(), Value::String("x".to_owned())),
            ("mode".to_owned(), Value::String("~/mode".to_owned())),
        ]))
        .unwrap();
    assert_eq!(
        mode["mode"], "~/mode",
        "a string parameter keeps ~ as written"
    );
}
