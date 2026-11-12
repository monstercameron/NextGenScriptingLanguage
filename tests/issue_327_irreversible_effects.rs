//! Issue #327, first open criterion: `explain` lists each step's irreversible
//! effects, declared by the applet contract, and adds a summary list. The plan
//! comes from `flow::agent::explain::explain_json`; nothing runs. An applet
//! whose effects are all reversible shows none.

use flow::agent::explain;
use flow::manifest::Registry;
use flow::parser;
use serde_json::{json, Value};

fn applet(name: &str, effects: Value, reversibility: Value) -> String {
    json!({
        "name": name,
        "binary": env!("CARGO_BIN_EXE_execute"),
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [{ "literal": "--version" }],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": { "kind": "oneshot" },
            "capabilities": effects,
            "effects": effects,
            "reversibility": reversibility,
        }
    })
    .to_string()
}

fn plan_for(program: &str) -> Value {
    let sources = [
        applet(
            "mixed.helper",
            json!(["fs.write.workspace", "network.loopback"]),
            json!({
                "fs.write.workspace": {"kind": "reversible", "compensate": "fs.restore"},
                "network.loopback": {"kind": "irreversible"}
            }),
        ),
        applet(
            "reversible.helper",
            json!(["fs.write.workspace"]),
            json!({
                "fs.write.workspace": {"kind": "reversible", "compensate": "fs.restore"}
            }),
        ),
    ];
    let registry = Registry::from_sources(&sources).expect("embedded manifests load");
    let parsed = parser::parse(program).expect("program parses");
    explain::explain_json(&parsed, &registry, "reversibility.program").expect("plan")
}

#[test]
fn irreversible_effect_is_listed_on_its_step_and_in_the_summary() {
    let plan = plan_for("run mixed.helper\n");
    let steps = plan["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0]["applet"], "mixed.helper");
    assert_eq!(
        steps[0]["irreversible_effects"],
        json!(["network.loopback"]),
        "{steps:?}"
    );
    assert_eq!(
        plan["summary"]["irreversible_effects"],
        json!(["network.loopback"]),
        "{}",
        plan["summary"]
    );
}

#[test]
fn applet_with_only_reversible_effects_shows_no_irreversible_effects() {
    let plan = plan_for("run reversible.helper\n");
    let step = &plan["steps"][0];
    assert_eq!(step["applet"], "reversible.helper");
    assert!(
        step.get("irreversible_effects").is_none(),
        "no irreversible key on the row: {step}"
    );
    assert!(
        plan["summary"].get("irreversible_effects").is_none(),
        "no irreversible key in the summary: {}",
        plan["summary"]
    );
}

#[test]
fn summary_lists_irreversible_effects_from_every_step_once() {
    let plan = plan_for("run mixed.helper\nrun mixed.helper\nrun reversible.helper\n");
    assert_eq!(
        plan["summary"]["irreversible_effects"],
        json!(["network.loopback"]),
        "{}",
        plan["summary"]
    );
}
