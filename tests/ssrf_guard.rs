//! Deterministic SSRF guard coverage for the remote applet transport, the
//! model helper, and the capability-policy layer.
//!
//! None of these tests touch the network: literal destinations are refused by
//! name before any resolution or socket use, the rebinding case resolves
//! through a fake helper script and is refused before connect (an owned
//! loopback listener proves no connection arrives), and the policy checks run
//! against in-memory registries. The granted-origin test serves an owned
//! loopback listener, which never leaves the host.

#![cfg(all(feature = "remote", feature = "model"))]

use flow::ast::Value as AstValue;
use flow::capabilities::{
    authorize_model_endpoint, authorize_model_endpoint_params, authorize_network_endpoint,
    AppletGrant, CapabilityPolicy,
};
use flow::contracts::{EffectCertainty, RemoteEndpointConfig, Sensitivity};
use flow::manifest::Registry;
use flow::remote::{RemoteClient, RemoteErrorKind};
use flow::security::{DataFlowPolicy, SensitivitySet};
use std::collections::{BTreeMap, BTreeSet};

fn model_registry_with_grant(origins: &[&str], network: bool, capabilities: &[&str]) -> Registry {
    let source = serde_json::json!({
        "name": "llm.ask",
        "binary": "@execute-model",
        "arguments": [],
        "contract": {"version": 1}
    });
    let registry = Registry::from_sources(&[source.to_string()]).unwrap();
    let grant = AppletGrant {
        capabilities: capabilities.iter().map(|name| (*name).to_owned()).collect(),
        binary_sha256: None,
        environment: BTreeSet::new(),
        filesystem: Default::default(),
        network,
        network_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        sandbox_required: false,
    };
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("llm.ask".to_owned(), grant)]),
    }));
    registry
}

fn model_params(endpoint: Option<&str>, fixture: bool) -> BTreeMap<String, AstValue> {
    let mut params = BTreeMap::new();
    if let Some(endpoint) = endpoint {
        params.insert("endpoint".to_owned(), AstValue::String(endpoint.to_owned()));
    }
    if fixture {
        params.insert(
            "fixture".to_owned(),
            AstValue::String("/tmp/fixture.json".to_owned()),
        );
    }
    params
}

fn unknown_sensitivities() -> SensitivitySet {
    SensitivitySet::from_label(Sensitivity::Unknown)
}

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
    std::fs::write(
        &helper,
        format!("#!/bin/sh\nprintf '127.0.0.1:{port}\\n'\n"),
    )
    .unwrap();
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
        assert_eq!(
            flow::models::diagnostic(&error),
            "configuration",
            "{endpoint}"
        );
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

// ---------------------------------------------------------------------------
// Model loopback grant (#448): a live loopback endpoint — either scheme —
// requires the explicit EXECUTE_MODEL_ALLOW_LOOPBACK grant, while hermetic
// fixture runs never open a socket and need no grant. The resolver contract
// underneath stays fail-closed: the grant allows loopback-only sets but never
// a mixed or metadata set.
// ---------------------------------------------------------------------------

/// The model resolver's address-set contract under the loopback grant: the
/// grant is not a bypass. Loopback-only sets pass; anything mixing in a
/// blocked address (the DNS-rebinding shape) or naming metadata is refused.
#[test]
fn model_resolver_policy_stays_fail_closed_under_the_loopback_grant() {
    use std::net::SocketAddr;

    fn addr(ip: &str, port: u16) -> SocketAddr {
        format!("{ip}:{port}").parse().unwrap()
    }

    let loopback = addr("127.0.0.1", 80);
    let metadata = addr("169.254.169.254", 80);
    let public = addr("93.184.216.34", 443);
    // Grantless default: even loopback is refused at connect time.
    assert!(flow::ssrf::check_resolved_addrs(&[loopback], false, false).is_err());
    // Explicit grant: loopback-only and public sets pass.
    assert!(flow::ssrf::check_resolved_addrs(&[loopback], true, false).is_ok());
    assert!(flow::ssrf::check_resolved_addrs(&[public], true, false).is_ok());
    // Rebinding shape under the grant: any mixed set is refused as a whole.
    assert!(flow::ssrf::check_resolved_addrs(&[loopback, metadata], true, false).is_err());
    assert!(flow::ssrf::check_resolved_addrs(&[public, metadata], true, false).is_err());
    assert!(flow::ssrf::check_resolved_addrs(&[metadata], true, false).is_err());
    assert!(flow::ssrf::check_resolved_addrs(&[], true, false).is_err());
}

