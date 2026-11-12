//! Issue #481: `path`, `file` and `directory` input types, exercised through the library.
use flow::{ast::Value, diagnostics, manifest::Registry, parser};
use std::collections::BTreeMap;

fn program(source: &str) -> flow::ast::Program {
    parser::parse(source).unwrap_or_else(|error| panic!("{source:?}: {error:#}"))
}

fn resolve_target(source: &str, value: &str) -> Result<BTreeMap<String, Value>, String> {
    let program = program(source);
    let supplied = BTreeMap::from([("target".to_owned(), Value::String(value.to_owned()))]);
    flow::inputs::resolve(&program.inputs, &supplied, false).map_err(|error| format!("{error:#}"))
}

#[test]
fn path_types_parse_and_describe_with_their_spelling() {
    for (word, label) in [
        ("path", "path"),
        ("file", "file"),
        ("directory", "directory"),
    ] {
        let source = format!("accept target as {word}\n");
        let program = program(&source);
        assert_eq!(program.inputs[0].kind.to_string(), label);
        let described = flow::agent::help::describe(&program, "job.program");
        assert_eq!(described["inputs"][0]["type"], label, "{source:?}");
    }
}

#[test]
fn path_types_are_refused_for_produce_and_one_of() {
    assert!(parser::parse("produce directory\n").is_err());
    assert!(parser::parse("accept target as directory one of [\"a\"]\n").is_err());
}

#[test]
fn an_empty_path_value_is_refused_before_any_filesystem_check() {
    let error = resolve_target("accept target as path\n", "").unwrap_err();
    assert!(error.contains("nonempty path"), "{error}");
}

#[test]
fn path_input_accepts_a_missing_path_because_it_is_not_checked() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("not-created-yet");
    let resolved = resolve_target("accept target as path\n", missing.to_str().unwrap()).unwrap();
    assert_eq!(
        resolved["target"],
        Value::String(missing.to_str().unwrap().into())
    );
}

#[test]
fn file_and_directory_inputs_check_existence_and_kind_at_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "x").unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let missing = dir.path().join("missing");
    let file_text = file.to_str().unwrap();
    let sub_text = sub.to_str().unwrap();
    let missing_text = missing.to_str().unwrap();
    let file_source = "accept target as file\n";
    let directory_source = "accept target as directory\n";

    assert!(resolve_target(file_source, file_text).is_ok());
    assert!(resolve_target(directory_source, sub_text).is_ok());

    let error = resolve_target(directory_source, missing_text).unwrap_err();
    assert!(error.contains("must name a directory"), "{error}");
    assert!(error.contains("does not exist"), "{error}");

    let error = resolve_target(directory_source, file_text).unwrap_err();
    assert!(error.contains("is not a directory"), "{error}");

    let error = resolve_target(file_source, sub_text).unwrap_err();
    assert!(error.contains("is not a regular file"), "{error}");

    let error = resolve_target(file_source, missing_text).unwrap_err();
    assert!(error.contains("must name a regular file"), "{error}");
}

#[test]
fn a_sensitive_path_value_is_not_shown_in_the_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("token-481-missing");
    let error = resolve_target(
        "accept target as file from environment \"FLOW_481_TARGET\"\n",
        secret.to_str().unwrap(),
    )
    .unwrap_err();
    assert!(error.contains("the supplied value is not shown"), "{error}");
    assert!(!error.contains("token-481-missing"), "{error}");
}

#[test]
fn static_check_does_not_touch_the_filesystem_for_a_path_default() {
    let source = "accept target as directory with default \"/flow-481-does-not-exist\"\n";
    let program = program(source);
    flow::inputs::validate(&program.inputs).unwrap();
    for diagnostic in diagnostics::analyze_source(source, &Registry::default()) {
        assert!(
            !diagnostic.message.contains("does not exist")
                && !diagnostic.message.contains("must name"),
            "{}",
            diagnostic.message
        );
    }
}
