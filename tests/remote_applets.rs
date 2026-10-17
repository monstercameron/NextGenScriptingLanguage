#![cfg(feature = "remote")]
use flow::contracts::{
    AppletBackend, AppletContract, AppletPort, AppletPorts, EffectCertainty, Lifecycle,
    OutcomeKind, OutcomeSpec, OutputEncoding, OutputType, PortDelivery, RemoteAuthSource,
    RemoteEndpointConfig, Sensitivity, ValueSchema,
};
use flow::remote::{
    RemoteAppletDescriptor, RemoteAppletIdentity, RemoteBackpressure, RemoteClient,
    RemoteErrorKind, RemoteEventFormat, RemoteEventPort, RemoteIdempotencyContract, RemoteRequest,
    PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

struct Request {
    line: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    delay: Duration,
    drop_connection: bool,
}

impl Reply {
    fn json(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "application/vnd.execute.applet+json;version=1",
            body,
            delay: Duration::ZERO,
            drop_connection: false,
        }
    }
}

fn local_server(
    count: usize,
    mut respond: impl FnMut(Request, usize) -> Reply + Send + 'static,
) -> (
    String,
    Arc<AtomicUsize>,
    mpsc::Receiver<Request>,
    JoinHandle<()>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_thread = Arc::clone(&seen);
    let (sender, receiver) = mpsc::channel();
    let thread = thread::spawn(move || {
        for index in 0..count {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            seen_thread.fetch_add(1, Ordering::SeqCst);
            let _ = sender.send(Request {
                line: request.line.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
            });
            let reply = respond(request, index);
            if !reply.delay.is_zero() {
                thread::sleep(reply.delay);
            }
            if reply.drop_connection {
                continue;
            }
            let reason = if reply.status == 200 { "OK" } else { "Error" };
            let header = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                reason,
                reply.content_type,
                reply.body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&reply.body);
            let _ = stream.flush();
        }
    });
    (format!("http://{address}"), seen, receiver, thread)
}

/// An idle server. `ready` becomes true at the point a cancellation should land: once the
/// request is read when no headers are sent, and once the headers are flushed when they are.
fn idle_response_server(
    send_headers: bool,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<std::sync::atomic::AtomicBool>,
    JoinHandle<()>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_thread = Arc::clone(&seen);
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready_thread = Arc::clone(&ready);
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _request = read_request(&mut stream);
        seen_thread.fetch_add(1, Ordering::SeqCst);
        if send_headers {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.execute.applet+json;version=1\r\nContent-Length: 128\r\nConnection: keep-alive\r\n\r\n{",
                )
                .unwrap();
            stream.flush().unwrap();
        }
        ready_thread.store(true, Ordering::SeqCst);
        // Stay idle until cancellation drops the client's socket. This makes
        // both the header wait and response-body wait deterministic without a
        // sleeping fixture thread.
        let mut probe = [0_u8; 1];
        let _ = stream.read(&mut probe);
    });
    (format!("http://{address}"), seen, ready, thread)
}

/// Set `cancelled` once the idle server reports `ready`. The wait is bounded, so a server
/// that never gets there still lets the test finish and fail with its own assertion.
fn cancel_when_ready(
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    ready: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ready.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        cancelled.store(true, Ordering::SeqCst);
    })
}

fn read_request(stream: &mut TcpStream) -> Request {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut headers = BTreeMap::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.trim_end().split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Request {
        line: line.trim_end().to_owned(),
        headers,
        body,
    }
}

fn remote_config(url: String) -> RemoteEndpointConfig {
    let mut config = RemoteEndpointConfig::new(url);
    config.allow_loopback_http_fixture = true;
    config
}

fn schema(kind: OutputType) -> ValueSchema {
    ValueSchema {
        kind,
        nullable: false,
        enum_values: None,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
    }
}

fn port(kind: OutputType, delivery: PortDelivery, encoding: Option<OutputEncoding>) -> AppletPort {
    AppletPort {
        schema: schema(kind),
        delivery,
        sensitivity: Sensitivity::Unknown,
        pointer: None,
        encoding,
    }
}

