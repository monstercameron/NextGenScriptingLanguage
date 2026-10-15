#![cfg(unix)]

use anyhow::{anyhow, bail, Context, Result};
use flow::ast::Value;
use flow::graph::{
    self, GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphRunOptions, GraphSession,
    NodeIo, PortValue, RuntimeGraphExecutor,
};
use flow::manifest::Registry;
use flow::modules::ProgramModuleSession;
use flow::runtime::RunOptions;
use serde_json::{json, Value as JsonValue};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
const COMPOSITION_DIR: &str = "examples/composition/graph-runtime";

#[derive(Clone, Debug)]
struct ObservedUpdate {
    source_node: String,
    generation: u64,
    revision: u64,
    text: String,
}

/// This fixture replaces only the renderer node. Applet, program-service,
/// remote-listener, and native-stream nodes still use RuntimeGraphExecutor.
struct HeadlessTuiFixture {
    runtime: RuntimeGraphExecutor,
    updates: mpsc::Sender<ObservedUpdate>,
}

impl GraphExecutor for HeadlessTuiFixture {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        if matches!(&node.kind, GraphNodeKind::Renderer { backend } if backend == "tui:text") {
            let initial = io
                .recv("properties")?
                .ok_or_else(|| anyhow!("headless TUI fixture did not receive its seed"))?;
            match initial.payload {
                PortValue::Value(Value::Record(fields))
                    if fields.get("text") == Some(&Value::String("ready".into())) => {}
                payload => bail!("unexpected headless TUI seed: {payload:?}"),
            }

            while let Some(event) = io.recv("property_updates")? {
                let PortValue::Value(Value::Record(fields)) = event.payload else {
                    bail!("headless TUI fixture expected a typed property record");
                };
                let Some(Value::String(text)) = fields.get("text") else {
                    bail!("headless TUI update has no text field");
                };
                self.updates
                    .send(ObservedUpdate {
                        source_node: event.port.node,
                        generation: event.generation,
                        revision: event.revision,
                        text: text.clone(),
                    })
                    .map_err(|_| anyhow!("acceptance test stopped reading renderer updates"))?;
            }
            return Ok(());
        }

        self.runtime.execute(node, io, context)
    }
}

struct LoopbackSseServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    closed: mpsc::Receiver<u64>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl LoopbackSseServer {
    fn start(listener: TcpListener, descriptor: Vec<u8>, generations: u64) -> Result<Self> {
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (closed_sender, closed) = mpsc::channel();
        let worker = thread::spawn(move || -> Result<()> {
            for generation in 1..=generations {
                let Some(mut discovery) = accept_until_stopped(&listener, &thread_stop) else {
                    break;
                };
                let request = read_request(&mut discovery)?;
                if !request.starts_with("GET /.well-known/execute-applet/v1 ") {
                    bail!("unexpected discovery request: {request:?}");
                }
                write_response(
                    &mut discovery,
                    "application/vnd.execute.applet+json;version=1",
                    &descriptor,
                )?;
                drop(discovery);

                let Some(mut events) = accept_until_stopped(&listener, &thread_stop) else {
                    break;
                };
                let request = read_request(&mut events)?;
                if !request.starts_with("GET /events/updates ") {
                    bail!("unexpected event request: {request:?}");
                }
                events.set_write_timeout(Some(Duration::from_millis(250)))?;
                events.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
                )?;
                events.flush()?;

                for sequence in 1..=3 {
                    let data = json!({"text": format!("remote-{sequence}")});
                    let frame = format!(
                        "id: {generation}-{sequence}\nevent: update\ndata: {}\n\n",
                        serde_json::to_string(&data)?
                    );
                    write_chunk(&mut events, frame.as_bytes())?;
                }

                loop {
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if write_chunk(&mut events, b": keepalive\n\n").is_err() {
                        let _ = closed_sender.send(generation);
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            Ok(())
        });

        Ok(Self {
            address,
            stop,
            closed,
            thread: Some(worker),
        })
    }

    fn wait_closed(&self, expected_generation: u64) -> Result<()> {
        let closed_generation = self
            .closed
            .recv_timeout(Duration::from_secs(3))
            .with_context(|| format!("waiting for SSE socket {expected_generation} to close"))?;
        if closed_generation != expected_generation {
            bail!(
                "SSE fixture observed generation {closed_generation} close while waiting for {expected_generation}"
            );
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.thread.take() {
            worker
                .join()
                .map_err(|_| anyhow!("loopback SSE fixture panicked"))??;
        }
        Ok(())
    }
}

impl Drop for LoopbackSseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

fn accept_until_stopped(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => return Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
    None
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;
    if first_line.is_empty() {
        return Ok(first_line);
    }
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header == "\r\n" || header.is_empty() {
            break;
        }
    }
    Ok(first_line.trim_end().to_owned())
}

fn write_response(stream: &mut TcpStream, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn write_chunk(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:X}\r\n", bytes.len())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn record_schema() -> JsonValue {
    json!({
        "type": "record",
        "fields": { "text": { "type": "text" } },
        "required": ["text"]
    })
}

fn remote_contract(endpoint: &str) -> JsonValue {
    json!({
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
                    "schema": record_schema(),
                    "delivery": "events"
                }
            }
        }
    })
}

