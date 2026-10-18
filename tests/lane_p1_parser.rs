use flow::ast::{step_ids, StepKind};
use flow::parser;

#[test]
fn otherwise_body_may_use_its_own_consistent_indentation() {
    let source = "accept flag as boolean with default true\nif flag is true\n  run \"printf\" with arguments [\"yes\"]\notherwise\n    run \"printf\" with arguments [\"no\"]\n";
    assert!(parser::parse(source).is_ok());
}

#[test]
fn labelled_program_call_keeps_label_and_expanded_labels_are_unique() {
    let source = "define a program named greet\n  hello: run \"printf\" with arguments [\"hi\"]\ncall-one: run program greet\ncall-two: run program greet\nhello: run \"printf\" with arguments [\"main\"]\n";
    let program = parser::parse(source).unwrap();
    let ids = step_ids(&program.steps);
    assert!(ids.contains(&"call-one".to_owned()));
    assert!(ids.contains(&"call-two".to_owned()));
    assert!(ids.contains(&"hello".to_owned()));
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        ids.len()
    );
}

#[test]
fn bracketed_continuation_and_bom_share_the_program_parser() {
    let source = "\u{feff}run \"printf\" with arguments [\n  \"a\",\n  \"b\"\n]\n";
    let program = parser::parse(source).unwrap();
    assert!(matches!(program.steps[0].kind, StepKind::Invoke { .. }));
}

#[test]
fn continuation_errors_keep_the_physical_line_and_plain_errors_keep_the_cause() {
    let continuation = "run \"printf\" with arguments [\n  \"a\",\n  \"unterminated,\n]\n";
    let error = parser::parse(continuation).unwrap_err().to_string();
    assert!(error.contains("line 3"), "{error}");
    assert!(error.contains("unterminated quoted string"), "{error}");

    let plain = "run \"printf\" with arguments [\"a\", @bad]\n";
    let error = parser::parse(plain).unwrap_err().to_string();
    assert!(error.contains("invalid value `@bad`"), "{error}");
}

#[test]
fn large_bracketed_continuation_parses() {
    use std::time::{Duration, Instant};

    let mut source = String::from("run \"printf\" with arguments [\n");
    for index in 0..40_000 {
        source.push_str(&format!("  \"item{index}\",\n"));
    }
    source.push_str("  \"end\"\n]\n");
    let start = Instant::now();
    let program = parser::parse(&source).unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(program.steps.len(), 1);
}

#[test]
fn malformed_and_misplaced_version_markers_name_the_version_rule() {
    for source in [
        "execute 4294967296\nrun \"printf\" with arguments [\"a\"]\n",
        "run \"printf\" with arguments [\"a\"]\nexecute 1\n",
        "execute 1\nexecute 1\n",
        "execute -1\n",
        "execute +01\n",
        "execute one\n",
        "execute 1 2\n",
    ] {
        let error = parser::parse(source).unwrap_err().to_string();
        assert!(error.contains("version"), "{error}");
    }
}

#[test]
fn labels_cannot_use_statement_heads_and_nested_task_labels_are_refused() {
    for keyword in [
        "await", "parallel", "race", "with", "on", "launch", "execute",
    ] {
        let source = format!("{keyword}: run \"printf\" with arguments [\"a\"]\n");
        assert!(parser::parse(&source)
            .unwrap_err()
            .to_string()
            .contains("keyword"));
    }
    for name in [
        "detach", "loop", "else", "append", "parse", "encode", "save",
    ] {
        let source = format!("{name}: run \"printf\" with arguments [\"a\"]\n");
        assert!(
            parser::parse(&source).is_ok(),
            "{name} should not be reserved as a statement head"
        );
    }
    let source =
        "parallel at most 2 as results\n  a: run text.trim_final_lf with text \"x\" as t1\n";
    assert!(parser::parse(source)
        .unwrap_err()
        .to_string()
        .contains("cannot have step labels"));
    let source = "spawn as job\n  a: run \"printf\" with arguments [\"x\"]\n";
    assert!(parser::parse(source)
        .unwrap_err()
        .to_string()
        .contains("cannot have step labels"));
}

#[test]
fn integer_values_follow_json_number_syntax_and_null_names_are_reserved() {
    for value in ["0755", "+5", "007", "-007"] {
        let source = format!("run \"printf\" with arguments [{value}]\n");
        assert!(parser::parse(&source)
            .unwrap_err()
            .to_string()
            .contains("invalid JSON number"));
    }
    for source in [
        "accept null as text with default \"x\"\n",
        "run \"printf\" with arguments [\"x\"] as null\n",
        "for each null in [\"x\"]\n  run \"printf\" with arguments [null]\n",
    ] {
        assert!(parser::parse(source)
            .unwrap_err()
            .to_string()
            .contains("reserved literal `null`"));
    }
}

#[cfg(feature = "graph")]
#[test]
fn graph_parser_accepts_version_marker_and_multiline_accept_and_node_values() {
    use flow::graph;
    use flow::manifest::Registry;
    let registry = Registry::from_sources(&[
        r#"{"name":"demo.list","binary":"/usr/bin/printf","parameters":{"items":{"type":"list","items":"string","required":true}},"arguments":[{"spread":"items"}],"contract":{"version":1,"output":{"type":"bytes","encoding":"raw"}}}"#,
    ]).unwrap();
    let source = "\u{feff}execute 1\naccept greeting as text one of [\n  \"hi\",\n  \"yo\"\n] with default \"hi\"\ngraph greeting\n  run demo.list with items [\n    \"hello\",\n    \"graph\"\n  ] as joined\nend graph\n";
    let plan = graph::parse(source, &registry).unwrap();
    assert!(graph::is_graph(source));
    assert_eq!(plan.nodes().len(), 1);
    assert_eq!(plan.input_declarations().len(), 1);

    let malformed = "graph bad\n  run demo.list with items [\n    \"hello\" \"graph\"\n  ] as joined\nend graph\n";
    let error = graph::parse(malformed, &registry).unwrap_err().to_string();
    assert!(error.starts_with("line 2: "), "{error}");
    assert!(!error.contains("line 1:"), "{error}");
}