fn contract() -> AppletContract {
    AppletContract {
        version: 1,
        backend: AppletBackend::InProcess,
        lifecycle: Lifecycle::OneShot,
        program: None,
        remote: None,
        environment: None,
        output: None,
        outcomes: vec![
            OutcomeSpec {
                status: 0,
                name: "success".into(),
                kind: OutcomeKind::Success,
                effect_certainty: Some(EffectCertainty::KnownResponse),
            },
            OutcomeSpec {
                status: 7,
                name: "absence".into(),
                kind: OutcomeKind::Absence,
                effect_certainty: Some(EffectCertainty::KnownResponse),
            },
        ],
        capabilities: Vec::new(),
        effects: Vec::new(),
        result_envelope: false,
        ports: Some(AppletPorts {
            inputs: BTreeMap::from([
                (
                    "name".into(),
                    port(OutputType::Text, PortDelivery::Single, None),
                ),
                (
                    "stdin".into(),
                    port(
                        OutputType::Bytes,
                        PortDelivery::Single,
                        Some(OutputEncoding::Raw),
                    ),
                ),
            ]),
            outputs: BTreeMap::from([
                (
                    "raw".into(),
                    port(OutputType::Bytes, PortDelivery::Single, None),
                ),
                (
                    "result".into(),
                    port(OutputType::Json, PortDelivery::Single, None),
                ),
            ]),
            events: BTreeMap::from([(
                "events".into(),
                port(OutputType::Json, PortDelivery::Events, None),
            )]),
        }),
    }
}

fn descriptor() -> RemoteAppletDescriptor {
    RemoteAppletDescriptor {
        protocol_version: PROTOCOL_VERSION,
        identity: RemoteAppletIdentity {
            name: "demo.echo".into(),
            version: "1.2.3".into(),
        },
        contract: contract(),
        events: vec![RemoteEventPort {
            port: "events".into(),
            format: RemoteEventFormat::Sse,
            max_event_bytes: 4096,
            resumable: true,
            reconnect: false,
            max_reconnects: 0,
            backpressure: RemoteBackpressure::Pull,
        }],
        idempotency: Some(RemoteIdempotencyContract {
            scope: "account:demo/operation:echo".into(),
            operations: BTreeSet::from(["echo".into()]),
            minimum_retention_seconds: 60,
        }),
    }
}

fn response(request_id: &str, status: i32, outputs: Value, error: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "protocol_version": 1,
        "request_id": request_id,
        "status": status,
        "outputs": outputs,
        "error": error,
    }))
    .unwrap()
}

fn cached_client(config: RemoteEndpointConfig, descriptor: RemoteAppletDescriptor) -> RemoteClient {
    let client = RemoteClient::new(config).unwrap();
    client.use_cached_descriptor(descriptor).unwrap();
    client
}

#[test]
fn pinned_descriptor_cache_and_static_compatibility_are_offline() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("descriptor.json");
    let bytes = serde_json::to_vec(&descriptor()).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    std::fs::write(&path, &bytes).unwrap();
    let (url, seen, _, server) = local_server(0, |_, _| unreachable!());
    let mut config = remote_config(url);
    config.descriptor_cache = Some("descriptor.json".into());
    config.descriptor_sha256 = Some(digest);
    let client = RemoteClient::new_with_credential_base(config, directory.path()).unwrap();
    let loaded = client.load_configured_descriptor_cache().unwrap().unwrap();
    assert_eq!(loaded, descriptor());
    let mut expected = contract();
    expected.backend = AppletBackend::Remote;
    expected.remote = Some(RemoteEndpointConfig::new("https://remote.example.invalid"));
    client.check_compatible("demo.echo", &expected).unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    server.join().unwrap();

    let (url, seen, _, server) = local_server(0, |_, _| unreachable!());
    let mut bad_pin = remote_config(url);
    bad_pin.descriptor_cache = Some("descriptor.json".into());
    bad_pin.descriptor_sha256 = Some("0".repeat(64));
    let client = RemoteClient::new_with_credential_base(bad_pin, directory.path()).unwrap();
    assert_eq!(
        client.load_configured_descriptor_cache().unwrap_err().kind,
        RemoteErrorKind::DescriptorCacheHashMismatch
    );
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    server.join().unwrap();
}

