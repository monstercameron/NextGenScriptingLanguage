use anyhow::{anyhow, Result};
use flow::ast::Value;
use flow::contracts::{OutputType, ValueSchema};
use flow::graph::{
    self, EdgeSpec, GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphRunOptions,
    GraphSession, NodeIo, PortRef, PortSchema, PortValue, RuntimeGraphExecutor,
};
use flow::manifest::Registry;
use flow::runtime::RunOptions;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn text_schema() -> ValueSchema {
    ValueSchema {
        kind: OutputType::Text,
        nullable: false,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
    }
}

fn options() -> RunOptions {
    RunOptions {
        inputs: BTreeMap::new(),
        working_directory: std::env::current_dir().unwrap(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: None,
    }
}

fn fixture_server() -> (Registry, mpsc::Receiver<String>, thread::JoinHandle<()>) {
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

struct RemoteTransformSink {
    remote: RuntimeGraphExecutor,
    metadata: Arc<Mutex<Vec<flow::graph::RemoteEventMetadata>>>,
    values: Arc<Mutex<Vec<Value>>>,
}

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

fn write_response(stream: &mut std::net::TcpStream, content_type: &str, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
}

fn write_chunk(stream: &mut std::net::TcpStream, body: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:x}\r\n", body.len())?;
    stream.write_all(body)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}
