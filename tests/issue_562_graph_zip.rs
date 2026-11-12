#![cfg(feature = "graph")]

use flow::graph::{GraphNode, GraphNodeKind, GraphPlan, PortSchema, ValueSchema};
use flow::manifest::Registry;
use std::collections::BTreeMap;

fn schema(kind: &str) -> ValueSchema {
    serde_json::from_value(serde_json::json!({ "type": kind })).unwrap()
}

fn zip_node(input_names: [&str; 2]) -> GraphNode {
    GraphNode {
        id: "zip".into(),
        kind: GraphNodeKind::Zip,
        inputs: input_names
            .iter()
            .map(|name| (name.to_string(), PortSchema::records(schema("json"))))
            .collect::<BTreeMap<_, _>>(),
        outputs: BTreeMap::from([(
            "pairs".into(),
            PortSchema::records(
                serde_json::from_value(serde_json::json!({
                    "type": "list",
                    "items": { "type": "json" }
                }))
                .unwrap(),
            ),
        )]),
        events: BTreeMap::new(),
        line: None,
    }
}

fn plan_with(node: GraphNode) -> GraphPlan {
    let mut plan = GraphPlan::new("zip_check");
    plan.add_node(node).unwrap();
    plan
}

#[test]
fn zip_with_unbound_inputs_is_refused_at_check() {
    let plan = plan_with(zip_node(["left", "right"]));
    let error = plan
        .validate_with_registry(&Registry::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("zip node \"zip\" input \"left\" must be fed by exactly one edge"),
        "unexpected error: {error}"
    );
}

#[test]
fn zip_with_wrong_port_names_is_refused_at_check() {
    let plan = plan_with(zip_node(["a", "b"]));
    let error = plan
        .validate_with_registry(&Registry::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("must declare inputs left and right, output pairs, and no events"),
        "unexpected error: {error}"
    );
}
