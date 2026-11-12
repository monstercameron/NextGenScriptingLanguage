use flow::{
    capabilities::{authorize_network_endpoint, AppletGrant, CapabilityPolicy, FilesystemGrant},
    manifest::Registry,
};
use std::collections::{BTreeMap, BTreeSet};

fn network_grant(origins: &[&str]) -> AppletGrant {
    AppletGrant {
        capabilities: BTreeSet::from(["network.connect".to_owned()]),
        binary_sha256: None,
        environment: BTreeSet::new(),
        filesystem: FilesystemGrant::default(),
        network: true,
        network_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        sandbox_required: false,
        resource_limits: None,
    }
}

fn fixture_registry() -> Registry {
    let source = serde_json::json!({
        "name": "remote.fixture",
        "binary": "@remote-applet",
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "remote",
            "remote": {
                "endpoint": "http://127.0.0.1:8080/v1",
                "allow_loopback_http_fixture": true
            },
            "capabilities": []
        }
    });
    Registry::from_sources(&[source.to_string()]).unwrap()
}

fn install_policy(registry: &Registry, grant: AppletGrant) {
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("remote.fixture".into(), grant)]),
    }));
}

#[test]
fn fixture_flag_does_not_authorize_loopback_under_policy_without_listed_origin() {
    let registry = fixture_registry();
    // Broad grant: network true with no listed origins.
    install_policy(&registry, network_grant(&[]));
    assert!(
        authorize_network_endpoint(&registry, "remote.fixture", "http://127.0.0.1:8080/v1/x")
            .is_err()
    );
    assert!(
        authorize_network_endpoint(&registry, "remote.fixture", "http://localhost:8080/v1/x")
            .is_err()
    );
}

#[test]
fn loopback_is_authorized_under_policy_once_origin_is_listed() {
    let registry = fixture_registry();
    install_policy(&registry, network_grant(&["http://127.0.0.1:8080"]));
    authorize_network_endpoint(&registry, "remote.fixture", "http://127.0.0.1:8080/v1/x").unwrap();
    // A different loopback origin stays refused.
    assert!(
        authorize_network_endpoint(&registry, "remote.fixture", "http://localhost:8080/v1/x")
            .is_err()
    );
}
