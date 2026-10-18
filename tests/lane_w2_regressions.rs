use flow::diagnostics::analyze_source;
use flow::manifest::Registry;

#[test]
fn descriptions_resolve_intent_and_refuse_ambiguous_matches() {
    let registry = Registry::from_sources(&[
        serde_json::json!({
            "name": "fixture.render",
            "description": "Render a text template by substituting named values.",
            "binary": "fixture",
            "parameters": {},
            "arguments": []
        })
        .to_string(),
        serde_json::json!({
            "name": "fixture.render_other",
            "description": "Render a text template using another engine.",
            "binary": "fixture",
            "parameters": {},
            "arguments": []
        })
        .to_string(),
    ])
    .unwrap();
    let error = registry.resolve("render text template").unwrap_err();
    assert!(error.to_string().contains("ambiguous applet description"));
    assert!(error.to_string().contains("fixture.render"));
    assert!(error.to_string().contains("fixture.render_other"));

    let unique = Registry::from_sources(&[serde_json::json!({
        "name": "fixture.yaml_parse",
        "description": "Parse YAML text into structured data.",
        "binary": "fixture",
        "parameters": {},
        "arguments": []
    })
    .to_string()])
    .unwrap();
    assert_eq!(
        unique.resolve("parse yaml").unwrap().name,
        "fixture.yaml_parse"
    );
}

#[test]
fn undefined_names_in_conditions_and_environment_values_are_reported() {
    let diagnostics = analyze_source(
        "accept name as text\naccept environment_value as text\nif nmae is \"ready\"\n  run \"true\" and environment {\"VALUE\": enviroment_value}\n",
        &Registry::default(),
    );
    let undefined = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "check.undefined_name")
        .collect::<Vec<_>>();
    assert_eq!(undefined.len(), 2, "{undefined:#?}");
    for diagnostic in undefined {
        assert_eq!(
            diagnostic.span.precision,
            flow::diagnostics::SpanPrecision::Token
        );
        assert!(matches!(
            &"accept name as text\naccept environment_value as text\nif nmae is \"ready\"\n  run \"true\" and environment {\"VALUE\": enviroment_value}\n"
                [diagnostic.span.start_byte..diagnostic.span.end_byte],
            "nmae" | "enviroment_value"
        ));
        assert!(!diagnostic.fixes.is_empty());
    }
}

#[test]
fn undefined_names_in_iteration_use_value_and_save_path_have_token_fixes() {
    let source = "accept text_name as text\naccept list_name as list of text\naccept path_name as text\nfor each item in list_nmae\n  use value text_nmae\n  then save to path_nmae\n";
    let diagnostics = analyze_source(source, &Registry::default());
    let undefined = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "check.undefined_name")
        .collect::<Vec<_>>();
    assert_eq!(undefined.len(), 3, "{undefined:#?}");
    for diagnostic in undefined {
        assert_eq!(
            diagnostic.span.precision,
            flow::diagnostics::SpanPrecision::Token
        );
        assert!(!diagnostic.fixes.is_empty());
        assert!(matches!(
            &source[diagnostic.span.start_byte..diagnostic.span.end_byte],
            "list_nmae" | "text_nmae" | "path_nmae"
        ));
    }
}

#[test]
fn diagnostics_keep_source_order_for_many_bad_lines() {
    let source = "bogus \"x\" \"y\"\n".repeat(2_000);
    let diagnostics = analyze_source(&source, &Registry::default());
    assert_eq!(diagnostics.len(), 2_000);
    assert!(diagnostics
        .windows(2)
        .all(|pair| pair[0].span.start_byte <= pair[1].span.start_byte));
    assert_eq!(diagnostics.last().unwrap().span.line, 2_000);
}

#[test]
fn diagnostic_output_is_capped_with_an_explicit_omission_record() {
    let source = "bogus \"x\" \"y\"\n".repeat(10_001);
    let diagnostics = analyze_source(&source, &Registry::default());
    assert_eq!(diagnostics.len(), 10_000);
    let truncated = diagnostics.last().unwrap();
    assert_eq!(truncated.code, "check.diagnostics.truncated");
    assert!(truncated
        .message
        .contains("2 additional diagnostics omitted"));
}

#[test]
fn known_number_results_are_rejected_in_text_only_value_flows() {
    let producer = serde_json::json!({
        "name": "fixture.number",
        "binary": "fixture-number",
        "parameters": {},
        "arguments": [],
        "contract": { "version": 1, "output": { "type": "number", "encoding": "json" } }
    });
    let registry = Registry::from_sources(&[producer.to_string()]).unwrap();
    let diagnostics = analyze_source(
        "run fixture.number as total\nuse value total\nrun \"true\" and environment {\"VALUE\": total}\n",
        &registry,
    );
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "check.type_mismatch")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 2, "{errors:#?}");
}

#[test]
fn a_leading_bom_is_not_an_invisible_character_but_a_later_one_is() {
    let registry = flow::manifest::Registry::from_sources(&Vec::<String>::new()).unwrap();
    let leading = flow::diagnostics::analyze_source("\u{feff}use value \"x\"\n", &registry);
    assert!(
        !leading
            .iter()
            .any(|diagnostic| diagnostic.code == "source.invisible_format"),
        "{leading:?}"
    );
    let inner = flow::diagnostics::analyze_source("use value \"x\u{feff}\"\n", &registry);
    assert!(inner
        .iter()
        .any(|diagnostic| diagnostic.code == "source.invisible_format"));
}
