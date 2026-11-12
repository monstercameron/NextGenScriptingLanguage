#![cfg(feature = "remote")]
use flow::contracts::{
    AppletBackend, AppletContract, AppletPort, AppletPorts, EffectCertainty, Lifecycle,
    OutcomeKind, OutcomeSpec, OutputType, PortDelivery, RemoteEndpointConfig, Sensitivity,
    ValueSchema,
};
use flow::remote::{
    RemoteAppletDescriptor, RemoteAppletIdentity, RemoteClient, RemoteErrorKind,
    RemoteIdempotencyContract, RemoteRequest, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

/// Accept `count` requests, record each request body, and answer each with a
/// success response that echoes the request's own `request_id`.
fn recording_server(count: usize) -> (String, Arc<AtomicUsize>, mpsc::Receiver<Value>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_thread = Arc::clone(&seen);
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for _ in 0..count {
            let (mut stream, _) = listener.accept().unwrap();
            let body = read_body(&mut stream);
            seen_thread.fetch_add(1, Ordering::SeqCst);
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            let request_id = parsed["request_id"].as_str().unwrap().to_owned();
            let _ = sender.send(parsed);
            let reply = serde_json::to_vec(&json!({
                "protocol_version": 1,
                "request_id": request_id,
                "status": 0,
                "outputs": {},
                "error": Value::Null,
            }))
            .unwrap();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.execute.applet+json;version=1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&reply);
            let _ = stream.flush();
        }
    });
    (format!("http://{address}"), seen, receiver)
}

fn read_body(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.trim_end().split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    body
}

fn remote_config(url: String) -> RemoteEndpointConfig {
    let mut config = RemoteEndpointConfig::new(url);
    config.allow_loopback_http_fixture = true;
    config
}

fn schema() -> ValueSchema {
    ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Text,
        nullable: false,
        enum_values: None,
        single_line: false,
        minimum: None,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        pattern: None,
    }
}

fn port() -> AppletPort {
    AppletPort {
        schema: schema(),
        delivery: PortDelivery::Single,
        sensitivity: Sensitivity::Unknown,
        pointer: None,
        encoding: None,
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
        outcomes: vec![OutcomeSpec {
            status: 0,
            name: "success".into(),
            kind: OutcomeKind::Success,
            effect_certainty: Some(EffectCertainty::KnownResponse),
        }],
        capabilities: Vec::new(),
        effects: Vec::new(),
        result_envelope: false,
        deprecated: false,
        replaced_by: None,
        reversibility: Default::default(),
        reconciler: None,
        health: Vec::new(),
        recovery: None,
        ports: Some(AppletPorts {
            inputs: BTreeMap::from([("name".into(), port())]),
            outputs: BTreeMap::from([("result".into(), port())]),
            events: BTreeMap::new(),
        }),
    }
}

fn descriptor() -> RemoteAppletDescriptor {
    RemoteAppletDescriptor {
        protocol_version: PROTOCOL_VERSION,
        identity: RemoteAppletIdentity {
            name: "demo.ledger".into(),
            version: "1.0.0".into(),
        },
        contract: contract(),
        events: Vec::new(),
        idempotency: Some(RemoteIdempotencyContract {
            scope: "account:demo/operation:apply".into(),
            operations: BTreeSet::from(["apply".into()]),
            minimum_retention_seconds: 60,
        }),
    }
}

fn cached_client(url: String, descriptor: RemoteAppletDescriptor) -> RemoteClient {
    let client = RemoteClient::new(remote_config(url)).unwrap();
    client.use_cached_descriptor(descriptor).unwrap();
    client
}

#[test]
fn retried_request_for_the_same_node_carries_the_same_key_on_the_wire() {
    let desc = descriptor();
    let key = "0123456789abcdef0123456789abcdef";
    // One logical request, sent twice: the second send models a retry after an
    // uncertain first send. The key is the caller's stable per-node key.
    let request = RemoteRequest::new("apply")
        .with_idempotency_key(key)
        .unwrap();
    let (url, seen, bodies) = recording_server(2);
    let client = cached_client(url, desc.clone());
    client.invoke(&desc, &request).unwrap();
    client.invoke(&desc, &request).unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 2);
    let first = bodies.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = bodies.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first["idempotency_key"], json!(key));
    assert_eq!(second["idempotency_key"], json!(key));
}

#[test]
fn supplied_key_is_sent_and_an_absent_key_sends_no_field() {
    let desc = descriptor();
    let (url, seen, bodies) = recording_server(1);
    let client = cached_client(url, desc.clone());
    let fresh = RemoteRequest::new("apply")
        .with_idempotency_key("fedcba9876543210fedcba9876543210")
        .unwrap();
    client.invoke(&desc, &fresh).unwrap();
    let body = bodies.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        body["idempotency_key"],
        json!("fedcba9876543210fedcba9876543210")
    );
    assert_eq!(seen.load(Ordering::SeqCst), 1);

    // A request without a supplied key carries no key field at all.
    let (url, _, bodies) = recording_server(1);
    let client = cached_client(url, desc.clone());
    client.invoke(&desc, &RemoteRequest::new("apply")).unwrap();
    let body = bodies.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(body.get("idempotency_key").is_none());
}

#[test]
fn invalid_key_is_rejected_at_construction() {
    let error = RemoteRequest::new("apply")
        .with_idempotency_key("has a space and secret: hunter2")
        .unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::InvalidRequest);
    let error = RemoteRequest::new("apply")
        .with_idempotency_key("")
        .unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::InvalidRequest);
    let error = RemoteRequest::new("apply")
        .with_idempotency_key("k".repeat(257))
        .unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::InvalidRequest);
}

#[test]
fn key_for_an_undeclared_operation_is_refused_without_a_request() {
    let desc = descriptor();
    let (url, seen, _) = recording_server(0);
    let client = cached_client(url, desc.clone());
    let request = RemoteRequest::new("read")
        .with_idempotency_key("0123456789abcdef")
        .unwrap();
    let error = client.invoke(&desc, &request).unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::IdempotencyNotDeclared);
    assert_eq!(seen.load(Ordering::SeqCst), 0);
}
