//! Single-letter flag aliases combine: `-vq` sets `-v` and `-q` when both are declared
//! boolean flags. A cluster with an undeclared letter is refused, and a dash-led value
//! whose first letter is not a declared flag stays positional.
use flow::ast::Value;
use std::collections::BTreeMap;

const FLAGS: &str = "accept verbose as boolean from flag \"--verbose\" alias \"-v\"\naccept quiet as boolean from flag \"--quiet\" alias \"-q\"\nrun \"printf\" with arguments [\"%s\\n\", \"done\"]\n";

fn bind(source: &str, arguments: &[&str]) -> anyhow::Result<BTreeMap<String, Value>> {
    let program = flow::parser::parse(source)?;
    let arguments: Vec<String> = arguments.iter().map(|s| s.to_string()).collect();
    flow::inputs::parse_cli_arguments(&program.inputs, &BTreeMap::new(), &arguments)
}

#[test]
fn cluster_sets_each_declared_flag_alias() {
    let values = bind(FLAGS, &["-vq"]).unwrap();
    assert_eq!(values["verbose"], Value::Bool(true));
    assert_eq!(values["quiet"], Value::Bool(true));
}

#[test]
fn cluster_with_an_undeclared_letter_is_refused() {
    let error = bind(FLAGS, &["-vx"]).unwrap_err();
    assert!(error.to_string().contains("unknown flag \"-x\""), "{error}");
}

#[test]
fn repeated_letter_in_a_cluster_is_refused() {
    let error = bind(FLAGS, &["-vv"]).unwrap_err();
    assert!(
        error.to_string().contains("given more than once"),
        "{error}"
    );
}

#[test]
fn cluster_is_not_split_when_its_first_letter_is_not_a_flag() {
    let source = "accept verbose as boolean from flag \"--verbose\" alias \"-v\"\naccept count as text from argument 1\nrun \"printf\" with arguments [\"%s\\n\", count]\n";
    let values = bind(source, &["-12"]).unwrap();
    assert_eq!(values["count"], Value::String("-12".into()));
    assert_eq!(values["verbose"], Value::Bool(false));
}

#[test]
fn option_value_that_looks_like_a_cluster_is_not_split() {
    let source = "accept verbose as boolean from flag \"--verbose\" alias \"-v\"\naccept name as text from option \"--name\"\nrun \"printf\" with arguments [\"%s\\n\", name]\n";
    let values = bind(source, &["--name", "-vq"]).unwrap();
    assert_eq!(values["name"], Value::String("-vq".into()));
    assert_eq!(values["verbose"], Value::Bool(false));
}
