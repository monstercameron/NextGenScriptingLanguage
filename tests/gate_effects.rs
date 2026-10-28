//! Every effect a shipped manifest declares is placed in a gate category, except the
//! ones listed on purpose in `DELIBERATELY_UNDECLARED`. An unknown effect stays
//! `undeclared`, so it needs explicit approval.

use flow::agent::gate::{effect_category, DELIBERATELY_UNDECLARED};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

fn shipped_effects() -> BTreeSet<String> {
    let mut effects = BTreeSet::new();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        collect(&value, &mut effects);
    }
    effects
}

fn collect(value: &serde_json::Value, effects: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, child) in fields {
                if key == "effects" {
                    if let Some(items) = child.as_array() {
                        effects.extend(
                            items
                                .iter()
                                .filter_map(|item| item.as_str().map(String::from)),
                        );
                    }
                }
                collect(child, effects);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect(item, effects);
            }
        }
        _ => {}
    }
}

#[test]
fn shipped_manifest_effects_are_all_placed() {
    let effects = shipped_effects();
    assert!(
        !effects.is_empty(),
        "the manifests declare no effects to audit"
    );
    for effect in &effects {
        if effect_category(effect) == "undeclared" {
            assert!(
                DELIBERATELY_UNDECLARED.contains(&effect.as_str()),
                "shipped effect {effect:?} is undeclared by accident; map it in EXPLICIT_EFFECTS or list it in DELIBERATELY_UNDECLARED"
            );
        }
    }
}

#[test]
fn the_deliberately_undeclared_list_matches_the_shipped_manifests() {
    let effects = shipped_effects();
    for effect in DELIBERATELY_UNDECLARED {
        assert!(
            effects.contains(*effect),
            "{effect:?} is listed as deliberately undeclared but no shipped manifest declares it"
        );
        assert_eq!(effect_category(effect), "undeclared");
    }
}

#[test]
fn unknown_effects_stay_undeclared() {
    assert_eq!(effect_category("made.up.effect"), "undeclared");
    assert_eq!(effect_category("filesystem.unknown"), "undeclared");
}

#[test]
fn the_demo_passes_the_gate_when_approved_and_declared() {
    // The header declares the demo's categories; approvals come only from --allow.
    let program = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo/theme-refresh.program");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("explain")
        .arg(&program)
        .arg("--json")
        .arg("--policy-header")
        .args([
            "--allow",
            "reads",
            "--allow",
            "writes",
            "--allow",
            "process",
            "--allow",
            "undeclared",
        ])
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .output()
        .unwrap();
    let verdict: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verdict["allowed"], true, "{verdict}");
    assert_eq!(
        verdict["violations"].as_array().unwrap().len(),
        0,
        "{verdict}"
    );
}

#[test]
fn package_installs_and_service_control_need_the_system_category() {
    // Approving `process` must not also approve changes to installed packages or services.
    assert_eq!(effect_category("package.install"), "system");
    assert_eq!(effect_category("systemd.control"), "system");
    assert_eq!(effect_category("process.spawn"), "process");
}
