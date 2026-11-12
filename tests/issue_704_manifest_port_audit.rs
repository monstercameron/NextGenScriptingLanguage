#![cfg(feature = "graph")]
use flow::graph;
use flow::manifest::Registry;
use serde_json::Value;
use std::path::Path;

// Every manifest input port that is generic JSON while the parameter of the same
// name is a list or record. Graph admission defers these to the manifest's own
// parameter validation, so each entry needs a literal admission test below.
const GENERIC_JSON_CONTAINER_PORTS: &[&str] = &["json.filter:where"];

fn parameter_kind(parameter: &Value) -> Option<&str> {
    match parameter.get("type").and_then(Value::as_str)? {
        "json" => parameter.pointer("/schema/type").and_then(Value::as_str),
        other => Some(other),
    }
}

fn generic_json_container_ports() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read manifests directory") {
        let path = entry.expect("read manifests directory entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read manifest");
        let manifest: Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let name = manifest["name"].as_str().unwrap_or_default();
        let Some(inputs) = manifest
            .pointer("/contract/ports/inputs")
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (port, declaration) in inputs {
            if declaration.pointer("/schema/type").and_then(Value::as_str) != Some("json") {
                continue;
            }
            let Some(parameter) = manifest.pointer("/parameters").and_then(|p| p.get(port)) else {
                continue;
            };
            if matches!(
                parameter_kind(parameter),
                Some("list" | "record" | "array" | "object")
            ) {
                found.push(format!("{name}:{port}"));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn generic_json_ports_over_container_parameters_are_exactly_the_known_set() {
    let found = generic_json_container_ports();
    assert_eq!(
        found, GENERIC_JSON_CONTAINER_PORTS,
        "a manifest gained or lost a generic JSON port over a list or record parameter; \
         add a literal admission test for it and update the constant"
    );
}

// `as name` binds stdout text, so a connected json.parse result is a text
// binding, not a JSON value. Admission must reject it against the list parameter
// `where` rather than defer it to runtime. A JSON-typed connection is not
// expressible in graph syntax today (see NEEDS in the lane report).
#[test]
fn json_filter_where_rejects_a_connected_text_binding_at_admission() {
    let registry = Registry::from_sources(&[
        include_str!("../manifests/core.json_filter.json"),
        include_str!("../manifests/core.json_parse.json"),
    ])
    .unwrap();
    let source = concat!(
        "graph demo\n",
        "  run json.parse with text \"[{\\\"active\\\":true}]\" as predicates\n",
        "  run json.filter with document [{\"active\":true}] and pointer \"\" and where predicates as filtered\n",
        "end graph\n"
    );
    let error =
        graph::parse(source, &registry).expect_err("a text binding is not a JSON predicate list");
    let message = format!("{error:#}");
    assert!(message.contains("where"), "{message}");
}
