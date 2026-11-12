//! The generated GBNF is built from the single table `flow::parser::GRAMMAR_FORMS`.
//! These tests check the emitted text: every form has exactly one rule, every rule
//! the statement alternation or a body names is defined, and rule names are unique.

use std::collections::BTreeSet;

use flow::parser::{gbnf_grammar, GRAMMAR_FORMS};

/// (name, body) for each `name ::= body` line of the generated grammar.
fn rules(grammar: &str) -> Vec<(String, String)> {
    grammar
        .lines()
        .filter_map(|line| {
            let (name, body) = line.split_once(" ::= ")?;
            Some((name.to_owned(), body.to_owned()))
        })
        .collect()
}

/// The rule names a GBNF body refers to. Quoted literals and bracketed character
/// classes are skipped, including their escapes.
fn referenced_names(body: &str) -> BTreeSet<String> {
    let mut plain = String::new();
    let mut chars = body.chars();
    let mut in_quote = false;
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' if !in_class => {
                in_quote = !in_quote;
                plain.push(' ');
            }
            '[' if !in_quote => in_class = true,
            ']' if in_class => in_class = false,
            _ if in_quote || in_class => {}
            _ => plain.push(c),
        }
    }
    plain
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|word| word.starts_with(|c: char| c.is_ascii_alphabetic()))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_form_has_exactly_one_rule_and_the_statement_rule_lists_them_all() {
    let grammar = gbnf_grammar();
    let rules = rules(&grammar);
    let names: BTreeSet<&str> = rules.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names.len(), rules.len(), "a rule name is defined twice");

    let statement = rules
        .iter()
        .find(|(name, _)| name == "statement")
        .expect("no statement rule");
    let alternatives: BTreeSet<&str> = statement.1.split(" | ").collect();
    let forms: BTreeSet<&str> = GRAMMAR_FORMS.iter().map(|form| form.rule).collect();
    assert_eq!(
        alternatives, forms,
        "the statement rule must list exactly the table's forms"
    );
    assert_eq!(forms.len(), GRAMMAR_FORMS.len(), "a form rule is repeated");
    for form in GRAMMAR_FORMS {
        assert!(names.contains(form.rule), "no rule for `{}`", form.rule);
    }
    assert!(names.contains("root"), "no root rule");
}

#[test]
fn every_referenced_rule_is_defined() {
    let grammar = gbnf_grammar();
    let rules = rules(&grammar);
    let defined: BTreeSet<&str> = rules.iter().map(|(name, _)| name.as_str()).collect();
    for (name, body) in &rules {
        for reference in referenced_names(body) {
            assert!(
                defined.contains(reference.as_str()),
                "rule `{name}` refers to undefined rule `{reference}`"
            );
        }
    }
}

#[test]
fn the_reference_scanner_ignores_literals_and_classes() {
    let names = referenced_names(r#""run " text ("with" | "a-b") [A-Za-z] "\"" applet"#);
    let expected: BTreeSet<String> = ["text", "applet"].iter().map(|s| s.to_string()).collect();
    assert_eq!(names, expected);
}
