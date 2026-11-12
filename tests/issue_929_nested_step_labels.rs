//! Step labels on statements inside `if`, `otherwise`, loop, `try` and temporary-scope
//! bodies name that statement's first step, exactly as at top level (issue #929).
use flow::{ast, parser};

/// Every step id of a parsed program, in step order. A labelled step's id is its label.
fn step_ids(script: &str) -> Vec<String> {
    let program = parser::parse(script).unwrap_or_else(|error| panic!("{error:#}\n{script}"));
    ast::step_ids(&program.steps)
}

fn parse_error(script: &str) -> String {
    match parser::parse(script) {
        Ok(_) => panic!("expected a parse error:\n{script}"),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn label_on_statement_in_if_body_names_its_step() {
    let ids = step_ids("if mode is \"live\"\n  fetch: run \"printf\" with arguments [\"x\\n\"]\n");
    assert!(ids.iter().any(|id| id == "fetch"), "{ids:?}");
}

#[test]
fn label_on_statement_in_otherwise_body_names_its_step() {
    let ids = step_ids(
        "if mode is \"live\"\n  run \"printf\" with arguments [\"a\\n\"]\notherwise\n  fetch: run \"printf\" with arguments [\"b\\n\"]\n",
    );
    assert!(ids.iter().any(|id| id == "fetch"), "{ids:?}");
}

#[test]
fn label_on_statement_in_loop_and_try_bodies_names_its_step() {
    let ids = step_ids(
        "repeat 2 times\n  again: run \"printf\" with arguments [\"a\\n\"]\ntry\n  attempt: run \"printf\" with arguments [\"b\\n\"]\nfinally\n  tidy: run \"printf\" with arguments [\"c\\n\"]\n",
    );
    for label in ["again", "attempt", "tidy"] {
        assert!(ids.iter().any(|id| id == label), "{label} missing: {ids:?}");
    }
}

#[test]
fn top_level_labels_still_name_their_step() {
    let ids = step_ids("fetch: run \"printf\" with arguments [\"x\\n\"]\n");
    assert_eq!(ids, ["fetch"]);
}

#[test]
fn duplicate_label_in_nested_body_is_rejected() {
    let error = parse_error(
        "fetch: run \"printf\" with arguments [\"a\"]\nif mode is \"live\"\n  fetch: run \"printf\" with arguments [\"b\"]\n",
    );
    assert!(error.contains("duplicate step label"), "{error}");
}

#[test]
fn keyword_label_in_nested_body_is_rejected() {
    let error = parse_error("if mode is \"live\"\n  if: run \"printf\" with arguments [\"a\"]\n");
    assert!(error.contains("is a keyword"), "{error}");
}
