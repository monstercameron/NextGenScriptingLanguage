use flow::{diagnostics::analyze_source, manifest::Registry};
use serde_json::{json, Value as JsonValue};

fn classifier(enum_from_parameter: JsonValue) -> String {
    json!({
        "name": "typed.classify",
        "binary": "fixture",
        "arguments": [],
        "parameters": {
            "labels": { "type": "list", "items": "string", "min_items": 2 }
        },
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": { "kind": "oneshot" },
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {
                    "label": { "type": "text" },
                    "accepted": { "type": "boolean" }
                },
                "required": ["label", "accepted"],
                "enum_from_parameter": enum_from_parameter
            },
            "capabilities": [],
            "effects": []
        }
    })
    .to_string()
}

fn program(compared: &str) -> String {
    format!(
        "run typed.classify with labels [\"billing\", \"other\"] as raw\n\
         then parse json as verdict\n\
         take text at \"/label\" from verdict as label\n\
         if label is \"{compared}\"\n  \
         run \"printf\" with arguments [\"billing\\n\"]\n"
    )
}

fn closed_set_diagnostics(source: &str, manifest: &str) -> Vec<flow::diagnostics::Diagnostic> {
    let registry = Registry::from_sources(&[manifest.to_owned()]).unwrap();
    analyze_source(source, &registry)
        .into_iter()
        .filter(|diagnostic| diagnostic.code == "check.literal_not_in_set")
        .collect()
}

#[test]
fn literal_outside_declared_label_set_fails_check_with_closest_fix() {
    let manifest = classifier(json!({ "label": "labels" }));
    let diagnostics = closed_set_diagnostics(&program("biling"), &manifest);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let diagnostic = &diagnostics[0];
    assert!(
        diagnostic.message.contains("\"biling\" is not one of them"),
        "{}",
        diagnostic.message
    );
    assert_eq!(diagnostic.fixes.len(), 1, "{diagnostic:#?}");
    assert_eq!(diagnostic.fixes[0].edits[0].replacement, "\"billing\"");
}

#[test]
fn literal_inside_declared_label_set_passes_check() {
    let manifest = classifier(json!({ "label": "labels" }));
    assert!(closed_set_diagnostics(&program("billing"), &manifest).is_empty());
}

#[test]
fn fields_without_enum_declaration_are_not_closed_sets() {
    let manifest = classifier(json!({}));
    assert!(closed_set_diagnostics(&program("biling"), &manifest).is_empty());
}

#[test]
fn enum_declaration_must_name_a_text_output_field() {
    let registry = Registry::from_sources(&[classifier(json!({ "missing": "labels" }))]);
    let error = registry.err().map(|error| format!("{error:#}"));
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("not an output field")),
        "{error:?}"
    );
}
