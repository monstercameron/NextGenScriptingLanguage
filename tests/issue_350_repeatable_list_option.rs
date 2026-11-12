//! `accept NAME as list of text from option "--name"`: each occurrence of the option adds
//! one item, in order. `--name=a,b` is one literal item, and a text option is still
//! refused when given twice.
use flow::ast::Value;
use std::collections::BTreeMap;

fn bind(source: &str, arguments: &[&str]) -> anyhow::Result<BTreeMap<String, Value>> {
    let program = flow::parser::parse(source)?;
    let arguments: Vec<String> = arguments.iter().map(|s| s.to_string()).collect();
    flow::inputs::parse_cli_arguments(&program.inputs, &BTreeMap::new(), &arguments)
}

const EXCLUDES: &str = "accept excludes as list of text from option \"--except\"\nrun \"printf\" with arguments [\"%s\\n\", \"done\"]\n";

fn strings(items: &Value) -> Vec<String> {
    let Value::List(items) = items else {
        panic!("expected a list, got {items:?}");
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(text) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn repeated_list_option_collects_every_occurrence_in_order() {
    let values = bind(EXCLUDES, &["--except", "a", "--except", "b"]).unwrap();
    assert_eq!(strings(&values["excludes"]), vec!["a", "b"]);
}

#[test]
fn list_option_accepts_the_equals_form_and_keeps_commas_literal() {
    let values = bind(EXCLUDES, &["--except=a,b", "--except=c"]).unwrap();
    assert_eq!(strings(&values["excludes"]), vec!["a,b", "c"]);
}

#[test]
fn single_occurrence_is_a_one_item_list() {
    let values = bind(EXCLUDES, &["--except", "only"]).unwrap();
    assert_eq!(strings(&values["excludes"]), vec!["only"]);
}

#[test]
fn absent_list_option_is_not_bound() {
    let values = bind(EXCLUDES, &[]).unwrap();
    assert!(!values.contains_key("excludes"));
}

#[test]
fn list_option_without_a_value_is_refused() {
    let error = bind(EXCLUDES, &["--except"]).unwrap_err();
    assert!(
        error.to_string().contains("--except needs a value"),
        "{error}"
    );
}

#[test]
fn text_option_given_twice_is_still_refused() {
    let source = "accept excludes as text from option \"--except\"\nrun \"printf\" with arguments [\"%s\\n\", \"done\"]\n";
    let error = bind(source, &["--except", "a", "--except", "b"]).unwrap_err();
    assert!(
        error.to_string().contains("given more than once"),
        "{error}"
    );
}

#[test]
fn list_option_with_a_default_replaces_the_default_when_given_repeatedly() {
    // Defaults apply later in resolve(); the CLI binder only reports what was given.
    let source = "accept excludes as list of text from option \"--except\" with default [\"x\"]\nrun \"printf\" with arguments [\"%s\\n\", \"done\"]\n";
    let values = bind(source, &[]).unwrap();
    assert!(!values.contains_key("excludes"));
    let values = bind(source, &["--except", "a", "--except", "b"]).unwrap();
    assert_eq!(strings(&values["excludes"]), vec!["a", "b"]);
}