fn ssrf_args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// Read one HTTP request (headers plus any declared body) from a test
/// connection, mirroring what the helper's transport sends.
fn read_test_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::Read;
    use std::time::{Duration, Instant};

    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let read = stream.read(&mut chunk).unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .map(|at| at + 4)
        {
            let header = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let body_len = header
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= end + body_len || Instant::now() >= deadline {
                break;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    bytes
}

/// Serve one canned response on an owned loopback listener and report the raw
/// bytes the helper sent, so tests can prove which destination was contacted.
fn serve_once(
    listener: std::net::TcpListener,
    response: Vec<u8>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        listener.set_nonblocking(false).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let received = read_test_request(&mut stream);
        let _ = std::io::Write::write_all(&mut stream, &response);
        received
    })
}

fn http_ok(body: &[u8]) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes()
    .into_iter()
    .chain(body.iter().copied())
    .collect()
}

/// True when no peer connects within the window: the refusal happened before
/// any socket use.
fn saw_no_connection(listener: &std::net::TcpListener) -> bool {
    use std::time::{Duration, Instant};

    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok(_) => return false,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    true
}

fn openai_completed(text: &str) -> serde_json::Value {
    serde_json::json!({
        "model": "model-under-test",
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "output": [{
            "type": "message",
            "id": "msg-test",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }],
        "usage": {"input_tokens": 2, "output_tokens": 1}
    })
}