#[test]
fn remote_backend_substitutes_for_five_logical_interfaces_without_expanding_effects() {
    let applet_names = [
        "demo.clock",
        "demo.formatter",
        "demo.widget",
        "demo.button",
        "demo.sync",
    ];
    for name in applet_names {
        let local_contract = contract();
        let mut expected = local_contract.clone();
        expected.backend = AppletBackend::Remote;
        expected.remote = Some(RemoteEndpointConfig::new("https://remote.example.invalid"));

        // A network transport permission is supplied by the host's explicit
        // backend grant. It is not part of the applet's logical interface.
        let mut advertised = descriptor();
        advertised.identity.name = name.to_owned();
        advertised
            .contract
            .capabilities
            .push("network.connect".into());
        advertised.validate().unwrap();

        let client =
            RemoteClient::new(RemoteEndpointConfig::new("https://remote.example.invalid")).unwrap();
        client.use_cached_descriptor(advertised.clone()).unwrap();
        client.check_compatible(name, &expected).unwrap();

        let mut overclaim = advertised;
        overclaim
            .contract
            .capabilities
            .push("filesystem.write".into());
        overclaim.contract.effects.push("filesystem.write".into());
        overclaim.validate().unwrap();
        let client =
            RemoteClient::new(RemoteEndpointConfig::new("https://remote.example.invalid")).unwrap();
        client.use_cached_descriptor(overclaim).unwrap();
        assert_eq!(
            client.check_compatible(name, &expected).unwrap_err().kind,
            RemoteErrorKind::IncompatibleContract
        );
    }
}

#[test]
fn remote_failures_keep_structured_kind_and_effect_certainty() {
    let transport = flow::remote::RemoteError {
        kind: RemoteErrorKind::Transport,
        effect_certainty: EffectCertainty::Uncertain,
    };
    let value = flow::runtime::execution_result::error_value(&anyhow::Error::new(transport));
    let flow::ast::Value::Record(fields) = value else {
        panic!("expected structured execution error");
    };
    assert_eq!(
        fields.get("kind"),
        Some(&flow::ast::Value::String("transport_error".into()))
    );
    assert_eq!(
        fields.get("effect_certainty"),
        Some(&flow::ast::Value::String("uncertain".into()))
    );

    let protocol = flow::remote::RemoteError {
        kind: RemoteErrorKind::InvalidResponse,
        effect_certainty: EffectCertainty::KnownResponse,
    };
    let value = flow::runtime::execution_result::error_value(&anyhow::Error::new(protocol));
    let flow::ast::Value::Record(fields) = value else {
        panic!("expected structured execution error");
    };
    assert_eq!(
        fields.get("kind"),
        Some(&flow::ast::Value::String("contract_decode_error".into()))
    );
    assert_eq!(
        fields.get("effect_certainty"),
        Some(&flow::ast::Value::String("known_response".into()))
    );
}

#[test]
fn discovery_rejects_unknown_protocol_before_any_applet_effect() {
    let body = serde_json::to_vec(&json!({
        "protocol_version": 9,
        "identity": {"name":"demo.echo","version":"1"},
        "contract": contract(),
        "events": [],
        "idempotency": null
    }))
    .unwrap();
    let (url, seen, requests, server) = local_server(1, move |_, _| Reply::json(body.clone()));
    let client = RemoteClient::new(remote_config(url)).unwrap();
    let error = client.discover().unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::UnsupportedProtocolVersion(9));
    assert_eq!(error.effect_certainty, EffectCertainty::NotStarted);
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().line,
        "GET /.well-known/execute-applet/v1 HTTP/1.1"
    );
    server.join().unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[test]
