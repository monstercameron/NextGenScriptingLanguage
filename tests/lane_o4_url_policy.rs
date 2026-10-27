use std::collections::{BTreeMap, BTreeSet};

use flow::capabilities::{authorize_network_endpoint, AppletGrant, CapabilityPolicy};
use flow::manifest::Registry;

#[test]
fn release_profile_aborts_panics_to_omit_unwind_metadata() {
    let manifest = include_str!("../Cargo.toml");
    let release = manifest
        .split("[profile.release]")
        .nth(1)
        .unwrap()
        .split("\n[")
        .next()
        .unwrap();
    assert!(release
        .lines()
        .any(|line| line.trim() == "panic = \"abort\""));
}

#[test]
fn full_url_dependency_is_optional_and_enabled_by_remote() {
    let manifest = include_str!("../Cargo.toml");
    assert!(manifest.lines().any(|line| {
        let line = line.trim();
        line.starts_with("url =") && line.contains("optional = true")
    }));
    assert!(manifest
        .lines()
        .any(|line| line.trim() == "remote = [\"dep:ureq\", \"dep:mio\", \"dep:url\"]"));
}

#[test]
fn capability_policy_rejects_whatwg_numeric_host_aliases() {
    let policy = r#"{"version":1,"applets":{"remote.lookup":{"capabilities":["network.connect"],"network":true,"network_origins":["https://2130706433"]}}}"#;
    assert!(CapabilityPolicy::from_json(policy).is_err());
}

#[test]
fn capability_endpoint_rejects_non_ascii_hosts_even_when_idna_matches() {
    let source = serde_json::json!({
        "name":"remote.lookup",
        "binary":"@remote-applet",
        "arguments":[],
        "contract":{
            "version":1,
            "backend":"remote",
            "remote":{"endpoint":"https://api.example.test/v1"},
            "capabilities":[]
        }
    });
    let registry = Registry::from_sources(&[source.to_string()]).unwrap();
    let grant = AppletGrant {
        capabilities: BTreeSet::from(["network.connect".to_owned()]),
        binary_sha256: None,
        environment: BTreeSet::new(),
        filesystem: Default::default(),
        network: true,
        network_origins: BTreeSet::from(["https://xn--bcher-kva.example".to_owned()]),
        sandbox_required: false,
    };
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        applets: BTreeMap::from([("remote.lookup".to_owned(), grant)]),
        host_environment: BTreeSet::new(),
        deny_privileged: false,
        allow_wrappers: BTreeSet::new(),
    }));

    let error = authorize_network_endpoint(
        &registry,
        "remote.lookup",
        concat!("https://b", "\u{00fc}", "cher.example/v1/search"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("ASCII"));
}