/// Live loopback transport requires the explicit grant; the hermetic fixture
/// path, an explicitly granted live request, and the redirect/proxy pins are
/// exercised end to end. All environment mutation stays inside this one test
/// so parallel tests never race on the shared process environment.
#[test]
fn model_live_loopback_requires_explicit_opt_in() {
    const OPT_IN: &str = "EXECUTE_MODEL_ALLOW_LOOPBACK";
    const PROXY_VARS: [&str; 6] = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ];

    let previous_opt_in = std::env::var_os(OPT_IN);
    let previous_proxies: Vec<(&str, Option<std::ffi::OsString>)> = PROXY_VARS
        .iter()
        .map(|name| (*name, std::env::var_os(name)))
        .collect();
    std::env::remove_var(OPT_IN);

    // Without the grant, a live loopback endpoint is refused by name before
    // credentials or sockets: the owned listener must never see a peer.
    let refused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let refused_port = refused.local_addr().unwrap().port();
    let args = ssrf_args(&[
        "classify",
        "--question",
        "which label fits",
        "--labels",
        "a",
        "b",
        "--endpoint",
        &format!("http://127.0.0.1:{refused_port}/v1"),
    ]);
    let mut input = &b""[..];
    let mut output = Vec::new();
    let error = flow::models::run(&args, &mut input, &mut output).unwrap_err();
    assert_eq!(flow::models::diagnostic(&error), "configuration");
    assert!(output.is_empty());
    assert!(
        saw_no_connection(&refused),
        "grantless loopback request opened a connection"
    );

    // Hermetic fixture runs need no grant: no socket is ever opened, and the
    // metadata endpoint stays refused even in fixture mode with the grant.
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("loopback.json");
    std::fs::write(
        &fixture,
        serde_json::json!({
            "expected_request": {
                "provider": "jev",
                "kind": "classify",
                "model": "jev-latest",
                "state": {"body": "need help"},
                "question": "Is this urgent?",
                "minimum": 0,
                "labels": ["urgent", "other"]
            },
            "response": {
                "model": "jev-latest",
                "answers": {"decision": {
                    "type": "choice",
                    "choice": "urgent",
                    "confidence": 0.79,
                    "probabilities": {"urgent": 0.79, "other": 0.21}
                }},
                "usage": {"input_tokens": 3, "output_tokens": 2}
            }
        })
        .to_string(),
    )
    .unwrap();
    let fixture_arg = fixture.to_string_lossy().into_owned();
    let args = ssrf_args(&[
        "classify",
        "--question",
        "Is this urgent?",
        "--labels",
        "urgent",
        "other",
        "--endpoint",
        &format!("http://127.0.0.1:{refused_port}/v1"),
        "--fixture",
        &fixture_arg,
    ]);
    let mut input = &br#"{"body":"need help"}"#[..];
    let mut output = Vec::new();
    flow::models::run(&args, &mut input, &mut output).unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["transport"], "fixture");
    assert_eq!(result["label"], "urgent");

    std::env::set_var(OPT_IN, "1");
    let args = ssrf_args(&[
        "classify",
        "--question",
        "which label fits",
        "--labels",
        "a",
        "b",
        "--endpoint",
        "https://169.254.169.254/",
        "--fixture",
        &fixture_arg,
    ]);
    let mut input = &br#"{"body":"need help"}"#[..];
    let mut output = Vec::new();
    let error = flow::models::run(&args, &mut input, &mut output).unwrap_err();
    assert_eq!(
        flow::models::diagnostic(&error),
        "configuration",
        "grant must not open metadata"
    );
    assert!(output.is_empty());

    // With the grant, a live loopback request reaches the owned server and
    // reports the loopback transport. No key is attached: the destination is
    // an unauthenticated local service, reachable only by explicit opt-in.
    let endpoint_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint_port = endpoint_listener.local_addr().unwrap().port();
    let body = openai_completed("served").to_string().into_bytes();
    let served = serve_once(endpoint_listener, http_ok(&body));
    let args = ssrf_args(&[
        "generate",
        "--provider",
        "openai",
        "--model",
        "model-under-test",
        "--instruction",
        "return a short result",
        "--endpoint",
        &format!("http://127.0.0.1:{endpoint_port}/v1/responses"),
    ]);
    let mut input = &b"source text"[..];
    let mut output = Vec::new();
    flow::models::run(&args, &mut input, &mut output).unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["transport"], "http_loopback");
    assert_eq!(result["text"], "served");
    let received = String::from_utf8_lossy(&served.join().unwrap()).to_ascii_lowercase();
    assert!(received.starts_with("post /v1/responses "));
    assert!(
        !received.contains("authorization:"),
        "keyless loopback request carried credentials"
    );

    // Redirects are never followed, even under the grant: the 302 surfaces as
    // an http_status failure and the redirect target sees no connection.
    let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let destination_port = destination.local_addr().unwrap().port();
    let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let source_port = source.local_addr().unwrap().port();
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nContent-Length: 0\r\nLocation: http://127.0.0.1:{destination_port}/redirected\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    let source_hit = serve_once(source, redirect);
    let args = ssrf_args(&[
        "generate",
        "--provider",
        "openai",
        "--model",
        "model-under-test",
        "--instruction",
        "return a short result",
        "--endpoint",
        &format!("http://127.0.0.1:{source_port}/v1/responses"),
    ]);
    let mut input = &b"source text"[..];
    let mut output = Vec::new();
    let error = flow::models::run(&args, &mut input, &mut output).unwrap_err();
    assert_eq!(flow::models::diagnostic(&error), "http_status 302");
    assert!(output.is_empty());
    assert!(!source_hit.join().unwrap().is_empty());
    assert!(
        saw_no_connection(&destination),
        "helper followed a provider redirect"
    );

    // Proxy environment is ignored under the grant: the request reaches the
    // explicit endpoint, never the proxy listener.
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_port = proxy.local_addr().unwrap().port();
    let proxy_target = format!("http://127.0.0.1:{proxy_port}");
    for name in PROXY_VARS {
        std::env::set_var(name, &proxy_target);
    }
    let direct = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let direct_port = direct.local_addr().unwrap().port();
    let body = openai_completed("direct").to_string().into_bytes();
    let reached = serve_once(direct, http_ok(&body));
    let args = ssrf_args(&[
        "generate",
        "--provider",
        "openai",
        "--model",
        "model-under-test",
        "--instruction",
        "return a short result",
        "--endpoint",
        &format!("http://127.0.0.1:{direct_port}/v1/responses"),
    ]);
    let mut input = &b"source text"[..];
    let mut output = Vec::new();
    flow::models::run(&args, &mut input, &mut output).unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["text"], "direct");
    assert!(!reached.join().unwrap().is_empty());
    assert!(
        saw_no_connection(&proxy),
        "helper routed provider traffic through a proxy"
    );

    for (name, value) in &previous_proxies {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    match previous_opt_in {
        Some(value) => std::env::set_var(OPT_IN, value),
        None => std::env::remove_var(OPT_IN),
    }
}