fn resolver_helper_process_observer_counts_spawns_before_request_completion() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let helper = directory.path().join("resolver-helper.sh");
    std::fs::write(
        &helper,
        "#!/bin/sh\n[ \"$1\" = __remote_resolve ] || exit 2\nprintf '127.0.0.1:%s\\n' \"$3\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let body = serde_json::to_vec(&descriptor()).unwrap();
    let (ip_url, _, _, server) = local_server(1, move |_, _| Reply::json(body.clone()));
    let port = ip_url.rsplit(':').next().unwrap();
    let launches = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&launches);
    let client = RemoteClient::new(remote_config(format!("http://localhost:{port}")))
        .unwrap()
        .with_resolver_helper(&helper)
        .with_process_observer(Arc::new(move |count| {
            observed.fetch_add(count, Ordering::SeqCst);
        }));

    client.discover().unwrap();
    server.join().unwrap();
    assert_eq!(launches.load(Ordering::SeqCst), 1);

    let (ip_url, _, _, server) = local_server(0, |_, _| unreachable!());
    let port = ip_url.rsplit(':').next().unwrap();
    let missing_helper = directory.path().join("does-not-exist");
    let failed_launches = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&failed_launches);
    let client = RemoteClient::new(remote_config(format!("http://localhost:{port}")))
        .unwrap()
        .with_resolver_helper(missing_helper)
        .with_process_observer(Arc::new(move |count| {
            observed.fetch_add(count, Ordering::SeqCst);
        }));

    assert!(client.discover().is_err());
    server.join().unwrap();
    assert_eq!(failed_launches.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[test]
fn bearer_file_auth_and_wire_bytes_are_explicit_and_debug_redacted() {
    let directory = tempfile::tempdir().unwrap();
    let credential = directory.path().join("credential.txt");
    std::fs::write(&credential, b"sensitive-token-42\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let discovery_body = serde_json::to_vec(&descriptor()).unwrap();
    let (url, _, requests, server) = local_server(2, move |request, _| {
        if request.line.starts_with("GET ") {
            return Reply::json(discovery_body.clone());
        }
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(payload["protocol_version"], 1);
        assert_eq!(payload["operation"], "echo");
        assert_eq!(payload["arguments"]["name"], "sample");
        assert_eq!(payload["input"]["encoding"], "base64");
        assert_eq!(payload["input"]["data"], "AP8K");
        Reply::json(response(
            payload["request_id"].as_str().unwrap(),
            0,
            json!({
                "raw": {"encoding":"base64","data":"AP8K"},
                "result": {"encoding":"json","data":{"ok":true}}
            }),
            Value::Null,
        ))
    });
    let mut config = remote_config(url);
    config.auth = Some(RemoteAuthSource::BearerFile {
        path: "credential.txt".into(),
    });
    let client = RemoteClient::new_with_credential_base(config.clone(), directory.path()).unwrap();
    let desc = descriptor();
    assert_eq!(client.discover().unwrap(), desc);
    let mut request = RemoteRequest::new("echo");
    request.arguments.insert("name".into(), json!("sample"));
    request.set_input_bytes(&contract(), &[0, 255, 10]).unwrap();
    let result = client.invoke(&desc, &request).unwrap();
    assert_eq!(result.status, 0);
    assert_eq!(result.outcome(&contract()).kind, OutcomeKind::Success);
    assert_eq!(
        result.outputs["raw"].decode_bytes(16).unwrap().unwrap(),
        [0, 255, 10]
    );
    let discovered = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    let invoked = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        discovered.headers["authorization"],
        "Bearer sensitive-token-42"
    );
    assert_eq!(
        invoked.headers["authorization"],
        "Bearer sensitive-token-42"
    );
    assert!(!format!("{client:?}").contains("sensitive-token-42"));
    assert!(!format!("{config:?}").contains("credential.txt"));
    assert!(!format!("{request:?}").contains("sample"));
    assert!(!format!("{result:?}").contains("ok"));
    server.join().unwrap();
}

#[test]
fn bearer_env_reads_only_the_configured_name() {
    const VARIABLE: &str = "EXECUTE_REMOTE_APPLET_TEST_CREDENTIAL";
    let previous = std::env::var_os(VARIABLE);
    std::env::set_var(VARIABLE, "environment-token-73");
    let body = serde_json::to_vec(&descriptor()).unwrap();
    let (url, _, requests, server) = local_server(1, move |_, _| Reply::json(body.clone()));
    let mut config = remote_config(url);
    config.auth = Some(RemoteAuthSource::BearerEnv {
        name: VARIABLE.into(),
    });
    let client = RemoteClient::new(config).unwrap();
    client.discover().unwrap();
    let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer environment-token-73")
    );
    server.join().unwrap();
    if let Some(previous) = previous {
        std::env::set_var(VARIABLE, previous);
    } else {
        std::env::remove_var(VARIABLE);
    }
}

