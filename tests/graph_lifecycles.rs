#![cfg(feature = "graph")]
use anyhow::{anyhow, Result};
use flow::ast::Value;
use flow::contracts::{OutputType, ValueSchema};
#[cfg(any(unix, feature = "remote"))]
use flow::graph::{self, GraphRunOptions, GraphSession};
#[cfg(feature = "remote")]
use flow::graph::{EdgeSpec, GraphNodeKind, PortRef, PortSchema};
use flow::graph::{
    GraphContext, GraphExecutor, GraphNode, NodeIo, PortValue, RuntimeGraphExecutor,
};
use flow::manifest::Registry;
#[cfg(any(unix, feature = "remote"))]
use flow::runtime::RunOptions;
use serde_json::json;
use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
#[cfg(any(unix, feature = "remote"))]
use std::time::{Duration, Instant};

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn text_schema() -> ValueSchema {
    ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Text,
        nullable: false,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: None,
        single_line: false,
        minimum: None,
        pattern: None,
    }
}

#[cfg(any(unix, feature = "remote"))]
fn options() -> RunOptions {
    RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        ..RunOptions::new(std::env::current_dir().unwrap())
    }
}

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn fixture_server() -> (Registry, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    fixture_server_at_host("127.0.0.1")
}

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn fixture_server_at_host(
    endpoint_host: &str,
) -> (Registry, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let endpoint = format!(
        "http://{endpoint_host}:{}",
        listener.local_addr().unwrap().port()
    );
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
    let descriptor = json!({
        "protocol_version": 1,
        "identity": {"name": "remote.feed", "version": "1"},
        "contract": contract,
        "events": [{
            "port": "updates",
            "format": "sse",
            "max_event_bytes": 4096,
            "resumable": true,
            "backpressure": "pull"
        }]
    });
    let descriptor = serde_json::to_vec(&descriptor).unwrap();
    let manifest = json!({
        "name": "remote.feed",
        "binary": "@remote-applet",
        "parameters": {},
        "arguments": [],
        "contract": contract
    });
    let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
    let (requests, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first_line = String::new();
            reader.read_line(&mut first_line).unwrap();
            let _ = requests.send(first_line.trim_end().to_owned());
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            let (content_type, body) = if first_line.contains("/.well-known/") {
                (
                    "application/vnd.execute.applet+json;version=1",
                    descriptor.as_slice(),
                )
            } else {
                (
                    "text/event-stream",
                    &b"id: cursor-1\nevent: changed\ndata: \"first\"\n\nid: cursor-1\nevent: changed\ndata: \"second\"\n\n"[..],
                )
            };
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        }
    });
    (registry, receiver, worker)
}

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
struct RemoteTransformSink {
    remote: RuntimeGraphExecutor,
    metadata: Arc<Mutex<Vec<flow::graph::RemoteEventMetadata>>>,
    values: Arc<Mutex<Vec<Value>>>,
}

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
impl GraphExecutor for RemoteTransformSink {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        match node.id.as_str() {
            "source" => self.remote.execute(node, io, context),
            "transform" => {
                while let Some(event) = io.recv("updates")? {
                    if let Some(metadata) = event.remote {
                        self.metadata.lock().unwrap().push(metadata);
                    }
                    io.emit("out", event.payload)?;
                }
                Ok(())
            }
            "sink" => {
                while let Some(event) = io.recv("value")? {
                    let PortValue::Value(value) = event.payload else {
                        return Err(anyhow!("expected typed text event"));
                    };
                    self.values.lock().unwrap().push(value);
                }
                Ok(())
            }
            _ => Err(anyhow!("unknown graph node {:?}", node.id)),
        }
    }
}