/// An explicitly granted private origin reaches the helper's transport without
/// the coarse loopback opt-in (#448): the parent conveys one canonical origin
/// after authorizing it against `network_origins`, and the helper honors only
/// that exact origin. A grant for any other origin still refuses a private
/// destination. Only this test touches `EXECUTE_MODEL_GRANTED_ORIGIN`, and it
/// restores the previous value, so parallel tests never race on it.
#[test]
fn model_explicitly_granted_private_origin_reaches_owned_loopback() {
    const GRANT: &str = "EXECUTE_MODEL_GRANTED_ORIGIN";

    let previous = std::env::var_os(GRANT);
    std::env::remove_var(GRANT);
    let endpoint_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint_port = endpoint_listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{endpoint_port}");
    let granted_endpoint = format!("{origin}/v1/responses");

    // Without the grant the owned loopback destination is refused by name
    // before any credential read or connection.
    let args = ssrf_args(&[
        "generate",
        "--provider",
        "openai",
        "--model",
        "model-under-test",
        "--instruction",
        "return a short result",
        "--endpoint",
        &granted_endpoint,
    ]);
    let mut input = &b"source text"[..];
    let mut output = Vec::new();
    let error = flow::models::run(&args, &mut input, &mut output).unwrap_err();
    assert_eq!(flow::models::diagnostic(&error), "configuration");
    assert!(output.is_empty());

    // A grant naming a different origin does not open this destination.
    std::env::set_var(GRANT, "http://127.0.0.1:1");
    let mut output = Vec::new();
    let error = flow::models::run(&args, &mut input, &mut output).unwrap_err();
    assert_eq!(flow::models::diagnostic(&error), "configuration");
    assert!(output.is_empty());

    // The exact granted origin reaches the owned server with no key attached.
    std::env::set_var(GRANT, &origin);
    let body = openai_completed("granted").to_string().into_bytes();
    let served = serve_once(endpoint_listener, http_ok(&body));
    let mut output = Vec::new();
    flow::models::run(&args, &mut input, &mut output).unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["transport"], "http_loopback");
    assert_eq!(result["text"], "granted");
    let received = String::from_utf8_lossy(&served.join().unwrap()).to_ascii_lowercase();
    assert!(received.starts_with("post /v1/responses "));
    assert!(
        !received.contains("authorization:"),
        "granted loopback request carried credentials"
    );

    match previous {
        Some(value) => std::env::set_var(GRANT, value),
        None => std::env::remove_var(GRANT),
    }
}

// ---------------------------------------------------------------------------
// Model origin-policy integration (#448): a custom model endpoint is
// authorized against the same `network_origins`/data-flow grants as a remote
// origin, before any child spawns. Refusals name the origin, never the body.
// ---------------------------------------------------------------------------