#[test]
fn explicit_remote_bearer_is_registered_for_redaction_before_send() {
    let variable = "EXECUTE_REMOTE_APPLET_REDACTOR_TEST_TOKEN";
    let token = "remote-redaction-sentinel-7A91";
    std::env::set_var(variable, token);
    let registry = flow::manifest::Registry::default();
    let redactor = flow::security::secret_redactor(&registry);
    let server_redactor = Arc::clone(&redactor);
    let server_token = token.to_owned();
    let (url, _, requests, server) = local_server(1, move |request, _| {
        assert_eq!(
            server_redactor.redact_text(&server_token),
            "[REDACTED]",
            "the explicit auth value must be registered before network send"
        );
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        Reply::json(response(
            body["request_id"].as_str().unwrap(),
            0,
            json!({}),
            Value::Null,
        ))
    });
    let mut config = remote_config(url);
    config.auth = Some(RemoteAuthSource::BearerEnv {
        name: variable.into(),
    });
    let client = cached_client(config, descriptor()).with_secret_redactor(redactor.clone());
    let result = client
        .invoke(&descriptor(), &RemoteRequest::new("echo"))
        .unwrap();
    assert_eq!(result.status, 0);
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .headers["authorization"],
        format!("Bearer {token}")
    );
    assert!(!format!("{client:?}").contains(token));
    assert!(!format!("{:?}", redactor).contains(token));
    std::env::remove_var(variable);
    server.join().unwrap();
}

#[test]
fn transport_uncertainty_http_status_and_idempotency_are_distinct() {
    let desc = descriptor();
    let request = RemoteRequest::new("echo");
    let (url, _, _, server) = local_server(1, |_, _| Reply {
        status: 502,
        content_type: "text/plain",
        body: b"upstream failure".to_vec(),
        delay: Duration::ZERO,
        drop_connection: false,
    });
    let client = cached_client(remote_config(url), desc.clone());
    let error = client.invoke(&desc, &request).unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::HttpStatus(502));
    assert_eq!(error.effect_certainty, EffectCertainty::Uncertain);
    server.join().unwrap();

    let (url, seen, _, server) = local_server(1, |_, _| Reply {
        status: 200,
        content_type: "application/vnd.execute.applet+json;version=1",
        body: Vec::new(),
        delay: Duration::ZERO,
        drop_connection: true,
    });
    let client = cached_client(remote_config(url), desc.clone());
    let error = client.invoke(&desc, &request).unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::Transport);
    assert_eq!(error.effect_certainty, EffectCertainty::Uncertain);
    server.join().unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1);

    let (url, seen, _, server) = local_server(0, |_, _| unreachable!());
    let client = cached_client(remote_config(url), desc.clone());
    let mut forbidden_retry = RemoteRequest::new("other");
    forbidden_retry.idempotency_key = Some("explicit-key".into());
    let error = client.invoke(&desc, &forbidden_retry).unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::IdempotencyNotDeclared);
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    server.join().unwrap();
}

#[test]
fn wrong_request_id_and_oversized_response_are_uncertain() {
    let desc = descriptor();
    let (url, _, _, server) = local_server(1, |_, _| {
        Reply::json(response("wrong-id", 0, json!({}), Value::Null))
    });
    let client = cached_client(remote_config(url), desc.clone());
    let error = client
        .invoke(&desc, &RemoteRequest::new("echo"))
        .unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::RequestIdMismatch);
    assert_eq!(error.effect_certainty, EffectCertainty::Uncertain);
    server.join().unwrap();

    let (url, seen, _, server) = local_server(0, |_, _| unreachable!());
    let mut config = remote_config(url);
    config.max_request_bytes = 128;
    let client = cached_client(config, desc.clone());
    let mut oversized = RemoteRequest::new("echo");
    oversized
        .arguments
        .insert("name".into(), json!("x".repeat(512)));
    let error = client.invoke(&desc, &oversized).unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::RequestTooLarge);
    assert_eq!(error.effect_certainty, EffectCertainty::NotStarted);
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    server.join().unwrap();

    let (url, _, _, server) = local_server(1, |_, _| Reply::json(vec![b' '; 256]));
    let mut config = remote_config(url);
    config.max_response_bytes = 128;
    let client = cached_client(config, desc.clone());
    let error = client
        .invoke(&desc, &RemoteRequest::new("echo"))
        .unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::ResponseTooLarge);
    assert_eq!(error.effect_certainty, EffectCertainty::Uncertain);
    server.join().unwrap();
}