#[cfg(feature = "remote")]
#[test]
fn local_sse_listener_routes_typed_events_through_transform_and_sink() {
    let (registry, requests, server) = fixture_server();
    let mut plan = graph::parse(
        "graph remote_events\nlisten to remote.feed as source\nend graph\n",
        &registry,
    )
    .unwrap();
    assert_eq!(
        plan.nodes()[0].events["updates"].delivery,
        flow::contracts::PortDelivery::Events
    );
    assert_eq!(
        plan.nodes()[0].events["updates"].schema.kind,
        OutputType::Text
    );

    plan.add_node(GraphNode {
        id: "transform".into(),
        kind: GraphNodeKind::Renderer {
            backend: "test".into(),
        },
        inputs: BTreeMap::from([("updates".into(), PortSchema::events(text_schema()))]),
        outputs: BTreeMap::from([("out".into(), PortSchema::records(text_schema()))]),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    plan.add_node(GraphNode {
        id: "sink".into(),
        kind: GraphNodeKind::Renderer {
            backend: "test".into(),
        },
        inputs: BTreeMap::from([("value".into(), PortSchema::records(text_schema()))]),
        outputs: BTreeMap::new(),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("source", "updates"),
        PortRef::new("transform", "updates"),
        1,
    ));
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("transform", "out"),
        PortRef::new("sink", "value"),
        1,
    ));
    plan.validate_with_registry(&registry).unwrap();

    let metadata = Arc::new(Mutex::new(Vec::new()));
    let values = Arc::new(Mutex::new(Vec::new()));
    let executor = Arc::new(RemoteTransformSink {
        remote: RuntimeGraphExecutor::new(registry.clone(), options()),
        metadata: Arc::clone(&metadata),
        values: Arc::clone(&values),
    });
    GraphSession::start(plan, executor, GraphRunOptions::default())
        .unwrap()
        .run_to_completion()
        .unwrap();

    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap(),
        "GET /.well-known/execute-applet/v1 HTTP/1.1"
    );
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap(),
        "GET /events/updates HTTP/1.1"
    );
    server.join().unwrap();
    assert_eq!(
        *values.lock().unwrap(),
        vec![
            Value::String("first".into()),
            Value::String("second".into())
        ]
    );
    let metadata = metadata.lock().unwrap();
    assert_eq!(metadata.len(), 2);
    assert_eq!(metadata[0].cursor.as_deref(), Some("cursor-1"));
    assert!(!metadata[0].duplicate_cursor);
    assert!(metadata[1].duplicate_cursor);
}

#[cfg(unix)]
#[cfg(feature = "remote")]
#[test]
fn remote_listener_accounts_dns_helper_spawns_in_graph_result() {
    let (registry, requests, server) = fixture_server_at_host("localhost");
    let plan = graph::parse(
        "graph remote_listener_dns_accounting\nlisten to remote.feed as source\nend graph\n",
        &registry,
    )
    .unwrap();
    let executor = Arc::new(
        RuntimeGraphExecutor::new(registry.clone(), options())
            .with_resolver_helper(env!("CARGO_BIN_EXE_execute")),
    );

    let result = GraphSession::start(plan, executor, GraphRunOptions::default())
        .unwrap()
        .run_to_completion()
        .unwrap();
    assert_eq!(result.outputs.len(), 2);
    assert!(
        result.launched_processes >= 1,
        "the actual execute resolver helper was spawned for localhost"
    );
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap(),
        "GET /.well-known/execute-applet/v1 HTTP/1.1"
    );
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap(),
        "GET /events/updates HTTP/1.1"
    );
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn cancelled_native_stream_accounts_and_reaps_its_spawned_process() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("native-stream.pid");
    let script = "printf '%s\\n' \"$$\" > \"$1\"; printf 'ready\\n'; exec /bin/sleep 30";
    let manifest = serde_json::json!({
        "name": "fixture.hanging-stream",
        "binary": "/bin/sh",
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [
            {"literal": "-c"},
            {"literal": script},
            {"literal": "sh"},
            {"literal": pid_file.to_string_lossy()}
        ],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "bytes", "encoding": "raw"},
            "capabilities": [],
            "effects": []
        }
    });
    let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
    let plan = graph::parse(
        "graph cancelled_native_stream\nstream from fixture.hanging-stream as source\nshow each line\nend graph\n",
        &registry,
    )
    .unwrap();
    let executor = Arc::new(RuntimeGraphExecutor::new(
        registry,
        options_for_directory(temp.path()),
    ));
    let mut session = GraphSession::start(plan, executor, GraphRunOptions::default()).unwrap();

    let ready = session
        .recv_output()
        .expect("the native stream emitted its readiness record")
        .expect("the stream remains live after its first record");
    assert_eq!(ready.payload, PortValue::Bytes(b"ready\n".to_vec()));
    assert!(
        pid_file.exists(),
        "the child wrote its PID before readiness"
    );
    let pid: libc::pid_t = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    session.cancel_and_join().unwrap();
    assert_eq!(session.context().metrics.launched_processes(), 1);

    let started = Instant::now();
    // SAFETY: `pid` was parsed from the pid file the fixture child wrote before its readiness
    // record. Signal 0 delivers nothing and only probes for existence. A zombie still answers
    // the probe, so this loop keeps polling until the child is reaped or 2 s pass.
    while unsafe { libc::kill(pid, 0) } == 0 && started.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        // SAFETY: signal 0 delivers nothing; it checks that `pid`, the child PID read above,
        // no longer exists. The assertion requires the probe to fail, meaning the child
        // was reaped after `cancel_and_join`.
        unsafe { libc::kill(pid, 0) },
        -1,
        "cancelled stream child was reaped"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "the child PID no longer exists after graph cancellation"
    );
}