/// Custom model endpoints without a network grant are refused before any
/// resolution or socket use, and a broad grant never preserves a non-public
/// destination. Unparsable endpoints and fragments fail closed.
#[test]
fn model_policy_denies_ungranted_custom_endpoints_before_any_effect() {
    let sensitivities = unknown_sensitivities();
    // No network grant at all.
    let registry = model_registry_with_grant(&[], false, &["process.exec"]);
    let error = authorize_model_endpoint(
        &registry,
        "llm.ask",
        "https://169.254.169.254/",
        &sensitivities,
        false,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("denies network access"),
        "{error:?}"
    );
    // Network without the connect capability is an invalid policy and fails
    // closed before any grant is consulted.
    let registry = model_registry_with_grant(&["https://169.254.169.254"], true, &["process.exec"]);
    let error = authorize_model_endpoint(
        &registry,
        "llm.ask",
        "https://169.254.169.254/",
        &sensitivities,
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("network.connect"), "{error:?}");
    // A listing for one origin does not open any other origin.
    let registry = model_registry_with_grant(
        &["https://169.254.169.254"],
        true,
        &["process.exec", "network.connect"],
    );
    let error = authorize_model_endpoint(
        &registry,
        "llm.ask",
        "https://example.test/",
        &sensitivities,
        false,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("denies the model endpoint origin"),
        "{error:?}"
    );
    // Malformed endpoints and fragments fail closed without a grant lookup.
    assert!(
        authorize_model_endpoint(&registry, "llm.ask", "not a url", &sensitivities, false).is_err()
    );
    let error = authorize_model_endpoint(
        &registry,
        "llm.ask",
        "https://169.254.169.254/#fragment",
        &sensitivities,
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("fragment"), "{error:?}");
    // A broad grant allows a public origin for metadata-policy checks, exactly
    // like a remote origin, but never preserves a non-public destination.
    let broad = model_registry_with_grant(&[], true, &["process.exec", "network.connect"]);
    assert!(authorize_model_endpoint(
        &broad,
        "llm.ask",
        "https://example.test/",
        &sensitivities,
        false
    )
    .unwrap()
    .is_none());
    for endpoint in [
        "https://10.0.0.1/",
        "https://169.254.169.254/",
        "http://127.0.0.1:11434/v1",
    ] {
        let error = authorize_model_endpoint(&broad, "llm.ask", endpoint, &sensitivities, false)
            .unwrap_err();
        assert!(
            error.to_string().contains("non-public"),
            "{endpoint}: {error:?}"
        );
    }
}