#[test]
fn remote_application_status_remains_a_typed_contract_outcome() {
    let desc = descriptor();
    let (url, _, _, server) = local_server(1, |request, _| {
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        Reply::json(response(
            payload["request_id"].as_str().unwrap(),
            7,
            json!({}),
            json!({"code":"not_found","message":"sensitive detail","retryable":false}),
        ))
    });
    let client = cached_client(remote_config(url), desc.clone());
    let result = client.invoke(&desc, &RemoteRequest::new("echo")).unwrap();
    let outcome = result.outcome(&contract());
    assert_eq!(outcome.kind, OutcomeKind::Absence);
    assert_eq!(outcome.effect_certainty, EffectCertainty::KnownResponse);
    assert!(!format!("{result:?}").contains("sensitive detail"));
    server.join().unwrap();
}

#[test]
fn cancellation_and_timeouts_are_bounded_and_post_is_never_replayed() {
    let desc = descriptor();
    let (url, seen, _, server) = local_server(0, |_, _| unreachable!());
    let client = cached_client(remote_config(url), desc.clone());
    let cancelled = std::sync::atomic::AtomicBool::new(true);
    let error = client
        .invoke_cancellable(&desc, &RemoteRequest::new("echo"), &Arc::new(cancelled))
        .unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::Cancelled);
    assert_eq!(error.effect_certainty, EffectCertainty::NotStarted);
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    server.join().unwrap();

    let (url, seen, _, server) = local_server(1, |request, _| {
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        Reply {
            status: 200,
            content_type: "application/vnd.execute.applet+json;version=1",
            body: response(
                payload["request_id"].as_str().unwrap(),
                0,
                json!({}),
                Value::Null,
            ),
            delay: Duration::from_millis(300),
            drop_connection: false,
        }
    });
    let mut config = remote_config(url);
    config.timeout_ms = 100;
    let client = cached_client(config, desc.clone());
    let error = client
        .invoke(&desc, &RemoteRequest::new("echo"))
        .unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::Timeout);
    assert_eq!(error.effect_certainty, EffectCertainty::Uncertain);
    server.join().unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_interrupts_idle_response_headers_and_body() {
    let desc = descriptor();

    let (url, seen, ready, server) = idle_response_server(false);
    let client = RemoteClient::new(remote_config(url)).unwrap();
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let setter = cancel_when_ready(Arc::clone(&cancelled), ready);
    let started = std::time::Instant::now();
    let error = client.discover_cancellable(&cancelled).unwrap_err();
    let elapsed = started.elapsed();
    setter.join().unwrap();
    server.join().unwrap();
    assert_eq!(error.kind, RemoteErrorKind::Cancelled);
    assert_eq!(error.effect_certainty, EffectCertainty::NotStarted);
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    assert_eq!(seen.load(Ordering::SeqCst), 1);

    let (url, seen, ready, server) = idle_response_server(true);
    let client = cached_client(remote_config(url), desc.clone());
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let setter = cancel_when_ready(Arc::clone(&cancelled), ready);
    let started = std::time::Instant::now();
    let error = client
        .invoke_cancellable(&desc, &RemoteRequest::new("echo"), &cancelled)
        .unwrap_err();
    let elapsed = started.elapsed();
    setter.join().unwrap();
    server.join().unwrap();
    assert_eq!(error.kind, RemoteErrorKind::Cancelled);
    assert_eq!(error.effect_certainty, EffectCertainty::Uncertain);
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[test]
fn cancellation_kills_and_reaps_the_supervised_dns_helper() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let helper = directory.path().join("resolver-helper");
    let pid_path = directory.path().join("resolver.pid");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 30\n",
            pid_path.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut config = RemoteEndpointConfig::new("https://remote-dns.example.invalid");
    config.timeout_ms = 5_000;
    let client = RemoteClient::new(config)
        .unwrap()
        .with_resolver_helper(helper);
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_thread = Arc::clone(&cancelled);
    let pid_path_for_cancel = pid_path.clone();
    let started = std::time::Instant::now();
    let helper_started_ns = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let helper_started_ns_for_cancel = Arc::clone(&helper_started_ns);
    let cancel_requested_ns = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let cancel_requested_ns_for_cancel = Arc::clone(&cancel_requested_ns);
    let setter = thread::spawn(move || {
        let wait_started = std::time::Instant::now();
        while !pid_path_for_cancel.exists() && wait_started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(5));
        }
        if pid_path_for_cancel.exists() {
            helper_started_ns_for_cancel.store(
                started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                Ordering::SeqCst,
            );
            thread::sleep(Duration::from_millis(60));
        }
        cancel_requested_ns_for_cancel.store(
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::SeqCst,
        );
        cancel_thread.store(true, Ordering::SeqCst);
    });
    let error = client.discover_cancellable(&cancelled).unwrap_err();
    let elapsed = started.elapsed();
    setter.join().unwrap();
    let helper_started = Duration::from_nanos(helper_started_ns.load(Ordering::SeqCst));
    let cancel_requested = Duration::from_nanos(cancel_requested_ns.load(Ordering::SeqCst));
    assert!(
        !helper_started.is_zero(),
        "resolver helper did not create its bounded PID marker before cancellation: error={error:?}, elapsed={elapsed:?}, configured_timeout=5s"
    );
    assert_eq!(error.kind, RemoteErrorKind::Cancelled);
    assert_eq!(error.effect_certainty, EffectCertainty::NotStarted);
    let cancellation_latency = elapsed.saturating_sub(cancel_requested);
    assert!(
        cancellation_latency < Duration::from_millis(150),
        "cancellation after helper start took too long: helper_started={helper_started:?}, cancel_requested={cancel_requested:?}, total={elapsed:?}, cancellation_latency={cancellation_latency:?}, error={error:?}"
    );

    let pid = std::fs::read_to_string(pid_path)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    // SAFETY: signal 0 sends nothing and takes only an integer. `pid` is the resolver
    // helper's PID that it published to `pid_path` (parsed just above) for this test.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn event_stream_is_pull_driven_and_surfaces_duplicate_cursors() {
    let (url, _, requests, server) = local_server(1, |_, _| {
        Reply {
        status: 200,
        content_type: "text/event-stream; charset=utf-8",
        body: b"id: cursor-1\ndata: {\"event\":\"created\"}\n\nid: cursor-1\ndata: {\"event\":\"created\"}\n\n".to_vec(),
        delay: Duration::ZERO,
        drop_connection: false,
    }
    });
    let client = RemoteClient::new(remote_config(url)).unwrap();
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut stream = client
        .open_events(&descriptor(), "events", None, cancelled)
        .unwrap();
    let first = stream.next_event().unwrap().unwrap();
    let second = stream.next_event().unwrap().unwrap();
    assert_eq!(first.cursor.as_deref(), Some("cursor-1"));
    assert!(!first.duplicate_cursor);
    assert!(second.duplicate_cursor);
    assert_eq!(second.data, json!({"event":"created"}));
    let observed = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(observed.line, "GET /events/events HTTP/1.1");
    server.join().unwrap();
}

