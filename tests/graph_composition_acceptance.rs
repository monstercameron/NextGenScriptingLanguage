#![cfg(unix)]

use anyhow::{anyhow, bail, Context, Result};
use flow::ast::Value;
use flow::graph::{
    self, GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphRunOptions, GraphSession,
    NodeIo, PortValue, RuntimeGraphExecutor,
};
use flow::manifest::Registry;
use flow::modules::{load_program_module_package, ProgramModuleSession};
use flow::runtime::RunOptions;
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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
    activity: Arc<Mutex<Vec<String>>>,
    thread_error: Arc<Mutex<Option<String>>>,
    closed: mpsc::Receiver<u64>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl LoopbackSseServer {
    fn start(listener: TcpListener, descriptor: Vec<u8>, generations: u64) -> Result<Self> {
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let activity = Arc::new(Mutex::new(Vec::new()));
        let thread_activity = Arc::clone(&activity);
        let thread_error = Arc::new(Mutex::new(None));
        let thread_thread_error = Arc::clone(&thread_error);
        let (closed_sender, closed) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = (|| -> Result<()> {
                for generation in 1..=generations {
                    let Some(mut discovery) = accept_until_stopped(&listener, &thread_stop) else {
                        break;
                    };
                    let discovery_accepted_at = Instant::now();
                    note_server_activity(
                        &thread_activity,
                        format!("generation {generation}: discovery accepted"),
                    );
                    let request = match read_request(&mut discovery) {
                        Ok(request) => request,
                        Err(error) => {
                            note_server_activity(
                                &thread_activity,
                                format!(
                                    "generation {generation}: discovery read failed after {:?}: {:?} ({error})",
                                    discovery_accepted_at.elapsed(),
                                    error.kind()
                                ),
                            );
                            return Err(error).with_context(|| {
                                format!(
                                    "reading generation {generation} discovery request after {:?}",
                                    discovery_accepted_at.elapsed()
                                )
                            });
                        }
                    };
                    note_server_activity(
                        &thread_activity,
                        format!(
                            "generation {generation}: discovery request after {:?}: {request:?}",
                            discovery_accepted_at.elapsed()
                        ),
                    );
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
                    let events_accepted_at = Instant::now();
                    note_server_activity(
                        &thread_activity,
                        format!("generation {generation}: event connection accepted"),
                    );
                    let request = match read_request(&mut events) {
                        Ok(request) => request,
                        Err(error) => {
                            note_server_activity(
                                &thread_activity,
                                format!(
                                    "generation {generation}: event read failed after {:?}: {:?} ({error})",
                                    events_accepted_at.elapsed(),
                                    error.kind()
                                ),
                            );
                            return Err(error).with_context(|| {
                                format!(
                                    "reading generation {generation} event request after {:?}",
                                    events_accepted_at.elapsed()
                                )
                            });
                        }
                    };
                    note_server_activity(
                        &thread_activity,
                        format!(
                            "generation {generation}: event request after {:?}: {request:?}",
                            events_accepted_at.elapsed()
                        ),
                    );
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
                        note_server_activity(
                            &thread_activity,
                            format!("generation {generation}: sent remote-{sequence}"),
                        );
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
            })();
            if let Err(error) = &result {
                if let Ok(mut thread_error) = thread_thread_error.lock() {
                    *thread_error = Some(format!("{error:#}"));
                }
            }
            result
        });

        Ok(Self {
            address,
            stop,
            activity,
            thread_error,
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

    fn activity(&self) -> Vec<String> {
        self.activity
            .lock()
            .map(|activity| activity.clone())
            .unwrap_or_default()
    }

    fn thread_error(&self) -> Option<String> {
        self.thread_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
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

fn note_server_activity(activity: &Mutex<Vec<String>>, note: String) {
    if let Ok(mut activity) = activity.lock() {
        activity.push(note);
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
    read_request_with_ready(stream, || {})
}

fn read_request_with_ready(
    stream: &mut TcpStream,
    on_read_start: impl FnOnce(),
) -> std::io::Result<String> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first_line = String::new();
    on_read_start();
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

#[test]
fn read_request_waits_for_a_delayed_request_on_a_nonblocking_socket() -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let mut client = TcpStream::connect(listener.local_addr()?)?;
    let accept_deadline = Instant::now() + Duration::from_secs(1);
    let (mut accepted, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= accept_deadline {
                    bail!("timed out accepting the delayed-request regression socket");
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error.into()),
        }
    };
    accepted.set_nonblocking(true)?;

    let (read_started_sender, read_started_receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        read_request_with_ready(&mut accepted, || {
            let _ = read_started_sender.send(());
        })
    });
    read_started_receiver
        .recv_timeout(Duration::from_secs(1))
        .context("waiting for the request reader to start")?;
    thread::sleep(Duration::from_millis(30));
    let write_result = client.write_all(b"GET /delayed HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let read_result = reader
        .join()
        .map_err(|_| anyhow!("delayed-request reader thread panicked"))?;
    let request = read_result?;
    write_result?;
    assert_eq!(request, "GET /delayed HTTP/1.1");
    Ok(())
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

#[allow(clippy::too_many_arguments)]
fn write_projection_module(
    applets: &Path,
    package_name: &str,
    applet_name: &str,
    source: &str,
    module_inputs: JsonValue,
    graph_inputs: JsonValue,
    parameters: JsonValue,
    accepts_stdin: bool,
) -> Result<()> {
    let package_root = applets.join(format!("{package_name}.package"));
    let package_source = package_root.join("src/library.program");
    let package_manifest = package_root.join("package/program.modules.json");
    let package_lock = package_root.join("package/program.modules.lock.json");
    fs::create_dir_all(package_source.parent().expect("source has a directory"))?;
    fs::create_dir_all(package_manifest.parent().expect("manifest has a directory"))?;
    fs::write(&package_source, source)?;
    let source_sha256 = format!("{:x}", Sha256::digest(source.as_bytes()));
    let value_schema = record_schema();
    let ports = json!({
        "inputs": graph_inputs,
        "events": {
            "result": {
                "schema": value_schema,
                "delivery": "events"
            }
        }
    });
    let output = json!({
        "type": "record",
        "encoding": "json",
        "fields": { "text": { "type": "text" } },
        "required": ["text"]
    });
    let package = json!({
        "format_version": 1,
        "name": format!("fixture.{package_name}"),
        "version": "1.0.0",
        "interface_version": 1,
        "source": "../src/library.program",
        "source_sha256": source_sha256,
        "exports": {
            "convert": {
                "entry": "convert",
                "interface_version": 1,
                "inputs": module_inputs,
                "output": output,
                "lifecycle": { "kind": "one_shot" },
                "capabilities": [],
                "ports": ports
            }
        },
        "dependencies": {}
    });
    fs::write(&package_manifest, serde_json::to_vec_pretty(&package)?)?;
    let loaded_package = load_program_module_package(&package_manifest, applets, None)?;
    fs::write(
        &package_lock,
        serde_json::to_vec_pretty(&loaded_package.lock_snapshot(applets)?)?,
    )?;

    let module_source = package_source
        .strip_prefix(applets)
        .expect("package lives under the applet module root");
    let module_manifest = package_manifest
        .strip_prefix(applets)
        .expect("package lives under the applet module root");
    let module_lock = package_lock
        .strip_prefix(applets)
        .expect("package lives under the applet module root");
    let descriptor = json!({
        "name": applet_name,
        "binary": "@program-module",
        "accepts_stdin": accepts_stdin,
        "parameters": parameters,
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": module_source,
                "entry": "convert",
                "interface_version": 1,
                "package_manifest": module_manifest,
                "package_lock": module_lock
            },
            "lifecycle": { "kind": "oneshot" },
            "output": output,
            "ports": ports
        }
    });
    fs::write(
        applets.join(format!("{package_name}.json")),
        serde_json::to_vec_pretty(&descriptor)?,
    )?;
    Ok(())
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
            bail!(
                "timed out waiting for generation {generation} composition updates; observed: {events:?}"
            );
        }
        let event = receiver.recv_timeout(remaining).with_context(|| {
            format!("waiting for generation {generation} renderer data; observed: {events:?}")
        })?;
        events.push(event);
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
        // SAFETY: signal 0 delivers no signal; it only asks the kernel whether `pid` names a
        // process. A zombie still answers the probe, so ESRCH means `pid` has been reaped.
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

fn graph_failure_diagnostic(
    error: anyhow::Error,
    session: &mut GraphSession,
    server: &LoopbackSseServer,
    worker_pids: &Path,
    worker_effects: &Path,
    observed: &[ObservedUpdate],
    launched_processes: impl FnOnce() -> usize,
) -> anyhow::Error {
    let graph_probe = session.recv_output().map(|event| format!("{event:?}"));
    let launches = launched_processes();
    let pids = read_pids(worker_pids);
    let effects = read_effects(worker_effects);
    let source_activity = server.activity();
    let source_error = server.thread_error();
    let joined = session.cancel_and_join();
    anyhow!(
        "{error:#}; diagnostic launches={launches} worker_pids={pids:?} worker_effects={effects:?} observed={observed:?} source_activity={source_activity:?} source_error={source_error:?} graph_probe={graph_probe:#?} graph_join={joined:#?}"
    )
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
    if let Err(error) = collect_until(&updates_receiver, &mut observed, 1, Duration::from_secs(8)) {
        return Err(graph_failure_diagnostic(
            error,
            &mut session,
            &server,
            &worker_pids,
            &worker_effects,
            &observed,
            || first_metrics.launched_processes(),
        ));
    }
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
    if let Err(error) = collect_until(&updates_receiver, &mut observed, 2, Duration::from_secs(8)) {
        return Err(graph_failure_diagnostic(
            error,
            &mut session,
            &server,
            &worker_pids,
            &worker_effects,
            &observed,
            || second_metrics.launched_processes(),
        ));
    }
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

#[test]
fn native_listener_projects_imported_finite_program_result_to_typed_event() -> Result<()> {
    let root = tempfile::tempdir()?;
    let applets = root.path().join("applets");
    fs::create_dir_all(&applets)?;
    write_projection_module(
        &applets,
        "native_projection",
        "composition.native_module",
        "define a program named convert\n  accept payload as json\n  then encode payload as json\n",
        json!([{ "name": "payload", "type": "json", "required": true }]),
        json!({
            "payload": {
                "schema": record_schema(),
                "delivery": "events"
            }
        }),
        json!({ "payload": { "type": "json", "required": true } }),
        false,
    )?;
    fs::write(
        applets.join("native_decode.json"),
        serde_json::to_vec_pretty(&json!({
            "name": "composition.native_decode",
            "binary": "jq",
            "accepts_stdin": true,
            "parameters": {},
            "arguments": [{ "literal": "." }],
            "contract": {
                "version": 1,
                "output": {
                    "type": "record",
                    "encoding": "json",
                    "fields": { "text": { "type": "text" } },
                    "required": ["text"]
                },
                "ports": {
                    "inputs": {
                        "stdin": {
                            "schema": { "type": "bytes" },
                            "delivery": "events",
                            "encoding": "raw"
                        }
                    },
                    "events": {
                        "result": {
                            "schema": record_schema(),
                            "delivery": "events"
                        }
                    }
                }
            }
        }))?,
    )?;
    let native_source = json!({
        "name": "composition.native_source",
        "binary": "/bin/echo",
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [{ "literal": "{\"text\":\"native-listener\"}" }],
        "contract": {
            "version": 1,
            "output": { "type": "bytes", "encoding": "raw" }
        }
    });
    fs::write(
        applets.join("native_source.json"),
        serde_json::to_vec_pretty(&native_source)?,
    )?;
    let registry = Registry::load(&applets)?;
    let plan = graph::parse(
        "graph native_result_events\n  listen to composition.native_source as source\n  run composition.native_decode as decoded\n  emit result from decoded to result\n  run composition.native_module as mapped\n  emit result from mapped to result\n  bind source.line to decoded.stdin with capacity 1 lossless\n  bind decoded.result to mapped.payload with capacity 1 lossless\nend graph\n",
        &registry,
    )?;
    plan.validate_with_registry(&registry)?;
    let module_session = Arc::new(ProgramModuleSession::preload_invocations(
        &registry,
        plan.invocations(),
    )?);
    let executor = RuntimeGraphExecutor::with_module_session(
        registry,
        RunOptions {
            inputs: BTreeMap::new(),
            working_directory: root.path().to_path_buf(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(Duration::from_secs(3)),
            total_timeout: None,
        },
        module_session,
    );
    let session = GraphSession::start(plan, Arc::new(executor), graph_options())?;
    let result = session.run_to_completion()?;
    assert_eq!(result.outputs.len(), 1, "{:?}", result.outputs);
    let event = &result.outputs[0];
    assert_eq!(event.port, flow::graph::PortRef::new("mapped", "result"));
    assert_eq!(event.generation, 1);
    assert_eq!(event.revision, 1);
    assert!(matches!(
        &event.payload,
        PortValue::Value(Value::Record(fields))
            if fields.get("text") == Some(&Value::String("native-listener".into()))
    ));
    Ok(())
}

#[test]
fn remote_typed_events_flow_through_imported_module_result_projection_and_cancel() -> Result<()> {
    let root = tempfile::tempdir()?;
    let applets = root.path().join("applets");
    fs::create_dir_all(&applets)?;
    write_projection_module(
        &applets,
        "remote_projection",
        "composition.remote_module",
        "define a program named convert\n  accept payload as json\n  then encode payload as json\n",
        json!([{ "name": "payload", "type": "json", "required": true }]),
        json!({
            "payload": {
                "schema": record_schema(),
                "delivery": "events"
            }
        }),
        json!({"payload": {"type": "json", "required": true}}),
        false,
    )?;

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
    let server = LoopbackSseServer::start(listener, descriptor, 1)?;
    let remote_manifest = json!({
        "name": "composition.remote_feed",
        "binary": "@remote-applet",
        "parameters": {},
        "arguments": [],
        "contract": remote_contract(&endpoint)
    });
    fs::write(
        applets.join("remote_feed.json"),
        serde_json::to_vec_pretty(&remote_manifest)?,
    )?;
    let registry = Registry::load(&applets)?;
    let plan = graph::parse(
        "graph remote_result_events\n  listen to composition.remote_feed as source\n  run composition.remote_module as mapped\n  emit result from mapped to result\n  bind source.updates to mapped.payload with capacity 1 lossless\nend graph\n",
        &registry,
    )?;
    plan.validate_with_registry(&registry)?;
    let module_session = Arc::new(ProgramModuleSession::preload_invocations(
        &registry,
        plan.invocations(),
    )?);
    let executor = RuntimeGraphExecutor::with_module_session(
        registry,
        RunOptions {
            inputs: BTreeMap::new(),
            working_directory: root.path().to_path_buf(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(Duration::from_secs(3)),
            total_timeout: None,
        },
        module_session,
    );
    let mut session = GraphSession::start(plan, Arc::new(executor), graph_options())?;
    let mut observed = Vec::new();
    for expected in 1..=3 {
        let event = session
            .recv_output()
            .with_context(|| format!("receiving projected remote event {expected}"))?
            .ok_or_else(|| anyhow!("remote result-event graph closed early"))?;
        observed.push(event);
        let event = observed.last().expect("just pushed output");
        assert_eq!(event.port, flow::graph::PortRef::new("mapped", "result"));
        assert_eq!(event.generation, 1);
        assert_eq!(event.revision, 1);
        assert!(matches!(
            &event.payload,
            PortValue::Value(Value::Record(fields))
                if fields.get("text") == Some(&Value::String(format!("remote-{expected}")))
        ));
    }
    assert!(observed
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence));
    session.cancel_and_join()?;
    server.wait_closed(1)?;
    server.finish()?;
    Ok(())
}
