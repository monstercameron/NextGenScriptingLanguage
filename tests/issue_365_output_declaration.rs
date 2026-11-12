//! Issue #365: a program's `produce TYPE [described as "TEXT"]` declaration parses, is
//! reported by inspect (`inputs::describe_with_output`) and by help, validates against the
//! published inspect schema, and survives the formatter. A program without the declaration
//! reports no `output` at all.

use flow::ast::OutputType;
use flow::parser::parse;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

fn describe_program(source: &str) -> Value {
    let program = parse(source).unwrap();
    flow::inputs::describe_with_output(&program.inputs, &BTreeMap::new(), program.output.as_ref())
        .unwrap()
}

/// The subset of JSON Schema that docs/schemas/inspect.v1.schema.json uses. The same
/// checker as tests/issue_364_protocol_schemas.rs, copied so this file stands alone.
fn violations(schema: &Value, instance: &Value, path: &str, errors: &mut Vec<String>) {
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = branches
            .iter()
            .filter(|branch| {
                let mut branch_errors = Vec::new();
                violations(branch, instance, path, &mut branch_errors);
                branch_errors.is_empty()
            })
            .count();
        if matching != 1 {
            errors.push(format!("{path}: matches {matching} oneOf branches"));
        }
    }
    if let Some(constant) = schema.get("const") {
        if constant != instance {
            errors.push(format!("{path}: expected {constant}"));
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(instance) {
            errors.push(format!("{path}: {instance} is not an allowed value"));
        }
    }
    if let Some(kind) = schema.get("type") {
        let kinds: Vec<&str> = match kind {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let matched = kinds.iter().any(|name| match *name {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "integer" => instance.is_i64() || instance.is_u64(),
            "number" => instance.is_number(),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            _ => false,
        });
        if !matched {
            errors.push(format!("{path}: expected type {kind}"));
            return;
        }
    }
    if let (Some(minimum), Some(number)) = (
        schema.get("minimum").and_then(Value::as_f64),
        instance.as_f64(),
    ) {
        if number < minimum {
            errors.push(format!("{path}: {number} is below {minimum}"));
        }
    }
    match instance {
        Value::Object(map) => {
            for name in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !map.contains_key(name) {
                    errors.push(format!("{path}: missing required property {name}"));
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
            for (key, value) in map {
                match properties.and_then(|known| known.get(key)) {
                    Some(sub) => violations(sub, value, &format!("{path}/{key}"), errors),
                    None if closed => errors.push(format!("{path}: unexpected property {key}")),
                    None => {}
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    violations(item_schema, item, &format!("{path}/{index}"), errors);
                }
            }
        }
        _ => {}
    }
}

fn inspect_errors(instance: &Value) -> Vec<String> {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/schemas/inspect.v1.schema.json"),
    )
    .unwrap();
    let schema: Value = serde_json::from_str(&text).unwrap();
    let mut errors = Vec::new();
    violations(&schema, instance, "", &mut errors);
    errors
}

#[test]
fn every_output_type_parses_with_and_without_a_description() {
    for (word, kind) in [
        ("text", OutputType::Text),
        ("integer", OutputType::Integer),
        ("boolean", OutputType::Boolean),
        ("list of text", OutputType::TextList),
        ("json", OutputType::Json),
        ("none", OutputType::None),
    ] {
        let plain = parse(&format!("produce {word}\n")).unwrap();
        let output = plain.output.expect(word);
        assert_eq!(
            (output.kind, output.description, output.line),
            (kind, None, 1)
        );

        let described = parse(&format!("produce {word} described as \"Result\"\n")).unwrap();
        let output = described.output.expect(word);
        assert_eq!(
            (output.kind, output.description.as_deref()),
            (kind, Some("Result"))
        );
    }
}

#[test]
fn near_misses_are_parse_errors_that_name_their_line() {
    let cases = [
        ("produce\n", "line 1: output type must be"),
        ("produce bytes\n", "line 1: output type must be"),
        ("produce list of integer\n", "line 1: output type must be"),
        ("produce list of json\n", "line 1: output type must be"),
        ("produce text one of [\"a\"]\n", "stray token `one`"),
        (
            "produce text described \"Result\"\n",
            "stray token `described`",
        ),
        (
            "produce text described as\n",
            "must be followed by a quoted description",
        ),
        (
            "produce text\nproduce text\n",
            "line 2: a program declares one output",
        ),
        (
            "run \"true\"\nproduce text\n",
            "line 2: the output declaration must precede",
        ),
    ];
    for (source, expected) in cases {
        let error = parse(source).expect_err(source);
        let message = format!("{error:#}");
        assert!(message.contains(expected), "{source:?}: {message}");
    }
}

#[test]
fn a_second_declaration_names_both_lines() {
    let error = parse("accept name as text\nproduce text\n\nproduce json\n").unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("line 4: a program declares one output, and line 2 already declares it"),
        "{message}"
    );
}