fn python3() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("Python 3 is available for the persistent-worker fixture")
}

fn make_generation(
    root: &Path,
    endpoint: &str,
    worker_pids: &Path,
    worker_effects: &Path,
    updates: &mpsc::Sender<ObservedUpdate>,
) -> Result<(graph::GraphPlan, Arc<dyn GraphExecutor>)> {
    let applets = root.join("applets");
    fs::create_dir_all(&applets)?;
    let example_root = Path::new(env!("CARGO_MANIFEST_DIR")).join(COMPOSITION_DIR);
    fs::write(
        applets.join("scheduled.program"),
        fs::read(example_root.join("scheduled.program"))?,
    )?;
    fs::write(
        applets.join("scheduled.json"),
        fs::read(example_root.join("scheduled.json"))?,
    )?;

    let output_schema = json!({
        "type": "record",
        "encoding": "json",
        "fields": { "text": { "type": "text" } },
        "required": ["text"]
    });
    let worker_script = example_root.join("transform_worker.py");
    let worker = json!({
        "name": "composition.worker_transform",
        "binary": python3(),
        "accepts_stdin": true,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "json_lines_worker",
            "lifecycle": {
                "kind": "json_lines",
                "startup_arguments": [worker_script, "--worker", worker_pids, worker_effects],
                "timeout_ms": 3000,
                "max_frame_bytes": 1048576,
                "max_stderr_bytes": 8192
            },
            "output": output_schema,
            "ports": {
                "inputs": {
                    "stdin": {
                        "schema": record_schema(),
                        "delivery": "events",
                        "encoding": "json"
                    }
                },
                "outputs": {
                    "stdout": {
                        "schema": record_schema(),
                        "delivery": "records"
                    }
                }
            }
        }
    });
    fs::write(
        applets.join("worker.json"),
        serde_json::to_vec_pretty(&worker)?,
    )?;

    let native = json!({
        "name": "composition.native_stream",
        "binary": "/bin/echo",
        "arguments": [{ "literal": "{\"text\":\"native-stream\"}" }],
        "parameters": {},
        "contract": { "version": 1, "output": output_schema }
    });
    fs::write(
        applets.join("native.json"),
        serde_json::to_vec_pretty(&native)?,
    )?;

    let contract = remote_contract(endpoint);
    let remote_manifest = json!({
        "name": "composition.remote_feed",
        "binary": "@remote-applet",
        "parameters": {},
        "arguments": [],
        "contract": contract
    });
    fs::write(
        applets.join("remote.json"),
        serde_json::to_vec_pretty(&remote_manifest)?,
    )?;

    let registry = Registry::load(&applets)?;
    let graph_source = fs::read_to_string(example_root.join("graph.program"))?;
    let plan = graph::parse(&graph_source, &registry)?;
    plan.validate_with_registry(&registry)?;
    let module_session = Arc::new(ProgramModuleSession::preload_invocations(
        &registry,
        plan.invocations(),
    )?);
    let run_options = RunOptions {
        inputs: BTreeMap::new(),
        working_directory: root.to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(8)),
        total_timeout: None,
    };
    let runtime =
        RuntimeGraphExecutor::with_module_session(registry.clone(), run_options, module_session);
    // Do not keep a registry clone outside the generation executor. Ordinary
    // worker applets retain their JSON-lines process in Registry host state.
    drop(registry);
    let executor: Arc<dyn GraphExecutor> = Arc::new(HeadlessTuiFixture {
        runtime,
        updates: updates.clone(),
    });
    Ok((plan, executor))
}