#[cfg(unix)]
fn options_for_directory(directory: &std::path::Path) -> RunOptions {
    let mut run_options = options();
    run_options.working_directory = directory.to_path_buf();
    run_options
}

#[cfg(feature = "remote")]
#[test]
fn idle_remote_sse_listener_is_cancelled_before_reconfigure_starts_next_generation() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
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
    let descriptor = serde_json::to_vec(&json!({
        "protocol_version": 1,
        "identity": {"name": "remote.feed", "version": "1"},
        "contract": contract,
        "events": [{
            "port": "updates",
            "format": "sse",
            "max_event_bytes": 4096,
            "resumable": false,
            "backpressure": "pull"
        }]
    }))
    .unwrap();
    let manifest = json!({
        "name": "remote.feed",
        "binary": "@remote-applet",
        "parameters": {},
        "arguments": [],
        "contract": contract
    });
    let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
    let (event_requests, event_request_rx) = mpsc::channel();
    let (old_stream_closed, old_stream_closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        for stream_number in 0..2 {
            let (mut discovery, _) = listener.accept().unwrap();
            let discovery_line = read_request_line(&discovery);
            assert!(discovery_line.contains("/.well-known/"));
            write_response(
                &mut discovery,
                "application/vnd.execute.applet+json;version=1",
                &descriptor,
            );

            let (mut events, _) = listener.accept().unwrap();
            let event_line = read_request_line(&events);
            assert!(event_line.contains("/events/updates"));
            event_requests.send(stream_number).unwrap();
            events
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
                )
                .unwrap();
            events.flush().unwrap();

            if stream_number == 0 {
                // Keep the first generation genuinely idle while the graph
                // session is reconfigured. The periodic comment also lets the
                // server observe that the cancelled client closed its socket.
                write_chunk(&mut events, b": keepalive\n\n").unwrap();
                loop {
                    thread::sleep(Duration::from_millis(10));
                    if write_chunk(&mut events, b": keepalive\n\n").is_err() {
                        old_stream_closed.send(()).unwrap();
                        break;
                    }
                }
            } else {
                write_chunk(
                    &mut events,
                    b"id: next-generation\nevent: changed\ndata: \"current\"\n\n",
                )
                .unwrap();
                events.write_all(b"0\r\n\r\n").unwrap();
                events.flush().unwrap();
            }
        }
    });

    let plan = graph::parse(
        "graph remote_events\nlisten to remote.feed as source\nend graph\n",
        &registry,
    )
    .unwrap();
    let first_executor = Arc::new(RuntimeGraphExecutor::new(registry.clone(), options()));
    let session =
        GraphSession::start(plan.clone(), first_executor, GraphRunOptions::default()).unwrap();
    assert_eq!(
        event_request_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        0
    );

    let next_executor = Arc::new(RuntimeGraphExecutor::new(registry, options()));
    let started = Instant::now();
    let next = session
        .reconfigure(plan, next_executor, GraphRunOptions::default())
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "reconfigure waited too long for the idle remote event source"
    );
    assert_eq!(
        event_request_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        1
    );
    let result = next.run_to_completion().unwrap();
    server.join().unwrap();
    old_stream_closed_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    assert_eq!(result.generation, 2);
    assert_eq!(result.revision, 2);
    assert_eq!(result.outputs.len(), 1);
    assert_eq!(result.outputs[0].generation, 2);
    assert_eq!(result.outputs[0].revision, 2);
    assert_eq!(
        result.outputs[0].payload,
        PortValue::Value(Value::String("current".into()))
    );
}