#[test]
fn inspect_reports_the_declared_output_and_only_then() {
    let declared = describe_program(
        "accept name as text\nproduce json described as \"Per-host report\"\nrun \"true\"\n",
    );
    assert_eq!(
        declared["output"],
        json!({"type": "json", "description": "Per-host report", "line": 2})
    );

    let undeclared = describe_program("accept name as text\nrun \"true\"\n");
    assert!(undeclared.get("output").is_none(), "{undeclared}");
    assert_eq!(undeclared["inputs"][0]["name"], "name");
}

#[test]
fn inspect_schema_accepts_programs_with_and_without_the_declaration() {
    let declared = describe_program(
        "accept name as text described as \"Who\"\nproduce text described as \"Greeting\"\nrun \"true\"\n",
    );
    assert_eq!(
        inspect_errors(&declared),
        Vec::<String>::new(),
        "{declared}"
    );

    let plain_declaration = describe_program("produce none\nrun \"true\"\n");
    assert_eq!(
        inspect_errors(&plain_declaration),
        Vec::<String>::new(),
        "{plain_declaration}"
    );

    let undeclared = describe_program("accept name as text\nrun \"true\"\n");
    assert_eq!(
        inspect_errors(&undeclared),
        Vec::<String>::new(),
        "{undeclared}"
    );

    let mut damaged = declared.clone();
    damaged["output"]["type"] = json!("list of integer");
    assert!(!inspect_errors(&damaged).is_empty(), "{damaged}");
    let mut extra = declared;
    extra["output"]["shape"] = json!("record");
    assert!(!inspect_errors(&extra).is_empty(), "{extra}");
}

#[test]
fn help_reports_the_declared_output_before_the_inputs() {
    let program =
        parse("accept name as text\nproduce text described as \"Greeting\"\nrun \"true\"\n")
            .unwrap();
    let help = flow::agent::help::describe(&program, "greet.program");
    assert_eq!(help["output"]["type"], "text");
    assert_eq!(help["output"]["description"], "Greeting");
    let text = flow::agent::help::render_program_help(&program, "greet.program");
    let output_at = text.find("Output: text  Greeting").expect(&text);
    assert!(text.find("Inputs:").unwrap() > output_at, "{text}");

    let plain = parse("run \"true\"\n").unwrap();
    assert!(flow::agent::help::describe(&plain, "plain.program")
        .get("output")
        .is_none());
    assert!(!flow::agent::help::render_program_help(&plain, "plain.program").contains("Output:"));
}

#[test]
fn formatter_keeps_the_declaration_when_blank_lines_move_it() {
    // The blank lines move `produce` from line 4 to line 3 after formatting. The formatter
    // compares programs with line numbers cleared, so this must still format.
    let source = "accept name as text\n\n\nproduce json described as \"Report\"\nrun \"true\"\n";
    let formatted = flow::format::format_program(source).unwrap();
    assert!(
        formatted.contains("produce json described as \"Report\"\n"),
        "{formatted}"
    );
    assert_eq!(flow::format::format_program(&formatted).unwrap(), formatted);
}