fn graph_options() -> GraphRunOptions {
    GraphRunOptions {
        deadline: Some(Instant::now() + Duration::from_secs(10)),
        ..GraphRunOptions::default()
    }
}

fn collect_until(
    receiver: &mpsc::Receiver<ObservedUpdate>,
    events: &mut Vec<ObservedUpdate>,
    generation: u64,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !has_generation_evidence(events, generation) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for generation {generation} composition updates");
        }
        events.push(
            receiver
                .recv_timeout(remaining)
                .with_context(|| format!("waiting for generation {generation} renderer data"))?,
        );
    }
    Ok(())
}

fn has_generation_evidence(events: &[ObservedUpdate], generation: u64) -> bool {
    let generation_events = events
        .iter()
        .filter(|event| event.generation == generation)
        .collect::<Vec<_>>();
    let transformed = generation_events
        .iter()
        .filter(|event| event.source_node == "transform")
        .map(|event| event.text.as_str())
        .collect::<Vec<_>>();
    ["remote-1", "remote-2", "remote-3"].iter().all(|name| {
        transformed
            .iter()
            .any(|text| text.starts_with(&format!("worker:{name}#")))
    }) && transformed
        .iter()
        .any(|text| text.starts_with("worker:scheduled#"))
        && generation_events
            .iter()
            .any(|event| event.source_node == "native" && event.text == "native-stream")
}

fn drain_updates(receiver: &mpsc::Receiver<ObservedUpdate>, events: &mut Vec<ObservedUpdate>) {
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
}

fn assert_generation_events(events: &[ObservedUpdate], generation: u64, revision: u64) {
    let generation_events = events
        .iter()
        .filter(|event| event.generation == generation)
        .collect::<Vec<_>>();
    assert_eq!(generation_events.len(), 5, "{generation_events:?}");
    assert!(
        generation_events
            .iter()
            .all(|event| event.revision == revision),
        "generation {generation} contains a stale revision: {generation_events:?}"
    );

    let transformed = generation_events
        .iter()
        .filter(|event| event.source_node == "transform")
        .map(|event| event.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(transformed.len(), 4, "{generation_events:?}");
    let mut sequence = transformed
        .iter()
        .map(|text| {
            let (prefix, suffix) = text
                .rsplit_once('#')
                .expect("worker output includes its persistent sequence number");
            assert!(
                prefix.starts_with("worker:"),
                "unexpected worker result {text}"
            );
            suffix.parse::<u64>().expect("worker sequence is numeric")
        })
        .collect::<Vec<_>>();
    sequence.sort_unstable();
    assert_eq!(sequence, vec![1, 2, 3, 4]);

    for source in ["remote-1", "remote-2", "remote-3"] {
        assert_eq!(
            transformed
                .iter()
                .filter(|text| text.starts_with(&format!("worker:{source}#")))
                .count(),
            1,
            "lossless bounded queue did not preserve {source} exactly once"
        );
    }
    assert_eq!(
        transformed
            .iter()
            .filter(|text| text.starts_with("worker:scheduled#"))
            .count(),
        1,
        "expected one scheduled event to traverse the shared worker"
    );
    assert_eq!(
        generation_events
            .iter()
            .filter(|event| event.source_node == "native" && event.text == "native-stream")
            .count(),
        1
    );
}

fn read_pids(path: &Path) -> Vec<i32> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| line.parse().expect("fixture PID is numeric"))
        .collect()
}