#[cfg(unix)]
#[test]
fn idle_native_listener_child_is_reaped_during_reconfigure() {
    let manifest = r#"{
      "name":"quiet.producer",
      "binary":"/bin/sh",
      "arguments":[
        {"literal":"-c"},
        {"literal":"printf 'ready\\n'; exec /bin/sleep 30"}
      ],
      "parameters":{},
      "contract":{"version":1,"output":{"type":"bytes","encoding":"raw"}}
    }"#;
    let registry = Registry::from_sources(&[manifest]).unwrap();
    let plan = graph::parse(
        "graph native_events\nlisten to quiet.producer as source\nend graph\n",
        &registry,
    )
    .unwrap();
    let mut run_options = options();
    run_options.command_timeout = Some(Duration::from_secs(3));
    let first_executor = Arc::new(RuntimeGraphExecutor::new(
        registry.clone(),
        run_options.clone(),
    ));
    let mut session =
        GraphSession::start(plan.clone(), first_executor, GraphRunOptions::default()).unwrap();
    let first_event = session.recv_output().unwrap().unwrap();
    assert_eq!(first_event.generation, 1);
    assert_eq!(first_event.payload, PortValue::Bytes(b"ready\n".to_vec()));

    let next_executor = Arc::new(RuntimeGraphExecutor::new(registry, run_options));
    let started = Instant::now();
    let mut next = session
        .reconfigure(plan, next_executor, GraphRunOptions::default())
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "reconfigure waited for the quiet native listener child"
    );
    let next_event = next.recv_output().unwrap().unwrap();
    assert_eq!(next_event.generation, 2);
    assert_eq!(next_event.revision, 2);
    assert_eq!(next_event.payload, PortValue::Bytes(b"ready\n".to_vec()));

    let started = Instant::now();
    next.cancel_and_join().unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "cancellation waited for the quiet native listener child"
    );
}

#[cfg(feature = "remote")]
#[test]
fn expired_remote_listener_deadline_prevents_discovery_request() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let manifest = json!({
        "name":"remote.feed",
        "binary":"@remote-applet",
        "parameters":{},
        "arguments":[],
        "contract":{
            "version":1,
            "backend":"remote",
            "remote":{
                "endpoint":endpoint,
                "allow_loopback_http_fixture":true,
                "allow_discovery":true
            },
            "ports":{"events":{"updates":{"schema":{"type":"text"},"delivery":"events"}}}
        }
    });
    let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
    let plan = graph::parse(
        "graph expired_remote\nlisten to remote.feed as source\nend graph\n",
        &registry,
    )
    .unwrap();
    let executor = Arc::new(RuntimeGraphExecutor::new(registry, options()));
    let deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let session = GraphSession::start(
        plan,
        executor,
        GraphRunOptions {
            deadline: Some(deadline),
            ..GraphRunOptions::default()
        },
    )
    .unwrap();
    assert!(session.run_to_completion().is_err());
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn read_request_line(stream: &std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first_line = String::new();
    reader.read_line(&mut first_line).unwrap();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header == "\r\n" || header.is_empty() {
            break;
        }
    }
    first_line
}

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn write_response(stream: &mut std::net::TcpStream, content_type: &str, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
}

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn write_chunk(stream: &mut std::net::TcpStream, body: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:x}\r\n", body.len())?;
    stream.write_all(body)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}
