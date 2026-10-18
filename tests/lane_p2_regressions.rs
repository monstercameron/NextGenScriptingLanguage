use flow::diagnostics::{analyze_source, DiagnosticSeverity};
use flow::inputs::validate;
use flow::manifest::Registry;
#[cfg(feature = "records")]
use flow::repair::{apply, Operation};
use flow::{ast::InputDeclaration, ast::InputType, ast::Value};

#[test]
fn check_uses_logical_statements_and_suffix_grammar() {
    for source in [
        "fetch: run \"printf\" with arguments [\"hello\\n\"]\n",
        "run \"printf\" with arguments [\n  \"%s %s\\n\",\n  \"a\",\n  \"b\"\n]\n",
        "run \"ls\" with arguments [\"-v\"] and stderr to \"e.log\"\n",
    ] {
        let errors = analyze_source(source, &Registry::default())
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "{source:?}: {errors:#?}");
    }
}

#[test]
fn parse_codes_ignore_offending_token_text_and_invisible_formats_are_flagged() {
    let diagnostics = analyze_source("run \"printf\" tabs\n", &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "parse.syntax"));

    let diagnostics = analyze_source(
        "run \"printf\" with arguments [\"x\u{202e}y\"]\n",
        &Registry::default(),
    );
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "source.invisible_format"));
}

#[test]
fn input_name_null_and_wrong_type_are_rejected_clearly() {
    let input = InputDeclaration {
        name: "null".into(),
        kind: InputType::Text,
        default: None,
        source: None,
        line: 1,
        description: None,
        choices: None,
    };
    assert!(validate(&[input])
        .unwrap_err()
        .to_string()
        .contains("invalid input name"));
    let input = InputDeclaration {
        name: "answer".into(),
        kind: InputType::Text,
        default: None,
        source: None,
        line: 1,
        description: None,
        choices: None,
    };
    assert!(input
        .validate_value(&Value::Integer(5))
        .unwrap_err()
        .to_string()
        .contains("got number"));
}

#[test]
fn environment_literal_nul_is_rejected_before_spawn() {
    let error =
        flow::parser::parse("run \"env\" and environment {\"A\": \"x\\u0000y\"}\n").unwrap_err();
    assert!(error.to_string().contains("environment value"));
    assert!(error.to_string().contains("NUL byte"));
}

#[cfg(feature = "records")]
#[test]
fn repair_add_retry_and_crlf_edits_are_local() {
    let source = "run \"printf\" with arguments [\"a\"]\r\nrun \"false\"\r\n";
    let retry = apply(
        source,
        &Operation::parse("add-retry 2 3").unwrap(),
        2,
        &Registry::default(),
    )
    .unwrap();
    assert_eq!(
        retry.after,
        "run \"printf\" with arguments [\"a\"]\r\nrun \"false\" and retry up to 3 times\r\n"
    );
}

#[test]
fn undefined_names_and_known_result_types_have_actionable_diagnostics() {
    let undefined = analyze_source(
        "accept name as text\nrun \"printf\" with arguments [\"%s\", nme]\n",
        &Registry::default(),
    );
    let diagnostic = undefined
        .iter()
        .find(|diagnostic| diagnostic.code == "check.undefined_name")
        .expect("stable undefined-name diagnostic");
    assert_eq!(
        &"accept name as text\nrun \"printf\" with arguments [\"%s\", nme]\n"
            [diagnostic.span.start_byte..diagnostic.span.end_byte],
        "nme"
    );
    assert!(!diagnostic.fixes.is_empty());

    let producer = serde_json::json!({
        "name": "fixture.number",
        "binary": "fixture-number",
        "parameters": {}, "arguments": [],
        "contract": { "version": 1, "output": { "type": "number", "encoding": "json" } }
    });
    let consumer = serde_json::json!({
        "name": "fixture.text",
        "binary": "fixture-text",
        "parameters": { "text": { "type": "string", "required": true } },
        "arguments": [{ "parameter": "text" }]
    });
    let registry = Registry::from_sources(&[producer.to_string(), consumer.to_string()]).unwrap();
    let diagnostics = analyze_source(
        "run fixture.number as total\nrun fixture.text with text total\n",
        &registry,
    );
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "applet.parameter.type"));
}

#[test]
fn parameter_rename_fix_does_not_create_a_duplicate() {
    let manifest = serde_json::json!({
        "name": "text.trim_final_lf",
        "binary": "fixture-text",
        "parameters": { "text": { "type": "string", "required": true } },
        "arguments": [{ "parameter": "text" }]
    });
    let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
    let diagnostics = analyze_source(
        "run text.trim_final_lf with text \"x\" and txet \"y\"\n",
        &registry,
    );
    let unknown = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "applet.parameter.unknown")
        .unwrap();
    assert!(unknown.fixes.is_empty());
}

#[test]
#[cfg(feature = "lsp")]
fn malformed_lsp_json_gets_parse_error_and_server_continues() {
    let initialize = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
    }))
    .unwrap();
    let shutdown = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "shutdown"
    }))
    .unwrap();
    let mut input = Vec::new();
    for body in [
        initialize.as_slice(),
        b"{bad json}".as_slice(),
        shutdown.as_slice(),
    ] {
        input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        input.extend_from_slice(body);
    }
    let mut output = Vec::new();
    flow::language_server::run_with_io(input.as_slice(), &mut output, Registry::default()).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("-32700"), "{output}");
    assert!(
        output.contains("\"id\":2"),
        "shutdown reply missing: {output}"
    );

    let mut missing_length = b"X-Header: invalid\r\n\r\n".to_vec();
    missing_length
        .extend_from_slice(format!("Content-Length: {}\r\n\r\n", shutdown.len()).as_bytes());
    missing_length.extend_from_slice(&shutdown);
    let mut output = Vec::new();
    flow::language_server::run_with_io(missing_length.as_slice(), &mut output, Registry::default())
        .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("-32700"), "{output}");
    assert!(
        output.contains("\"id\":2"),
        "shutdown reply missing: {output}"
    );
}
