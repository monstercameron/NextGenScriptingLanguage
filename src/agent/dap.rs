//! `execute dap`: a Debug Adapter Protocol translator for `execute run --control`.
//!
//! The adapter keeps no run state of its own. `configurationDone` starts
//! `execute run FILE --control --break-at ...`, and each later request is an
//! `execute live ...` call on that run's endpoint. A pause becomes a `stopped`
//! event and program output becomes `output` events.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// The one thread every event and stack trace refers to.
const THREAD_ID: i64 = 1;
/// The `variablesReference` of the single scope, the run's bindings.
const BINDINGS_REFERENCE: i64 = 1;
/// The largest DAP message accepted from the client.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// The stderr line `execute run --control` prints with its endpoint address.
const ENDPOINT_PREFIX: &str = "execute: control endpoint ";

/// `execute dap`: serve DAP on stdin and stdout until `disconnect` or end of input.
pub fn cli(arguments: &[String]) -> Result<i32> {
    if !arguments.is_empty() {
        bail!("usage: execute dap (no options; the adapter speaks DAP on stdin and stdout)");
    }
    let exe = std::env::current_exe().context("locating the execute executable")?;
    let sink = Sink::new();
    let mut session = Session::new(exe, sink.clone());
    let mut input = BufReader::new(io::stdin().lock());
    while let Some(message) = read_message(&mut input)? {
        if message.get("type").and_then(Value::as_str) != Some("request") {
            continue;
        }
        let command = message["command"].as_str().unwrap_or_default().to_owned();
        let result = session.handle(&command, &message["arguments"]);
        sink.reply(&message, result);
        if command == "initialize" {
            sink.event("initialized", json!({}));
        }
        if command == "disconnect" {
            break;
        }
    }
    session.shutdown();
    Ok(0)
}

/// Read one DAP message: `Content-Length` headers, a blank line, then a JSON body.
/// Returns `None` at end of input between messages.
pub fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            if length.is_none() {
                return Ok(None);
            }
            bail!("the client closed the stream inside a message header");
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("Content-Length") {
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .with_context(|| format!("invalid Content-Length {:?}", value.trim()))?,
                );
            }
        }
    }
    let length = length.context("DAP message has no Content-Length header")?;
    if length > MAX_MESSAGE {
        bail!("DAP message of {length} bytes exceeds the {MAX_MESSAGE}-byte limit");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(
        serde_json::from_slice(&body).context("DAP message body is not JSON")?,
    ))
}

