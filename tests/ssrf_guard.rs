//! Deterministic SSRF guard coverage for the remote applet transport, the
//! model helper, and the capability-policy layer.
//!
//! None of these tests touch the network: literal destinations are refused by
//! name before any resolution or socket use, the rebinding case resolves
//! through a fake helper script and is refused before connect (an owned
//! loopback listener proves no connection arrives), and the policy checks run
//! against in-memory registries.

#![cfg(all(feature = "remote", feature = "model"))]

use flow::capabilities::{authorize_network_endpoint, AppletGrant, CapabilityPolicy};
use flow::contracts::{EffectCertainty, RemoteEndpointConfig};
use flow::manifest::Registry;
use flow::remote::{RemoteClient, RemoteErrorKind};
use std::collections::{BTreeMap, BTreeSet};

fn policy_with_endpoint(endpoint: &str, origins: &[&str]) -> (Registry, &'static str) {
    let source = serde_json::json!({
        "name": "remote.lookup",
        "binary": "@remote-applet",
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "remote",
            "remote": {"endpoint": endpoint},
            "capabilities": []
        }
    });
    let registry = Registry::from_sources(&[source.to_string()]).unwrap();
    let grant = AppletGrant {
        capabilities: BTreeSet::from(["network.connect".to_owned()]),
        binary_sha256: None,
        environment: BTreeSet::new(),
        filesystem: Default::default(),
        network: true,
        network_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        sandbox_required: false,
    };
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("remote.lookup".to_owned(), grant)]),
    }));
    (registry, "remote.lookup")
}

#[test]
fn remote_literal_destinations_are_refused_before_resolution() {
    // Every address here would be refused without a grant; none may trigger
    // resolution or a socket, so this test needs no network access.
    for endpoint in [
        "https://169.254.169.254/",
        "https://169.254.169.253/",
        "https://100.100.100.200/",
        "https://10.0.0.1/",
        "https://192.168.0.1/",
        "https://127.0.0.1/",
        "https://localhost/",
        "https://[::ffff:169.254.169.254]/",
        "https://[fd00::1]/",
    ] {
        let client = RemoteClient::new(RemoteEndpointConfig::new(endpoint)).unwrap();
        let error = client.discover().unwrap_err();
        assert_eq!(
            error.kind,
            RemoteErrorKind::InvalidConfiguration,
            "{endpoint}"
        );
        assert_eq!(
            error.effect_certainty,
            EffectCertainty::NotStarted,
            "{endpoint}"
        );
    }
}

#[cfg(unix)]
#[test]
fn remote_rebinding_helper_answer_is_refused_without_connecting() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // An owned loopback listener stands in for the "public" half of a
    // rebinding answer. The refusal must happen in the resolver, before any
    // connection, so the listener must never see a peer.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();

    let directory = tempfile::tempdir().unwrap();
    let helper = directory.path().join("rebinding-helper.sh");
    std::fs::write(&helper, format!("#!/bin/sh\nprintf '127.0.0.1:{port}\\n'\n")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let launches = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&launches);
    let client = RemoteClient::new(RemoteEndpointConfig::new("https://rebind.example.test/"))
        .unwrap()
        .with_resolver_helper(&helper)
        .with_process_observer(Arc::new(move |count| {
            observed.fetch_add(count, Ordering::SeqCst);
        }));
    let error = client.discover().unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::Transport);
    assert_eq!(error.effect_certainty, EffectCertainty::NotStarted);
    assert_eq!(launches.load(Ordering::SeqCst), 1);

    let deadline = Instant::now() + Duration::from_millis(200);
    let mut connected = false;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok(_) => {
                connected = true;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    assert!(
        !connected,
        "rebinding address set must be refused before connect"
    );
}

#[test]
fn model_literal_destinations_are_refused_without_network() {
    // The check runs before fixture handling, credential reads, and sockets,
    // so even a fixture-mode request with a metadata endpoint is refused.
    for endpoint in [
        "https://169.254.169.254/",
        "https://100.100.100.200/",
        "https://10.0.0.1/",
    ] {
        let args: Vec<String> = [
            "classify",
            "--question",
            "which label fits",
            "--labels",
            "a",
            "b",
            "--endpoint",
            endpoint,
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        let mut input = &b""[..];
        let mut output = Vec::new();
        let error = flow::models::run(&args, &mut input, &mut output).unwrap_err();
        assert_eq!(flow::models::diagnostic(&error), "configuration", "{endpoint}");
        assert!(output.is_empty(), "{endpoint}");
    }
}

#[test]
fn policy_layer_refuses_non_public_literal_without_explicit_grant() {
    // A broad empty-list grant allows the origin for metadata-policy checks
    // but does not preserve a private destination.
    let (registry, name) = policy_with_endpoint("https://10.0.0.1/", &[]);
    let error = authorize_network_endpoint(&registry, name, "https://10.0.0.1/").unwrap_err();
    assert!(error.to_string().contains("non-public"), "{error:?}");
}

#[test]
fn policy_layer_preserves_explicitly_granted_literal_origin() {
    let (registry, name) =
        policy_with_endpoint("https://169.254.169.254/", &["https://169.254.169.254"]);
    authorize_network_endpoint(&registry, name, "https://169.254.169.254/").unwrap();
}
