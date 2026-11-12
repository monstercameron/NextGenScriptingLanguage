#![cfg(feature = "graph")]
//! #372: the graph admission limit used by `check` is configurable, so a
//! 100-applet scale graph can be validated by an embedder that raises
//! `GraphRunOptions::max_nodes`, while the default limit still refuses it.

use flow::contracts::{OutputType, ValueSchema};
use flow::graph::{GraphNode, GraphNodeKind, GraphPlan, GraphRunOptions, PortSchema};
use flow::manifest::Registry;
use std::collections::BTreeMap;

fn integer_schema() -> ValueSchema {
    ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Integer,
        nullable: false,
        enum_values: None,
        single_line: false,
        minimum: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
        pattern: None,
    }
}

fn timer_plan(nodes: usize) -> GraphPlan {
    let mut plan = GraphPlan::new("scale");
    for index in 0..nodes {
        plan.add_node(GraphNode {
            id: format!("timer{index}"),
            kind: GraphNodeKind::Timer { interval_ms: 1000 },
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            events: BTreeMap::from([("tick".into(), PortSchema::events(integer_schema()))]),
            line: None,
        })
        .unwrap();
    }
    plan
}

#[test]
fn hundred_node_graph_is_admitted_only_when_max_nodes_is_raised() {
    let registry = Registry::default();
    let plan = timer_plan(100);

    let raised = GraphRunOptions {
        max_nodes: 100,
        ..GraphRunOptions::default()
    };
    plan.validate_with_registry_and_limits(&registry, &raised)
        .unwrap();

    let default_limit = GraphRunOptions::default();
    let error = plan
        .validate_with_registry_and_limits(&registry, &default_limit)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("graph node count 100 exceeds max_nodes 64"),
        "{error:#}"
    );

    let error = plan.validate_with_registry(&registry).unwrap_err();
    assert!(
        format!("{error:#}").contains("graph node count 100 exceeds max_nodes 64"),
        "{error:#}"
    );
}
