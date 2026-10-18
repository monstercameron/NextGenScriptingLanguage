use serde_json::Value;

fn playground_sources() -> Vec<(String, String)> {
    let examples: Value = serde_json::from_str(include_str!("../web/examples.json"))
        .expect("playground examples should be valid JSON");
    examples
        .as_array()
        .expect("playground examples should be an array")
        .iter()
        .map(|example| {
            (
                example["id"]
                    .as_str()
                    .expect("each playground example should have an id")
                    .to_owned(),
                example["source"]
                    .as_str()
                    .expect("each playground example should have source")
                    .to_owned(),
            )
        })
        .collect()
}

#[test]
fn playground_does_not_use_printf_to_bind_literal_values() {
    let offenders: Vec<_> = playground_sources()
        .into_iter()
        .flat_map(|(id, source)| {
            source
                .lines()
                .filter(|line| {
                    let Some((_, arguments)) = line.split_once("run \"printf\" with arguments [\"")
                    else {
                        return false;
                    };
                    arguments
                        .split_once("\"] as ")
                        .is_some_and(|(literal, _)| !literal.contains('%'))
                })
                .map(move |line| format!("{id}: {line}"))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "literal printf bindings remain:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn playground_does_not_build_json_with_printf_templates() {
    let offenders: Vec<_> = playground_sources()
        .into_iter()
        .flat_map(|(id, source)| {
            source
                .lines()
                .filter(|line| {
                    line.contains("run \"printf\" with arguments [\"") && line.contains("{\\\"")
                })
                .map(move |line| format!("{id}: {line}"))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "printf JSON templates remain:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn playground_builds_reports_from_records_and_lists() {
    let sources = playground_sources();
    for id in [
        "incident-command",
        "launch-control",
        "support-signal",
        "disk-usage",
        "service-health",
        "cert-expiry",
        "ci-triage",
    ] {
        let source = sources
            .iter()
            .find(|(example_id, _)| example_id == id)
            .map(|(_, source)| source)
            .unwrap_or_else(|| panic!("missing playground demo {id}"));
        assert!(
            source.contains("run json.stringify with value {"),
            "{id} should serialize a record literal"
        );
    }

    let inventory = sources
        .iter()
        .find(|(id, _)| id == "inventory-report")
        .map(|(_, source)| source)
        .expect("inventory report demo should exist");
    assert!(inventory.contains("json.set with document role_items and pointer \"/-\""));
    assert!(inventory.contains("json.set with document system_items and pointer \"/-\""));
    assert!(!inventory.contains("text.concat"));

    let health = sources
        .iter()
        .find(|(id, _)| id == "service-health")
        .map(|(_, source)| source)
        .expect("service health demo should exist");
    assert!(health.contains("text.format with template"));
}
