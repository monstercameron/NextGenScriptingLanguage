//! `accept NAME as json matching "PATH"`: the parser keeps the schema path on the
//! input declaration, and the clause is refused on every other input type.
use flow::ast::InputType;

#[test]
fn json_input_keeps_its_matching_schema_path() {
    let program = flow::parser::parse(
        "accept hosts as json matching \"schemas/hosts.json\" described as \"Hosts\"\nuse value \"x\"\n",
    )
    .unwrap();
    let input = &program.inputs[0];
    assert_eq!(input.kind, InputType::Json);
    assert_eq!(input.schema_path.as_deref(), Some("schemas/hosts.json"));
    assert_eq!(input.description.as_deref(), Some("Hosts"));
}

#[test]
fn json_input_without_the_clause_has_no_schema_path() {
    let program = flow::parser::parse("accept hosts as json\nuse value \"x\"\n").unwrap();
    assert_eq!(program.inputs[0].schema_path, None);
}

#[test]
fn matching_clause_is_refused_on_a_non_json_input() {
    let error = flow::parser::parse(
        "accept name as text matching \"schemas/name.json\"\nuse value \"x\"\n",
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("applies only to json inputs"),
        "{error:#}"
    );
}
