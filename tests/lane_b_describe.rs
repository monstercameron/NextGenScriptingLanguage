//! `described as "TEXT"` on input declarations: stored on the AST, accepted in either
//! order with `with default`, rejected when repeated or not quoted, and shown by inspect.
use flow::parser::parse;
use std::{fs, process::Command};

#[test]
fn description_is_stored_before_or_after_default() {
    let before = parse("accept name as text described as \"Who to greet\" with default \"you\"\n")
        .expect("description before default");
    assert_eq!(
        before.inputs[0].description.as_deref(),
        Some("Who to greet")
    );
    assert_eq!(
        before.inputs[0].default,
        Some(flow::ast::Value::String("you".into()))
    );

    let after = parse("accept name as text with default \"you\" described as \"Who to greet\"\n")
        .expect("description after default");
    assert_eq!(after.inputs[0].description.as_deref(), Some("Who to greet"));
    assert_eq!(
        after.inputs[0].default,
        Some(flow::ast::Value::String("you".into()))
    );
}

#[test]
fn input_without_description_stores_none() {
    let program = parse("accept name as text with default \"you\"\n").unwrap();
    assert_eq!(program.inputs[0].description, None);
}

#[test]
fn repeated_description_is_rejected() {
    let Err(error) = parse("accept name as text described as \"a\" described as \"b\"\n") else {
        panic!("duplicate description must fail");
    };
    assert!(format!("{error:#}").contains("more than once"), "{error:#}");
}

#[test]
fn unquoted_description_is_rejected() {
    let Err(error) = parse("accept name as text described as 5\n") else {
        panic!("unquoted description must fail");
    };
    assert!(
        format!("{error:#}").contains("must be followed by a quoted description"),
        "{error:#}"
    );
}

#[test]
fn inspect_reports_the_description_for_each_input() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("greet.program");
    fs::write(
        &file,
        "accept name as text described as \"Who to greet\"\naccept count as integer with default 2\nrun \"printf\" with arguments [name]\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("inspect")
        .arg(&file)
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let inputs = report["inputs"].as_array().expect("inputs array");
    assert_eq!(inputs[0]["name"], "name");
    assert_eq!(inputs[0]["description"], "Who to greet");
    assert!(
        inputs[1].get("description").is_none(),
        "undescribed input must not gain a description key"
    );
}