#[test]
fn opt_in_reconnect_resumes_with_last_event_id_and_preserves_at_least_once_delivery() {
    let mut desc = descriptor();
    desc.events[0].reconnect = true;
    desc.events[0].max_reconnects = 1;
    let (url, seen, requests, server) = local_server(2, |_, _| Reply {
        status: 200,
        content_type: "text/event-stream",
        body: b"id: cursor-1\ndata: {\"event\":\"created\"}\n\n".to_vec(),
        delay: Duration::ZERO,
        drop_connection: false,
    });
    let client = RemoteClient::new(remote_config(url)).unwrap();
    let mut stream = client
        .open_events(
            &desc,
            "events",
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
    assert!(!stream.next_event().unwrap().unwrap().duplicate_cursor);
    let resumed = stream.next_event().unwrap().unwrap();
    assert!(resumed.duplicate_cursor);
    let first = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    let second = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(first.line, "GET /events/events HTTP/1.1");
    assert_eq!(
        second.headers.get("last-event-id").map(String::as_str),
        Some("cursor-1")
    );
    server.join().unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 2);
}

#[test]
fn json_lines_events_are_typed_and_frame_bounded() {
    let mut desc = descriptor();
    desc.events[0].format = RemoteEventFormat::JsonLines;
    desc.events[0].max_event_bytes = 256;
    let (url, _, requests, server) = local_server(1, |_, _| Reply {
        status: 200,
        content_type: "application/x-ndjson",
        body: b"{\"cursor\":\"json-1\",\"event\":\"changed\",\"data\":{\"ok\":true}}\n".to_vec(),
        delay: Duration::ZERO,
        drop_connection: false,
    });
    let client = RemoteClient::new(remote_config(url)).unwrap();
    let mut stream = client
        .open_events(
            &desc,
            "events",
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
    let event = stream.next_event().unwrap().unwrap();
    assert_eq!(event.event, "changed");
    assert_eq!(event.cursor.as_deref(), Some("json-1"));
    assert_eq!(event.data, json!({"ok": true}));
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().line,
        "GET /events/events HTTP/1.1"
    );
    server.join().unwrap();

    desc.events[0].max_event_bytes = 64;
    let (url, _, _, server) = local_server(1, |_, _| Reply {
        status: 200,
        content_type: "application/x-ndjson",
        body: format!(
            "{{\"cursor\":\"json-2\",\"event\":\"changed\",\"data\":{{\"value\":\"{}\"}}}}\n",
            "x".repeat(128)
        )
        .into_bytes(),
        delay: Duration::ZERO,
        drop_connection: false,
    });
    let client = RemoteClient::new(remote_config(url)).unwrap();
    let mut stream = client
        .open_events(
            &desc,
            "events",
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
    assert_eq!(
        stream.next_event().unwrap_err().kind,
        RemoteErrorKind::StreamTooLarge
    );
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn bearer_symlink_and_non_loopback_http_are_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let directory = tempfile::tempdir().unwrap();
    let secret = directory.path().join("secret");
    let link = directory.path().join("link");
    std::fs::write(&secret, b"token\n").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&secret, &link).unwrap();

    let (url, seen, _, server) = local_server(0, |_, _| unreachable!());
    let mut config = remote_config(url);
    config.auth = Some(RemoteAuthSource::BearerFile { path: link });
    let client = RemoteClient::new_with_credential_base(config, directory.path()).unwrap();
    let error = client.discover().unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::CredentialUnavailable);
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    server.join().unwrap();

    let mut config = RemoteEndpointConfig::new("http://example.com");
    config.allow_loopback_http_fixture = true;
    assert_eq!(
        RemoteClient::new(config).unwrap_err().kind,
        RemoteErrorKind::InvalidConfiguration
    );
}

