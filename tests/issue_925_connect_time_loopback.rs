//! #925 connect-time half: a remote applet's manifest `allow_loopback_http_fixture`
//! flag must not reach the transport's loopback switch under a capability policy
//! unless the policy lists the endpoint origin. The fixture server counts every
//! accepted connection, so "the server saw nothing" is the observable refusal.
#![cfg(all(feature = "remote", feature = "graph"))]

use flow::capabilities::{AppletGrant, CapabilityPolicy, FilesystemGrant};
use flow::graph::{self, GraphRunOptions, GraphSession, RuntimeGraphExecutor};
use flow::manifest::Registry;
use flow::runtime::{self, RunOptions};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Serves every accepted connection with HTTP 500 until `stop` is set, counting
/// connections. Returns the endpoint, the counter, the stop flag and the worker.
fn fixture_server() -> (String, Arc<AtomicUsize>, Arc<AtomicBool>, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (seen_worker, stop_worker) = (Arc::clone(&seen), Arc::clone(&stop));
    let worker = thread::spawn(move || {
        while !stop_worker.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    seen_worker.fetch_add(1, Ordering::SeqCst);
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut buffer = [0u8; 4096];
                    let _ = stream.read(&mut buffer);
                    let _ = stream.write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (endpoint, seen, stop, worker)
}

fn stop_server(stop: &AtomicBool, worker: JoinHandle<()>) {
    stop.store(true, Ordering::SeqCst);
    worker.join().unwrap();
}

fn remote_registry(endpoint: &str) -> Registry {
    let source = json!({
        "name": "remote.fixture",
        "binary": "@remote-applet",
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "remote",
            "remote": {
                "endpoint": endpoint,
                "allow_loopback_http_fixture": true,
                "allow_discovery": true
            },
            "capabilities": []
        }
    });
    Registry::from_sources(&[source.to_string()]).unwrap()
}

fn graph_registry(endpoint: &str) -> Registry {
    let contract = json!({
        "version": 1,
        "backend": "remote",
        "remote": {
            "endpoint": endpoint,
            "allow_loopback_http_fixture": true,
            "allow_discovery": true
        },
        "ports": {
            "events": {
                "updates": {
                    "schema": {"type": "text"},
                    "delivery": "events"
                }
            }
        }
    });
    let source = json!({
        "name": "remote.feed",
        "binary": "@remote-applet",
        "parameters": {},
        "arguments": [],
        "contract": contract
    });
    Registry::from_sources(&[source.to_string()]).unwrap()
}

fn install_policy(registry: &Registry, name: &str, origins: &[&str]) {
    let grant = AppletGrant {
        capabilities: BTreeSet::from(["network.connect".to_owned()]),
        binary_sha256: None,
        environment: BTreeSet::new(),
        filesystem: FilesystemGrant::default(),
        network: true,
        network_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        sandbox_required: false,
        resource_limits: None,
    };
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([(name.to_owned(), grant)]),
    }));
}

#[test]
fn policy_without_listed_origin_never_connects_to_fixture_loopback() {
    let (endpoint, seen, stop, worker) = fixture_server();
    let registry = remote_registry(&endpoint);
    install_policy(&registry, "remote.fixture", &[]);
    let directory = tempfile::tempdir().unwrap();
    let program = flow::parser::parse("run remote.fixture\n").unwrap();

    let result = runtime::run(
        &program,
        &registry,
        &RunOptions::new(directory.path().to_path_buf()),
    );
    stop_server(&stop, worker);

    assert!(
        result.is_err(),
        "an unlisted loopback origin must be refused"
    );
    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "no connection may reach the fixture"
    );
}

#[test]
fn policy_listing_the_loopback_origin_connects_to_fixture() {
    let (endpoint, seen, stop, worker) = fixture_server();
    let registry = remote_registry(&endpoint);
    install_policy(&registry, "remote.fixture", &[&endpoint]);
    let directory = tempfile::tempdir().unwrap();
    let program = flow::parser::parse("run remote.fixture\n").unwrap();

    // The fixture answers 500, so the run fails after the request; the point is
    // that the request reached the fixture at all.
    let result = runtime::run(
        &program,
        &registry,
        &RunOptions::new(directory.path().to_path_buf()),
    );
    stop_server(&stop, worker);

    assert!(
        seen.load(Ordering::SeqCst) >= 1,
        "a listed loopback origin must connect; run result: {:?}",
        result.err().map(|error| format!("{error:#}"))
    );
}

#[test]
fn without_policy_fixture_flag_still_connects() {
    let (endpoint, seen, stop, worker) = fixture_server();
    let registry = remote_registry(&endpoint);
    let directory = tempfile::tempdir().unwrap();
    let program = flow::parser::parse("run remote.fixture\n").unwrap();

    let result = runtime::run(
        &program,
        &registry,
        &RunOptions::new(directory.path().to_path_buf()),
    );
    stop_server(&stop, worker);

    assert!(
        seen.load(Ordering::SeqCst) >= 1,
        "with no policy the manifest fixture flag keeps working; run result: {:?}",
        result.err().map(|error| format!("{error:#}"))
    );
}

#[test]
fn graph_listener_under_policy_without_listed_origin_never_connects() {
    let (endpoint, seen, stop, worker) = fixture_server();
    let registry = graph_registry(&endpoint);
    install_policy(&registry, "remote.feed", &[]);
    let directory = tempfile::tempdir().unwrap();
    let plan = graph::parse(
        "graph remote_events_925\nlisten to remote.feed as source\nend graph\n",
        &registry,
    )
    .unwrap();
    let executor = Arc::new(RuntimeGraphExecutor::new(
        registry.clone(),
        RunOptions::new(directory.path().to_path_buf()),
    ));

    // The listener may fail or stop early; either way nothing may reach the fixture.
    let _ = GraphSession::start(plan, executor, GraphRunOptions::default())
        .and_then(|session| session.run_to_completion());
    stop_server(&stop, worker);

    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "no graph connection may reach the fixture"
    );
}
