use flow::manifest::{CacheableEffectConflict, Registry};
use serde_json::json;
use std::path::Path;

fn manifest(effect: Option<&str>, cacheable: bool) -> String {
    let effects: Vec<_> = effect.into_iter().collect();
    json!({
        "name":"fixture", "binary":"fixture", "cacheable":cacheable,
        "arguments":[], "contract":{"version":1,"capabilities":effects,"effects":effects}
    })
    .to_string()
}

#[test]
fn cacheable_nonread_effects_have_a_specific_typed_error() {
    for effect in [
        "fs.write",
        "network.loopback",
        "process.spawn",
        "systemd.control",
        "custom.effect",
    ] {
        let error = Registry::from_sources(&[manifest(Some(effect), true)])
            .err()
            .unwrap();
        let conflict = error
            .downcast_ref::<CacheableEffectConflict>()
            .expect("typed conflict");
        assert_eq!(conflict.effect, effect);
        assert!(format!("{error:#}").contains("cacheable_effect_conflict"));
        assert!(Registry::from_sources(&[manifest(Some(effect), false)]).is_ok());
    }
    let incomplete = json!({"name":"fixture","binary":"fixture","cacheable":true,"arguments":[],"contract":{"version":1,"effects":["fs.write"]}}).to_string();
    let error = Registry::from_sources(&[incomplete]).err().unwrap();
    assert!(error.downcast_ref::<CacheableEffectConflict>().is_some());
    for effect in [None, Some("fs.read"), Some("clock.read")] {
        assert!(Registry::from_sources(&[manifest(effect, true)]).is_ok());
    }
}

#[test]
fn all_shipped_manifests_load_with_full_validation() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    Registry::load_default(&dir, flow::manifest::ManifestCheck::Full)
        .unwrap()
        .manifests()
        .unwrap();
}

#[cfg(feature = "graph")]
#[test]
fn graph_replay_requires_a_declared_read_only_contract() {
    use flow::ast::Invocation;
    use flow::graph::{GraphNode, GraphPlan};
    use std::collections::BTreeMap;
    let call = Invocation {
        operation: "fixture".into(),
        direct: false,
        discard_stderr: false,
        parameters: BTreeMap::new(),
        binding: None,
        cache: false,
        sensitive_binding: false,
        status_binding: None,
        result_binding: None,
        retry: None,
    };
    let node = GraphNode::applet("node", call, BTreeMap::new(), BTreeMap::new());
    let plan = GraphPlan::new("test");
    let legacy = Registry::from_sources(&[
        json!({"name":"fixture","binary":"fixture","cacheable":true,"arguments":[]}).to_string(),
    ])
    .unwrap();
    assert!(!plan.checkpoint_retry_safe(&node, &legacy).unwrap());
    for effect in [None, Some("fs.read")] {
        let registry = Registry::from_sources(&[manifest(effect, true)]).unwrap();
        assert!(plan.checkpoint_retry_safe(&node, &registry).unwrap());
    }
}

#[test]
fn explain_reports_retry_safety_from_effect_contracts() {
    let program = flow::parser::parse("run fixture and retry up to 1 time\n").unwrap();
    let is_reported_idempotent = |manifest: String| {
        let registry = Registry::from_sources(&[manifest]).unwrap();
        let plan = flow::agent::explain::explain_json(&program, &registry, "case.program").unwrap();
        plan["steps"][0]["retry"]["idempotent"].as_bool().unwrap()
    };

    let legacy_cacheable = json!({
        "name":"fixture", "binary":"fixture", "cacheable":true, "arguments":[]
    })
    .to_string();
    assert!(
        !is_reported_idempotent(legacy_cacheable),
        "legacy cacheability alone does not establish replay safety"
    );
    assert!(
        is_reported_idempotent(manifest(None, true)),
        "a cacheable applet with a read-only contract is replay-safe"
    );
}

#[test]
fn check_reports_cacheable_effect_conflict_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("bad.json");
    std::fs::write(&manifest_path, manifest(Some("fs.write"), true)).unwrap();
    let program = dir.path().join("case.program");
    std::fs::write(&program, "run fixture\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("check")
        .arg(program)
        .arg("--manifest-dir")
        .arg(manifest_path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cacheable_effect_conflict"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
