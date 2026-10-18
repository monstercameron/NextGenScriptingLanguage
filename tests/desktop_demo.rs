#![cfg(feature = "graph")]
use flow::ast::Value;
use flow::graph;
use flow::manifest::Registry;
use std::collections::BTreeMap;

fn demo(producer: &str, renderer: &str) -> flow::graph::GraphPlan {
    let source = include_str!("../examples/desktop/clock.program.in")
        .replace("{{CLOCK_SOURCE}}", &format!("desktop.clock_{producer}"))
        .replace("{{RENDERER}}", renderer);
    let registry =
        Registry::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
            .unwrap();
    let plan = graph::parse(&source, &registry).unwrap();
    plan.validate_with_registry(&registry).unwrap();
    plan
}

#[test]
fn local_and_remote_clock_producers_share_tui_and_quickshell_graph_contracts() {
    for producer in ["local", "remote"] {
        for renderer in ["tui", "quickshell"] {
            let plan = demo(producer, renderer);
            assert_eq!(plan.nodes().len(), 6);
            assert!(plan.nodes().iter().any(|node| node.id == "clock_source"));
            assert!(plan.nodes().iter().any(|node| node.id == "formatter"));
            assert!(plan.nodes().iter().any(|node| node.id == "clock"));
            assert!(plan.nodes().iter().any(|node| node.id == "sync_button"));
            assert!(plan.nodes().iter().any(|node| node.id == "sync"));
        }
    }
}

#[test]
fn desktop_clicked_contract_accepts_empty_record_and_rejects_unknown_fields() {
    let registry =
        Registry::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
            .unwrap();
    let manifest = registry.resolve("desktop.fixture_sync").unwrap();
    let empty = BTreeMap::from([("click".to_owned(), Value::Record(BTreeMap::new()))]);
    manifest.build_arguments(&empty).unwrap();

    let extra = BTreeMap::from([(
        "click".to_owned(),
        Value::Record(BTreeMap::from([(
            "unexpected".to_owned(),
            Value::Bool(true),
        )])),
    )]);
    assert!(manifest.build_arguments(&extra).is_err());
}
