//! #323: the in-process `http.get` request is authorized by the capability
//! policy before any connection, redirects are re-authorized per hop, and the
//! body is bounded by the shared response limit. Every test drives a loopback
//! fixture server through the `allow_loopback_http_fixture` switch.

#![cfg(all(feature = "remote", not(target_arch = "wasm32")))]

use flow::capabilities::{authorize_http_origin, AppletGrant, CapabilityPolicy, FilesystemGrant};
use flow::contracts::{EffectCertainty, RemoteError, RemoteErrorKind};
use flow::manifest::Registry;
use flow::remote::{http_get, HttpGetOptions, HttpResponse};
use flow::security::SensitivitySet;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const APPLET: &str = "http.get";

fn network_grant(origins: &[String]) -> AppletGrant {
    AppletGrant {
        capabilities: BTreeSet::from(["network.connect".to_owned()]),
        binary_sha256: None,
        environment: BTreeSet::new(),
        filesystem: FilesystemGrant::default(),
        network: true,
        network_origins: origins.iter().cloned().collect(),
        sandbox_required: false,
        resource_limits: None,
    }
}

fn install_policy(registry: &Registry, origins: &[String]) {
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([(APPLET.into(), network_grant(origins))]),
    }));
}

/// The capability path as the applet dispatch will use it: each URL is
/// authorized before its origin is contacted, and the result says whether the
/// origin is explicitly listed.
fn authorizer(
    registry: &Registry,
    fixture: bool,
) -> impl Fn(&str) -> Result<bool, RemoteError> + '_ {
    move |url: &str| {
        authorize_http_origin(registry, APPLET, url, fixture, &SensitivitySet::default()).map_err(
            |_| RemoteError {
                kind: RemoteErrorKind::InvalidConfiguration,
                effect_certainty: EffectCertainty::NotStarted,
            },
        )
    }
}

fn origin(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// A loopback HTTP/1.1 fixture. `handler` maps the request path to a complete
/// raw response. The returned counter holds the number of accepted
/// connections, so a test can prove that nothing connected.
fn serve<F>(handler: F) -> (SocketAddr, Arc<AtomicUsize>)
where
    F: Fn(&str) -> String + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                }
            }
            let path = String::from_utf8_lossy(&request)
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_owned();
            let _ = stream.write_all(handler(&path).as_bytes());
        }
    });
    (address, accepted)
}

fn ok_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A refused request must never reach the socket. The accept loop runs on
/// its own thread, so give it a moment before asserting that nothing arrived.
fn settle() {
    thread::sleep(Duration::from_millis(200));
}

fn options(fixture: bool) -> HttpGetOptions {
    HttpGetOptions {
        allow_loopback_http_fixture: fixture,
        ..HttpGetOptions::default()
    }
}

#[test]
fn granted_get_returns_status_headers_and_body() {
    let (address, accepted) = serve(|_| ok_response("hello"));
    let registry = Registry::default();
    install_policy(&registry, &[origin(address.port())]);
    let url = format!("{}/data", origin(address.port()));
    let response: HttpResponse =
        http_get(&url, &options(true), &authorizer(&registry, true)).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello");
    assert!(response
        .headers
        .iter()
        .any(|(name, value)| name == "content-type" && value == "text/plain"));
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}

#[test]
fn ungranted_origin_is_refused_before_any_connection() {
    let (address, accepted) = serve(|_| ok_response("secret"));
    let registry = Registry::default();
    // The policy grants a different origin; this one is not listed.
    install_policy(&registry, &[origin(address.port().wrapping_add(1))]);
    let url = format!("{}/data", origin(address.port()));
    let result = http_get(&url, &options(true), &authorizer(&registry, true));
    assert!(result.is_err());
    settle();
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
}

#[test]
fn cross_origin_redirect_is_refused_before_the_second_connection() {
    let (target, target_accepted) = serve(|_| ok_response("leaked"));
    let target_url = format!("{}/landing", origin(target.port()));
    let (first, first_accepted) = serve(move |_| {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    });
    let registry = Registry::default();
    // Only the first hop's origin is granted.
    install_policy(&registry, &[origin(first.port())]);
    let url = format!("{}/start", origin(first.port()));
    let result = http_get(&url, &options(true), &authorizer(&registry, true));
    assert!(result.is_err());
    settle();
    assert_eq!(first_accepted.load(Ordering::SeqCst), 1);
    assert_eq!(target_accepted.load(Ordering::SeqCst), 0);
}

#[test]
fn same_origin_redirect_is_followed_and_authorized_per_hop() {
    let (address, accepted) = serve(|path| {
        if path == "/start" {
            "HTTP/1.1 301 Moved Permanently\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
        } else {
            ok_response("done")
        }
    });
    let registry = Registry::default();
    install_policy(&registry, &[origin(address.port())]);
    let url = format!("{}/start", origin(address.port()));
    let response = http_get(&url, &options(true), &authorizer(&registry, true)).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"done");
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
}

#[test]
fn body_over_the_limit_is_refused() {
    let (address, _accepted) = serve(|_| ok_response(&"x".repeat(64)));
    let registry = Registry::default();
    install_policy(&registry, &[origin(address.port())]);
    let url = format!("{}/big", origin(address.port()));
    let limited = HttpGetOptions {
        max_bytes: 16,
        ..options(true)
    };
    let error = http_get(&url, &limited, &authorizer(&registry, true)).unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::ResponseTooLarge);
}

#[test]
fn plain_http_is_refused_without_the_fixture_switch() {
    let (address, accepted) = serve(|_| ok_response("never"));
    let registry = Registry::default();
    install_policy(&registry, &[origin(address.port())]);
    let url = format!("{}/data", origin(address.port()));
    let result = http_get(&url, &options(false), &authorizer(&registry, false));
    assert!(result.is_err());
    settle();
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
}