#[cfg(unix)]
#[test]
fn bearer_file_requires_owner_private_regular_file_and_resolves_relative_to_base() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let directory = tempfile::tempdir().unwrap();
    let credential = directory.path().join("credential.txt");
    std::fs::write(&credential, b"private-token\n").unwrap();
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();

    let (url, seen, _, server) = local_server(0, |_, _| unreachable!());
    let mut config = remote_config(url);
    config.auth = Some(RemoteAuthSource::BearerFile {
        path: "credential.txt".into(),
    });
    let client = RemoteClient::new_with_credential_base(config.clone(), directory.path()).unwrap();
    assert_eq!(client.bearer_file_path().unwrap(), credential);
    let metadata = std::fs::metadata(&credential).unwrap();
    // SAFETY: geteuid takes no arguments, cannot fail, and only reports the calling process's
    // effective uid; no memory is touched.
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    server.join().unwrap();

    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o644)).unwrap();
    let error = RemoteClient::new_with_credential_base(config.clone(), directory.path())
        .unwrap()
        .discover()
        .unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::CredentialInvalid);

    let fifo = directory.path().join("credential.fifo");
    let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `fifo_name` is a CString built just above from the path, so it is NUL-terminated
    // and outlives the call. mkfifo only reads the path, and the fifo path is new in this
    // fresh tempdir.
    assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
    config.auth = Some(RemoteAuthSource::BearerFile { path: fifo });
    let error = RemoteClient::new_with_credential_base(config, directory.path())
        .unwrap()
        .discover()
        .unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::CredentialInvalid);
    assert_eq!(seen.load(Ordering::SeqCst), 0);
}

#[cfg(windows)]
#[test]
fn bearer_file_is_rejected_until_existing_file_dacl_can_be_verified() {
    let directory = tempfile::tempdir().unwrap();
    let credential = directory.path().join("credential.txt");
    std::fs::write(&credential, b"private-token\n").unwrap();
    let (url, seen, _, server) = local_server(0, |_, _| unreachable!());
    let mut config = remote_config(url);
    config.auth = Some(RemoteAuthSource::BearerFile { path: credential });
    let error = RemoteClient::new(config).unwrap().discover().unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::CredentialInvalid);
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    server.join().unwrap();
}