fn wait_for_pid_count(path: &Path, count: usize, timeout: Duration) -> Result<Vec<i32>> {
    let deadline = Instant::now() + timeout;
    loop {
        let pids = read_pids(path);
        if pids.len() >= count {
            return Ok(pids);
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for {count} PID markers in {}",
                path.display()
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_metric(metric: impl Fn() -> usize, count: usize, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while metric() < count {
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for {count} successful process launches; observed {}",
                metric()
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn assert_process_reaped(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let result = unsafe { libc::kill(pid, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        assert!(Instant::now() < deadline, "process {pid} was not reaped");
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_effects(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn parsed_runtime_graph_composes_services_workers_streams_and_reconfiguration() -> Result<()> {
    let root = tempfile::tempdir()?;
    let applets = root.path().join("applets");
    fs::create_dir_all(&applets)?;
    let worker_pids = root.path().join("worker.pids");
    let worker_effects = root.path().join("worker.effects");

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let contract = remote_contract(&endpoint);
    let descriptor = serde_json::to_vec(&json!({
        "protocol_version": 1,
        "identity": { "name": "composition.remote_feed", "version": "1" },
        "contract": contract,
        "events": [{
            "port": "updates",
            "format": "sse",
            "max_event_bytes": 4096,
            "resumable": false,
            "backpressure": "pull"
        }]
    }))?;
    let server = LoopbackSseServer::start(listener, descriptor, 2)?;

    let (updates_sender, updates_receiver) = mpsc::channel();
    let (first_plan, first_executor) = make_generation(
        root.path(),
        &endpoint,
        &worker_pids,
        &worker_effects,
        &updates_sender,
    )?;
    let mut session = GraphSession::start(first_plan, first_executor, graph_options())?;
    assert_eq!(session.context().generation, 1);
    assert_eq!(session.context().revision, 1);
    let first_metrics = Arc::clone(&session.context().metrics);
    let mut observed = Vec::new();
    collect_until(&updates_receiver, &mut observed, 1, Duration::from_secs(8))?;
    wait_for_metric(
        || first_metrics.launched_processes(),
        2,
        Duration::from_secs(2),
    )?;
    assert_eq!(first_metrics.launched_processes(), 2);
    assert_eq!(read_effects(&worker_effects).len(), 4);
    let first_worker_pid = *wait_for_pid_count(&worker_pids, 1, Duration::from_secs(2))?
        .first()
        .expect("worker marker exists");
    let (next_plan, next_executor) = make_generation(
        root.path(),
        &endpoint,
        &worker_pids,
        &worker_effects,
        &updates_sender,
    )?;
    let reconfigure_started = Instant::now();
    session.try_reconfigure(next_plan, next_executor, graph_options())?;
    assert!(
        reconfigure_started.elapsed() < Duration::from_secs(3),
        "reconfiguration waited too long for active graph sources"
    );
    assert_eq!(session.context().generation, 2);
    assert_eq!(session.context().revision, 2);
    assert_eq!(first_metrics.launched_processes(), 2);
    server.wait_closed(1)?;
    assert_process_reaped(first_worker_pid);

    let second_metrics = Arc::clone(&session.context().metrics);
    collect_until(&updates_receiver, &mut observed, 2, Duration::from_secs(8))?;
    wait_for_metric(
        || second_metrics.launched_processes(),
        2,
        Duration::from_secs(2),
    )?;
    assert_eq!(second_metrics.launched_processes(), 2);
    let pids = wait_for_pid_count(&worker_pids, 2, Duration::from_secs(2))?;
    let worker_pid = *pids.get(1).expect("second generation worker PID exists");
    assert_ne!(worker_pid, first_worker_pid);

    session.cancel_and_join()?;
    server.wait_closed(2)?;
    assert_process_reaped(worker_pid);
    server.finish()?;
    drain_updates(&updates_receiver, &mut observed);

    assert_generation_events(&observed, 1, 1);
    assert_generation_events(&observed, 2, 2);
    assert_eq!(read_effects(&worker_effects).len(), 8);
    assert_eq!(read_pids(&worker_pids).len(), 2);
    Ok(())
}