/// Write one DAP message with its byte-length `Content-Length` header.
pub fn write_message(writer: &mut impl Write, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Serializes every outgoing message, from the request loop and the run's threads.
#[derive(Clone)]
struct Sink {
    out: Arc<Mutex<io::Stdout>>,
    seq: Arc<AtomicI64>,
}

impl Sink {
    fn new() -> Self {
        Self {
            out: Arc::new(Mutex::new(io::stdout())),
            seq: Arc::new(AtomicI64::new(0)),
        }
    }

    fn send(&self, mut message: Value) {
        message["seq"] = json!(self.seq.fetch_add(1, Ordering::SeqCst) + 1);
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // A client that has gone away ends the session through its input stream.
        let _ = write_message(&mut *out, &message);
    }

    fn event(&self, name: &str, body: Value) {
        self.send(json!({"type": "event", "event": name, "body": body}));
    }

    fn reply(&self, request: &Value, result: Result<Value>) {
        let mut message = json!({
            "type": "response",
            "request_seq": request["seq"],
            "command": request["command"],
            "success": result.is_ok(),
        });
        match result {
            Ok(body) => message["body"] = body,
            Err(error) => message["message"] = json!(format!("{error:#}")),
        }
        self.send(message);
    }
}

/// The program a `launch` request names, held until `configurationDone` starts it.
#[derive(Clone)]
struct Launch {
    program: PathBuf,
    cwd: PathBuf,
    inputs: Vec<(String, String)>,
}

/// A started run: its endpoint, its child process, and whether it has ended.
struct Run {
    endpoint: PathBuf,
    child: Arc<Mutex<Child>>,
    finished: Arc<AtomicBool>,
}

struct Session {
    exe: PathBuf,
    sink: Sink,
    launch: Option<Launch>,
    run: Option<Run>,
    /// The lines the client set on the launched program.
    breakpoints: BTreeSet<usize>,
    /// Set by `next`, so the stop it causes is reported as a step.
    stepping: Arc<AtomicBool>,
}

impl Session {
    fn new(exe: PathBuf, sink: Sink) -> Self {
        Self {
            exe,
            sink,
            launch: None,
            run: None,
            breakpoints: BTreeSet::new(),
            stepping: Arc::new(AtomicBool::new(false)),
        }
    }

    fn handle(&mut self, command: &str, arguments: &Value) -> Result<Value> {
        match command {
            "initialize" => Ok(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsTerminateRequest": false,
                "supportsEvaluateForHovers": false,
            })),
            "launch" => self.launch(arguments),
            "setBreakpoints" => self.set_breakpoints(arguments),
            "configurationDone" => self.start(),
            "threads" => Ok(json!({"threads": [{"id": THREAD_ID, "name": "main"}]})),
            "stackTrace" => Ok(self.stack_trace()),
            "scopes" => Ok(json!({"scopes": [{
                "name": "Bindings",
                "variablesReference": BINDINGS_REFERENCE,
                "expensive": false,
            }]})),
            "variables" => Ok(self.variables()),
            "next" => self.next(),
            "continue" => self.resume(),
            "disconnect" => {
                self.shutdown();
                Ok(json!({}))
            }
            other => bail!("unsupported request {other:?}"),
        }
    }

    fn launch(&mut self, arguments: &Value) -> Result<Value> {
        let program = arguments["program"]
            .as_str()
            .context("launch requires a program path")?;
        let cwd = match arguments["cwd"].as_str() {
            Some(directory) => PathBuf::from(directory),
            None => std::env::current_dir().context("reading the working directory")?,
        };
        let program = if Path::new(program).is_absolute() {
            PathBuf::from(program)
        } else {
            cwd.join(program)
        };
        if !program.is_file() {
            bail!("program {} does not exist", program.display());
        }
        let inputs = arguments["inputs"]
            .as_object()
            .map(|map| {
                map.iter()
                    .map(|(name, value)| {
                        let text = match value {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        (name.clone(), text)
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.launch = Some(Launch {
            program,
            cwd,
            inputs,
        });
        Ok(json!({}))
    }

    fn set_breakpoints(&mut self, arguments: &Value) -> Result<Value> {
        let source = arguments["source"]["path"]
            .as_str()
            .context("setBreakpoints requires source.path")?;
        let requested: Vec<usize> = arguments["breakpoints"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["line"].as_u64())
                    .filter(|line| *line >= 1)
                    .filter_map(|line| usize::try_from(line).ok())
                    .collect()
            })
            .unwrap_or_default();
        let is_program = self
            .launch
            .as_ref()
            .is_some_and(|launch| same_file(Path::new(source), &launch.program));
        if !is_program {
            // The run has one program; breakpoints in other files cannot be honoured.
            let unverified: Vec<Value> = requested
                .iter()
                .map(|line| {
                    json!({
                        "verified": false,
                        "line": line,
                        "message": "only the launched program has breakpoints",
                    })
                })
                .collect();
            return Ok(json!({"breakpoints": unverified}));
        }
        self.breakpoints = requested.iter().copied().collect();
        if let Some(run) = &self.run {
            let lines: Vec<String> = self.breakpoints.iter().map(usize::to_string).collect();
            let mut arguments = vec!["control", "breakpoint"];
            arguments.extend(lines.iter().map(String::as_str));
            live_call(&self.exe, &run.endpoint, &arguments)?;
        }
        let verified: Vec<Value> = requested
            .iter()
            .map(|line| json!({"verified": true, "line": line}))
            .collect();
        Ok(json!({"breakpoints": verified}))
    }

    fn start(&mut self) -> Result<Value> {
        if self.run.is_some() {
            return Ok(json!({}));
        }
        let launch = self
            .launch
            .clone()
            .context("configurationDone arrived before launch")?;
        let mut command = Command::new(&self.exe);
        command
            .arg("run")
            .arg(&launch.program)
            .arg("--control")
            .current_dir(&launch.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for line in &self.breakpoints {
            command.arg("--break-at").arg(line.to_string());
        }
        for (name, value) in &launch.inputs {
            command.arg("--input").arg(format!("{name}={value}"));
        }
        let mut child = command.spawn().context("starting execute run")?;
        let stdout = child.stdout.take().context("capturing program output")?;
        let stderr = child.stderr.take().context("capturing diagnostics")?;

        let (endpoint_sender, endpoint_receiver) = mpsc::channel();
        let output_sink = self.sink.clone();
        let output = thread::spawn(move || forward_output(stdout, &output_sink));
        let diagnostic_sink = self.sink.clone();
        let diagnostics =
            thread::spawn(move || forward_diagnostics(stderr, &diagnostic_sink, endpoint_sender));
        let endpoint = match endpoint_receiver.recv_timeout(Duration::from_secs(20)) {
            Ok(endpoint) => PathBuf::from(endpoint),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = output.join();
                let _ = diagnostics.join();
                bail!("the run did not open a control endpoint");
            }
        };

        let child = Arc::new(Mutex::new(child));
        let finished = Arc::new(AtomicBool::new(false));
        {
            let child = child.clone();
            let finished = finished.clone();
            let sink = self.sink.clone();
            thread::spawn(move || {
                let code = wait_for_exit(&child);
                // Output and diagnostics finish before the run is reported as ended.
                let _ = output.join();
                let _ = diagnostics.join();
                sink.event("exited", json!({"exitCode": code}));
                sink.event("terminated", json!({}));
                finished.store(true, Ordering::SeqCst);
            });
        }
        {
            let endpoint = endpoint.clone();
            let finished = finished.clone();
            let sink = self.sink.clone();
            let stepping = self.stepping.clone();
            let exe = self.exe.clone();
            thread::spawn(move || watch_stops(&exe, &endpoint, &finished, &sink, &stepping));
        }
        self.run = Some(Run {
            endpoint,
            child,
            finished,
        });
        Ok(json!({}))
    }

    /// The paused statement's line and bindings, or `None` when the run is not paused.
    fn paused(&self) -> Option<Value> {
        let run = self.run.as_ref()?;
        if run.finished.load(Ordering::SeqCst) {
            return None;
        }
        let reply = live_call(&self.exe, &run.endpoint, &["read", "/runtime/debug"]).ok()?;
        let paused = reply["result"]["paused"].clone();
        paused.is_object().then_some(paused)
    }

    fn stack_trace(&self) -> Value {
        let (Some(paused), Some(launch)) = (self.paused(), self.launch.as_ref()) else {
            return json!({"stackFrames": [], "totalFrames": 0});
        };
        let name = launch
            .program
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        json!({
            "stackFrames": [{
                "id": THREAD_ID,
                "name": "main",
                "source": {"name": name, "path": launch.program.to_string_lossy()},
                "line": paused["line"],
                "column": 1,
            }],
            "totalFrames": 1,
        })
    }

    fn variables(&self) -> Value {
        let Some(paused) = self.paused() else {
            return json!({"variables": []});
        };
        let variables: Vec<Value> = paused["bindings"]
            .as_object()
            .map(|map| {
                map.iter()
                    .map(|(name, value)| {
                        json!({
                            "name": name,
                            "value": render(value),
                            "variablesReference": 0,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({"variables": variables})
    }

    fn next(&mut self) -> Result<Value> {
        let run = self.run.as_ref().context("the run has not started")?;
        if self.paused().is_none() {
            bail!("the run is not paused at a breakpoint");
        }
        self.stepping.store(true, Ordering::SeqCst);
        live_call(&self.exe, &run.endpoint, &["control", "step"])?;
        Ok(json!({}))
    }

    /// `continue` releases a paused statement. Running already, it is a no-op.
    fn resume(&mut self) -> Result<Value> {
        let run = self.run.as_ref().context("the run has not started")?;
        self.stepping.store(false, Ordering::SeqCst);
        if self.paused().is_some() {
            live_call(&self.exe, &run.endpoint, &["control", "continue"])?;
        }
        Ok(json!({"allThreadsContinued": true}))
    }

    /// Stop a run that is still going, and wait for its exit events.
    fn shutdown(&mut self) {
        let Some(run) = self.run.take() else {
            return;
        };
        if run.finished.load(Ordering::SeqCst) {
            return;
        }
        let _ = live_call(&self.exe, &run.endpoint, &["control", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !run.finished.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if !run.finished.load(Ordering::SeqCst) {
            let _ = run
                .child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .kill();
        }
    }
}

/// Wait for the child to exit and return its status, or 1 when it has no code.
fn wait_for_exit(child: &Mutex<Child>) -> i32 {
    loop {
        let polled = child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .try_wait();
        match polled {
            Ok(Some(status)) => return status.code().unwrap_or(1),
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => return 1,
        }
    }
}

/// Forward the program's stdout as `output` events until it closes.
fn forward_output(mut pipe: impl Read, sink: &Sink) {
    let mut buffer = [0u8; 4096];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => sink.event(
                "output",
                json!({
                    "category": "stdout",
                    "output": String::from_utf8_lossy(&buffer[..count]),
                }),
            ),
        }
    }
}

/// Send the endpoint line to the session and forward other diagnostics as `output`.
fn forward_diagnostics(pipe: impl Read, sink: &Sink, endpoint: mpsc::Sender<String>) {
    for line in BufReader::new(pipe).lines().map_while(Result::ok) {
        if let Some(address) = line.strip_prefix(ENDPOINT_PREFIX) {
            let _ = endpoint.send(address.to_owned());
        } else {
            sink.event(
                "output",
                json!({"category": "stderr", "output": format!("{line}\n")}),
            );
        }
    }
}

/// Report each new pause as a `stopped` event until the run ends. `live wait`
/// returns on every change and after a short timeout, so this does not spin.
fn watch_stops(
    exe: &Path,
    endpoint: &Path,
    finished: &AtomicBool,
    sink: &Sink,
    stepping: &AtomicBool,
) {
    let mut after = 0u64;
    let mut reported = None;
    while !finished.load(Ordering::SeqCst) {
        let Ok(reply) = live_call(
            exe,
            endpoint,
            &[
                "wait",
                "/runtime/debug",
                "--after",
                &after.to_string(),
                "--timeout-ms",
                "500",
            ],
        ) else {
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        let Some(revision) = reply["revision"].as_u64() else {
            continue;
        };
        after = revision;
        let pause = reply["result"]["snapshot"]["paused"]["id"].as_u64();
        if let Some(id) = pause.filter(|id| reported != Some(*id)) {
            reported = Some(id);
            let reason = if stepping.swap(false, Ordering::SeqCst) {
                "step"
            } else {
                "breakpoint"
            };
            sink.event(
                "stopped",
                json!({"reason": reason, "threadId": THREAD_ID, "allThreadsStopped": true}),
            );
        }
    }
}

/// Run `execute live ARGS --socket ENDPOINT` and return its JSON reply, or the
/// error it reports.
fn live_call(exe: &Path, endpoint: &Path, arguments: &[&str]) -> Result<Value> {
    let endpoint = endpoint.to_string_lossy().into_owned();
    let output = Command::new(exe)
        .arg("live")
        .args(arguments)
        .args(["--socket", endpoint.as_str()])
        .stdin(Stdio::null())
        .output()
        .context("running execute live")?;
    let reply: Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "live reply is not JSON: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })?;
    if reply["ok"] == true {
        Ok(reply)
    } else {
        bail!(
            "{}",
            reply["error"]["message"]
                .as_str()
                .unwrap_or("live request failed")
        )
    }
}

/// Whether two paths name the same file, comparing canonical forms when possible.
fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// A variable's value as the client shows it: text unquoted, anything else as JSON.
fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