/// Only an explicitly listed origin preserves a non-public model destination;
/// the canonical origin returns to the spawn site so it can convey the grant
/// to the helper. Live loopback literals need a listing, while hermetic
/// fixture runs keep working without one.
#[test]
fn model_policy_preserves_exactly_the_listed_origin() {
    let sensitivities = unknown_sensitivities();
    let registry = model_registry_with_grant(
        &["https://169.254.169.254", "https://10.0.0.1"],
        true,
        &["process.exec", "network.connect"],
    );
    assert_eq!(
        authorize_model_endpoint(
            &registry,
            "llm.ask",
            "https://169.254.169.254/",
            &sensitivities,
            false
        )
        .unwrap(),
        Some("https://169.254.169.254".to_owned())
    );
    assert_eq!(
        authorize_model_endpoint(
            &registry,
            "llm.ask",
            "https://10.0.0.1/",
            &sensitivities,
            false
        )
        .unwrap(),
        Some("https://10.0.0.1".to_owned())
    );
    // An unlisted public origin stays refused even with other listings.
    let error = authorize_model_endpoint(
        &registry,
        "llm.ask",
        "https://example.test/",
        &sensitivities,
        false,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("denies the model endpoint origin"),
        "{error:?}"
    );
    // A live loopback literal needs an explicit listing too: the unlisted
    // origin is denied before the SSRF check is even reached.
    let error = authorize_model_endpoint(
        &registry,
        "llm.ask",
        "http://127.0.0.1:11434/v1",
        &sensitivities,
        false,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("denies the model endpoint origin"),
        "{error:?}"
    );
    // A hermetic fixture run never opens a socket, so the loopback literal
    // stays usable there and returns no grant to convey. A non-public
    // literal is still refused by name even in fixture mode.
    let manifest = registry.resolve("llm.ask").unwrap();
    let params = model_params(Some("http://127.0.0.1:11434/v1"), true);
    assert!(authorize_model_endpoint_params(
        &registry,
        manifest,
        &params,
        &BTreeMap::new(),
        &sensitivities
    )
    .unwrap()
    .is_none());
    let manifest = registry.resolve("llm.ask").unwrap();
    let params = model_params(Some("https://169.254.169.254/"), true);
    let error = authorize_model_endpoint_params(
        &registry,
        manifest,
        &params,
        &BTreeMap::new(),
        &sensitivities,
    )
    .unwrap_err();
    assert!(error.to_string().contains("non-public"), "{error:?}");
    // An explicitly listed loopback origin returns the grant for the helper,
    // and default endpoints plus other binaries need no check.
    let loopback_granted = model_registry_with_grant(
        &["http://127.0.0.1:11434"],
        true,
        &["process.exec", "network.connect"],
    );
    let manifest = loopback_granted.resolve("llm.ask").unwrap();
    let params = model_params(Some("http://127.0.0.1:11434/v1"), false);
    assert_eq!(
        authorize_model_endpoint_params(
            &loopback_granted,
            manifest,
            &params,
            &BTreeMap::new(),
            &sensitivities
        )
        .unwrap(),
        Some("http://127.0.0.1:11434".to_owned())
    );
    let manifest = loopback_granted.resolve("llm.ask").unwrap();
    assert!(authorize_model_endpoint_params(
        &loopback_granted,
        manifest,
        &model_params(None, false),
        &BTreeMap::new(),
        &sensitivities
    )
    .unwrap()
    .is_none());
    // A reference endpoint resolves through bindings; unbound names defer to
    // argument resolution instead of reaching the transport.
    let mut referenced = model_params(None, false);
    referenced.insert(
        "endpoint".to_owned(),
        AstValue::Reference("custom".to_owned()),
    );
    let manifest = loopback_granted.resolve("llm.ask").unwrap();
    assert_eq!(
        authorize_model_endpoint_params(
            &loopback_granted,
            manifest,
            &referenced,
            &BTreeMap::from([(
                "custom".to_owned(),
                AstValue::String("http://127.0.0.1:11434/v1".to_owned())
            )]),
            &sensitivities
        )
        .unwrap(),
        Some("http://127.0.0.1:11434".to_owned())
    );
    let manifest = loopback_granted.resolve("llm.ask").unwrap();
    assert!(authorize_model_endpoint_params(
        &loopback_granted,
        manifest,
        &referenced,
        &BTreeMap::new(),
        &sensitivities
    )
    .unwrap()
    .is_none());
}

/// The data-flow policy gates custom model sinks: stdin plus any explicit
/// credential travels to the origin, so a withheld sink denies the request
/// and a granted one allows it.
#[test]
fn model_policy_routes_custom_endpoints_through_data_flow() {
    let sensitivities = unknown_sensitivities();
    let registry = model_registry_with_grant(
        &["https://example.test"],
        true,
        &["process.exec", "network.connect"],
    );
    // Without a data-flow policy the listed sink is approved.
    assert_eq!(
        authorize_model_endpoint(
            &registry,
            "llm.ask",
            "https://example.test/",
            &sensitivities,
            false
        )
        .unwrap(),
        Some("https://example.test".to_owned())
    );
    // A data-flow policy that withholds the sink denies the exfiltration.
    let denied = DataFlowPolicy::from_json(
        &serde_json::to_vec(&serde_json::json!({"version": 1, "remote_sinks": {}})).unwrap(),
    )
    .unwrap();
    registry.set_host_state(denied);
    assert!(authorize_model_endpoint(
        &registry,
        "llm.ask",
        "https://example.test/",
        &sensitivities,
        false
    )
    .is_err());
    // Granting the sink for the call's labels allows it again.
    let allowed = DataFlowPolicy::from_json(
        &serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "remote_sinks": {"https://example.test": {"allow": ["unknown"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    registry.set_host_state(allowed);
    assert_eq!(
        authorize_model_endpoint(
            &registry,
            "llm.ask",
            "https://example.test/",
            &sensitivities,
            false
        )
        .unwrap(),
        Some("https://example.test".to_owned())
    );
}
