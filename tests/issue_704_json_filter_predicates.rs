#![cfg(feature = "graph")]
use flow::graph;
use flow::manifest::Registry;

fn filter_registry() -> Registry {
    Registry::from_sources(&[include_str!("../manifests/core.json_filter.json")]).unwrap()
}

fn filter_graph(where_literal: &str) -> String {
    format!(
        "graph demo\n  run json.filter with document [{{\"active\":true}}] and pointer \"\" and where {where_literal} as filtered\nend graph\n"
    )
}

#[test]
fn json_filter_accepts_valid_literal_predicates_at_graph_admission() {
    let registry = filter_registry();
    let source = filter_graph(r#"[{"pointer":"/active","equals":true}]"#);
    graph::parse(&source, &registry)
        .unwrap_or_else(|error| panic!("valid json.filter predicates rejected: {error:#}"));
}

#[test]
fn json_filter_rejects_malformed_literal_predicates_at_graph_admission() {
    let registry = filter_registry();
    for where_literal in [
        r#"[{"pointer":"/active"}]"#,
        r#"[{"exists":"yes"}]"#,
        r#"[{"exists":true,"equals":1}]"#,
        r#"[{"pointer":"/active","equals":true,"extra":1}]"#,
    ] {
        let source = filter_graph(where_literal);
        let error = graph::parse(&source, &registry)
            .expect_err("malformed json.filter predicate must be rejected at admission");
        let message = format!("{error:#}");
        assert!(message.contains("where"), "{where_literal}: {message}");
    }
}

#[test]
fn json_filter_rejects_non_list_where_at_graph_admission() {
    let registry = filter_registry();
    let source = filter_graph(r#"{"pointer":"/active","equals":true}"#);
    let error = graph::parse(&source, &registry)
        .expect_err("a single predicate record is not a list of predicates");
    let message = format!("{error:#}");
    assert!(message.contains("where"), "{message}");
}
