//! Browser host for the shared Rust language core. No OS processes or network access.
//! Finite byte pipelines are buffered; this is not the native concurrent pipe host.
use crate::ast::{Connection, Invocation, ListenSource, Program, StepKind, Trigger, Value};
use crate::contracts::OutcomeKind;
use crate::manifest::{AppletOutput, Builtin, Manifest, Registry};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[path = "runtime/calls.rs"]
mod calls;
#[path = "runtime/control.rs"]
mod control;
#[path = "runtime/loops.rs"]
mod loops;
include!("runtime/shared.rs");

const LIMIT: usize = 1024 * 1024;
const VERSIONED_CACHE_MAGIC: &[u8] = b"FLOW-CONTRACT-CACHE\0\x01";
const APPLETS: &[&str] = &[
    "cat", "cd", "echo", "false", "grep", "head", "ls", "mkdir", "printf", "pwd", "sort", "true",
    "uniq", "wc",
];

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub action: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub stdin: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

#[derive(Default, Serialize)]
pub struct Response {
    pub ok: bool,
    pub stdout: String,
    pub error: String,
    pub files: BTreeMap<String, String>,
    /// Successful writes in this request, even when the bytes were unchanged.
    /// The virtual filesystem's 128-file bound also bounds this receipt.
    pub written_files: BTreeSet<String>,
    pub cwd: String,
    pub directories: Vec<String>,
    pub trace: Vec<serde_json::Value>,
    pub cache_hits: usize,
    pub applet_calls: usize,
    #[serde(skip)]
    trace_bytes: usize,
    #[serde(skip)]
    trace_truncated: bool,
}

impl Response {
    fn append_stdout(&mut self, bytes: &[u8]) -> Result<()> {
        let text = String::from_utf8_lossy(bytes);
        if self.stdout.len().saturating_add(text.len()) > LIMIT {
            bail!("stdout exceeds 1 MiB browser limit");
        }
        self.stdout.push_str(&text);
        Ok(())
    }

    fn push_trace(&mut self, value: impl FnOnce() -> serde_json::Value) {
        if self.trace_truncated {
            return;
        }
        let value = value();
        // Loops can execute far more statements than the source-size limit.
        // Keep diagnostics bounded independently of the execution budget.
        struct Counter(usize);
        impl std::io::Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // Use the same serializer and escaping rules without allocating a
        // second copy of every trace entry just to measure its byte length.
        let mut counter = Counter(0);
        let bytes = serde_json::to_writer(&mut counter, &value)
            .map_or(LIMIT, |_| counter.0.saturating_add(1));
        if self.trace.len() >= 4095 || self.trace_bytes.saturating_add(bytes) > LIMIT - 1024 {
            self.trace.push(serde_json::json!({"operation":"trace truncated","reason":"browser trace limit: 4096 entries or 1 MiB"}));
            self.trace_truncated = true;
        } else {
            self.trace_bytes += bytes;
            self.trace.push(value);
        }
    }
}

pub struct BrowserSession {
    files: BTreeMap<String, Vec<u8>>,
    directories: BTreeSet<String>,
    cwd: String,
    cache: BTreeMap<String, Vec<u8>>,
    registry: Registry,
}

impl Default for BrowserSession {
    fn default() -> Self {
        let sources = [
            include_str!("../manifests/cat.json"),
            include_str!("../manifests/cd.json"),
            include_str!("../manifests/count.lines.json"),
            include_str!("../manifests/false.json"),
            include_str!("../manifests/grep.json"),
            include_str!("../manifests/head.json"),
            include_str!("../manifests/ls.json"),
            include_str!("../manifests/mkdir.json"),
            include_str!("../manifests/printf.json"),
            include_str!("../manifests/pwd.json"),
            include_str!("../manifests/sort.json"),
            include_str!("../manifests/uniq.json"),
            include_str!("../manifests/wc.json"),
            include_str!("../manifests/core.json_parse.json"),
            include_str!("../manifests/core.json_stringify.json"),
            include_str!("../manifests/core.json_set.json"),
            include_str!("../manifests/core.text_format.json"),
        ];
        Self {
            files: BTreeMap::new(),
            directories: BTreeSet::from(["/".into(), "/workspace".into()]),
            cwd: "/workspace".into(),
            cache: BTreeMap::new(),
            registry: Registry::from_sources(&sources).expect("embedded applet manifests"),
        }
    }
}

impl BrowserSession {
    pub fn handle(&mut self, request: Request) -> Response {
        let mut response = Response::default();
        let result = self.dispatch(request, &mut response);
        response.ok = result.is_ok();
        if let Err(error) = result {
            response.error = format!("{error:#}");
        }
        response.files = self
            .files
            .iter()
            .map(|(path, bytes)| (path.clone(), String::from_utf8_lossy(bytes).into_owned()))
            .collect();
        response.cwd = self.cwd.clone();
        response.directories = self.directories.iter().cloned().collect();
        response
    }

    fn dispatch(&mut self, mut request: Request, response: &mut Response) -> Result<()> {
        match request.action.as_str() {
            "write" => {
                let path = absolute(&request.path, &self.cwd)?;
                self.write(&path, request.content.into_bytes())?;
                response.written_files.insert(path);
            }
            "reset" => *self = Self::default(),
            "inspect" => {}
            "command" => {
                let words = command_words(&request.command)?;
                let Some(command) = words.first() else {
                    return Ok(());
                };
                match command.as_str() {
                    "help" => response.stdout = "execute FILE.program [--input name=VALUE]\nexecute check FILE.program\nexecute explain FILE.program\nls · pwd · cd · cat · mkdir · echo · printf · sort · uniq · wc · head · grep · true · false\nUse the editor for .program pipelines. Commands have quoted argv, without shell expansion.\nBrowser applets have a documented flag subset. Files live in this tab's WASM memory.\nclear clears the terminal; reset restores the examples. No host files or network.\n".into(),
                    "execute" => {
                        let mut at = 1;
                        request.action = match words.get(at).map(String::as_str) {
                            Some("check") => {at += 1; "check"},
                            Some("explain") => {at += 1; "explain"},
                            _ => "run",
                        }.into();
                        let path = words.get(at).context("usage: execute [check|explain] FILE.program [--input name=VALUE]")?;
                        request.source = String::from_utf8(self.read(path, &self.cwd)?)?;
                        at += 1;
                        while at < words.len() {
                            if words[at] != "--input" { bail!("expected --input name=VALUE"); }
                            at += 1;
                            let (name, value) = words.get(at).and_then(|item| item.split_once('='))
                                .context("expected --input name=VALUE")?;
                            if request.inputs.insert(name.into(), value.into()).is_some() { bail!("duplicate input {name:?}"); }
                            at += 1;
                        }
                        self.execute(&request, response)?;
                    }
                    _ => {
                        let cwd = self.cwd.clone();
                        let (output, status) = self.applet(command, &words[1..], &[], &cwd)?;
                        response.stdout = String::from_utf8_lossy(&output).into_owned();
                        if status != 0 { bail!("{command} exited with status {status}"); }
                    }
                }
            }
            "run" | "check" | "explain" => self.execute(&request, response)?,
            _ => bail!("unknown browser request action {:?}", request.action),
        }
        Ok(())
    }

    fn execute(&mut self, request: &Request, response: &mut Response) -> Result<()> {
        if request.source.len() > 64 * 1024 {
            bail!("source exceeds 64 KiB browser limit");
        }
        let program = crate::parser::parse(&request.source)?;
        if program.steps.len() > 4096 {
            bail!("program exceeds 4096 expanded steps");
        }
        let supplied = crate::inputs::parse_cli(&program.inputs, &request.inputs)?;
        validate_with_arguments(&program, &self.registry, &supplied)?;
        let known = crate::inputs::known_values(&program.inputs, &supplied)?;
        preflight_inputs_with_loop_limit(
            &program,
            &self.registry,
            &known,
            loops::MAX_LOOP_ITERATIONS,
        )?;
        for step in &program.steps {
            let unsupported = match &step.kind {
                StepKind::BreakLoop | StepKind::ContinueLoop | StepKind::Return => {
                    Some("loop transfers and reusable-program return")
                }
                StepKind::Append { .. }
                | StepKind::Redirect { .. }
                | StepKind::Environment { .. } => Some("file redirection and append"),
                StepKind::Interactive => Some("terminal handoff (interactively)"),
                StepKind::HandOver => Some("hand over to (replaces the script)"),
                StepKind::LaunchDetached { .. } => Some("launch detached"),
                StepKind::TempScope { .. } | StepKind::EndTempScope { .. } => {
                    Some("temporary file and directory scopes")
                }
                StepKind::OnInterrupt { .. } => Some("on interrupt drain"),
                StepKind::CaptureBytes { .. } | StepKind::UseBytes { .. } => Some("byte captures"),
                StepKind::Exit { .. } | StepKind::Fail { .. } => {
                    Some("exit with status and fail with message")
                }
                StepKind::IfCommand { .. }
                | StepKind::Loop {
                    kind: crate::ast::LoopKind::WhileCommand { .. },
                    ..
                } => Some("command conditions"),
                StepKind::Try { .. }
                | StepKind::Catch { .. }
                | StepKind::Finally
                | StepKind::EndTry => Some("try/catch/finally"),
                StepKind::TakeJsonDynamic { .. } => Some("dynamic JSON pointers"),
                StepKind::SpawnJob { .. }
                | StepKind::InspectJob { .. }
                | StepKind::AwaitJob { .. }
                | StepKind::CancelJob { .. } => Some("native background jobs"),
                StepKind::Invoke { call, .. } if call.result_binding.is_some() => {
                    Some("source-level execution result capture")
                }
                _ => None,
            };
            if let Some(feature) = unsupported {
                bail!(
                    "line {}: browser host does not support {feature}; use native execute",
                    step.line
                );
            }
            if let StepKind::Invoke { call, .. } = &step.kind {
                if core::supports(call) && call.cache {
                    bail!(
                        "line {}: browser data applets do not support cache",
                        step.line
                    );
                }
                let manifest = self.registry.resolve_call(call)?;
                let binary = manifest.binary_for(call);
                if call.direct && binary == "cd" {
                    bail!(
                        "line {}: cd is an interpreter builtin; use run cd with path",
                        step.line
                    );
                }
                if !APPLETS.contains(&binary) && !core::supports(call) {
                    bail!(
                        "line {}: {binary:?} is unavailable in the browser; use native execute",
                        step.line
                    );
                }
            }
        }
        if request.action == "check" {
            response.stdout = "Program checks passed (browser host).\n".into();
            return Ok(());
        }
        if request.action == "explain" {
            response.stdout = explain_with_inputs(&program, &self.registry, &supplied)? + "\n";
            return Ok(());
        }
        let mut bindings = crate::inputs::resolve(&program.inputs, &supplied, false)?;
        let mut cwd = PathBuf::from(&self.cwd);
        let mut frames: Vec<calls::Frame<'_>> = Vec::new();
        let mut loops = loops::LoopStack::default();
        let mut data = Vec::new();
        let mut stdin = Some(request.stdin.as_bytes().to_vec());
        let mut index = 0;
        let mut program_stdout = true;
        while index < program.steps.len() {
            let step = &program.steps[index];
            let mut jump = None;
            let execution = (|| -> Result<()> {
                match &step.kind {
                    StepKind::Loop { kind, end } => {
                        if !loops.header_with_commands(
                            index,
                            kind,
                            &mut bindings,
                            &mut |_, _, _| {
                                bail!("command conditions are not supported in browser execution")
                            },
                        )? {
                            jump = Some(index + end);
                        }
                    }
                    StepKind::EndLoop { back } => {
                        let header = index
                            .checked_sub(*back)
                            .context("invalid loop back offset")?;
                        loops.end_iteration(header, &mut bindings)?;
                        jump = Some(header);
                    }
                    StepKind::If {
                        condition,
                        otherwise,
                        ..
                    } => {
                        let matched = control::evaluate(condition, &bindings)?;
                        response.push_trace(|| serde_json::json!({"line":step.line,"operation":"if","matched":matched}));
                        if !matched {
                            jump = Some(index + otherwise);
                        }
                    }
                    StepKind::Otherwise { end } => jump = Some(index + end),
                    StepKind::OnInterrupt { .. } => {
                        bail!("on interrupt is not supported in browser execution")
                    }
                    StepKind::CaptureBytes { .. } | StepKind::UseBytes { .. } => {
                        bail!("byte captures are not supported in browser execution")
                    }
                    StepKind::EndIf => {}
                    StepKind::BreakLoop
                    | StepKind::ContinueLoop
                    | StepKind::Return
                    | StepKind::Try { .. }
                    | StepKind::Catch { .. }
                    | StepKind::Finally
                    | StepKind::EndTry
                    | StepKind::Exit { .. }
                    | StepKind::Fail { .. }
                    | StepKind::IfCommand { .. }
                    | StepKind::Append { .. }
                    | StepKind::Redirect { .. }
                    | StepKind::Environment { .. }
                    | StepKind::Interactive
                    | StepKind::HandOver
                    | StepKind::LaunchDetached { .. }
                    | StepKind::TempScope { .. }
                    | StepKind::EndTempScope { .. }
                    | StepKind::TakeJsonDynamic { .. } => {
                        bail!("control flow feature was not validated for browser execution");
                    }
                    StepKind::CallProgram {
                        call,
                        inputs,
                        connection,
                        ..
                    } => {
                        let child = calls::bind(call, inputs, &bindings)?;
                        frames.push(calls::Frame {
                            name: &call.operation,
                            line: step.line,
                            binding: call.binding.as_deref(),
                            bindings: std::mem::replace(&mut bindings, child),
                            cwd: crate::working_dir::WorkingDirectory::plain(cwd.clone()),
                        });
                        if *connection == Connection::Start {
                            data.clear();
                        }
                    }
                    StepKind::EndCall => {
                        let frame = frames.pop().context("program return without call")?;
                        if frame.binding.is_some() {
                            program_stdout = false;
                        }
                        bindings = frame.bindings;
                        cwd = frame.cwd.path().to_path_buf();
                        bind_text(frame.binding, &data, &mut bindings)?;
                        response.push_trace(|| serde_json::json!({"line":frame.line,"operation":"return","program":frame.name}));
                    }
                    StepKind::Read { path, binding } => {
                        let path = path_value(path, &bindings, &cwd)?;
                        data = self.read(&path.to_string_lossy(), &cwd.to_string_lossy())?;
                        bind_text(binding.as_deref(), &data, &mut bindings)?;
                    }
                    StepKind::ReadStdin { binding } => {
                        data = stdin.take().unwrap_or_default();
                        bind_text(binding.as_deref(), &data, &mut bindings)?;
                    }
                    StepKind::ReadCallInput { binding } => {
                        bind_text(binding.as_deref(), &data, &mut bindings)?
                    }
                    StepKind::UseValue { source } => {
                        data = text_value_bytes(source, &bindings, step.line)?;
                    }
                    StepKind::BindValue { source, binding } => {
                        let value = resolve(source, &bindings)?;
                        data = bound_value_bytes(&value, LIMIT)?;
                        bindings.insert(binding.clone(), value);
                    }
                    StepKind::UseFolder { path } => {
                        let next = path_value(path, &bindings, &cwd)?;
                        let next = absolute(&next.to_string_lossy(), &cwd.to_string_lossy())?;
                        if !self.directories.contains(&next) {
                            bail!("directory does not exist: {next}");
                        }
                        cwd = PathBuf::from(next);
                    }
                    StepKind::ParseJson { binding } => {
                        bindings.insert(binding.clone(), crate::structured::parse(&data)?);
                    }
                    StepKind::TakeJson {
                        pointer,
                        source,
                        kind,
                        binding,
                    } => {
                        let source = structured_source(source, &bindings)?;
                        let value =
                            crate::structured::take(pointer, &source, *kind, step.line)?.clone();
                        bindings.insert(binding.clone(), value);
                    }
                    StepKind::EncodeJson { source } => {
                        data = crate::structured::encode(
                            structured_source(source, &bindings)?.as_ref(),
                            LIMIT,
                        )?;
                    }
                    StepKind::Save { path, .. } => {
                        let path = path_value(path, &bindings, &cwd)?;
                        let path = absolute(&path.to_string_lossy(), &cwd.to_string_lossy())?;
                        self.write(&path, data.clone())?;
                        response.written_files.insert(path);
                    }
                    StepKind::TaskGroup { .. } => {
                        bail!("browser host does not support task groups");
                    }
                    StepKind::SpawnJob { .. }
                    | StepKind::InspectJob { .. }
                    | StepKind::AwaitJob { .. }
                    | StepKind::CancelJob { .. } => {
                        bail!("browser host does not support native background jobs");
                    }
                    StepKind::Invoke { call, connection } => {
                        let manifest = self.registry.resolve_call(call)?.clone();
                        let argv = arguments_for_call(call, &manifest, &bindings)?;
                        let binary = manifest.binary_for(call).to_string();
                        if manifest.builtin == Some(Builtin::Cd) {
                            let next = absolute(&argv[0], &cwd.to_string_lossy())?;
                            if !self.directories.contains(&next) {
                                bail!("directory does not exist: {next}");
                            }
                            cwd = PathBuf::from(next);
                            data.clear();
                            return Ok(());
                        }
                        if *connection == Connection::Start || !manifest.accepts_stdin {
                            data.clear();
                        }
                        let input_bytes = data.len();
                        // Include the complete virtual filesystem: cat/head file arguments cannot return stale hits.
                        let key = if call.cache {
                            let mut hash = Sha256::new();
                            let mut field = |bytes: &[u8]| {
                                hash.update((bytes.len() as u64).to_le_bytes());
                                hash.update(bytes);
                            };
                            field(&serde_json::to_vec(&(&binary, &argv, &cwd))?);
                            field(&data);
                            for (path, bytes) in &self.files {
                                field(path.as_bytes());
                                field(bytes);
                            }
                            Some(format!("{:x}", hash.finalize()))
                        } else {
                            None
                        };
                        let cached = key.as_ref().and_then(|key| self.cache.get(key)).cloned();
                        let hit = cached.is_some();
                        let mut typed_value = None;
                        let (output, status) = match cached {
                            Some(bytes) => {
                                response.cache_hits += 1;
                                read_cached_output(&manifest, &bytes)?
                            }
                            None => {
                                response.applet_calls += 1;
                                if core::supports(call) {
                                    let value = core::execute(call, &bindings, &data)?;
                                    let mut bytes = bound_value_bytes(&value, LIMIT)?;
                                    if !matches!(value, Value::String(_)) {
                                        bytes.push(b'\n');
                                    }
                                    typed_value = Some(value);
                                    (bytes, 0)
                                } else {
                                    self.applet(&binary, &argv, &data, &cwd.to_string_lossy())?
                                }
                            }
                        };
                        let outcome = manifest.outcome_for(status);
                        if outcome.kind != OutcomeKind::Success && call.status_binding.is_none() {
                            bail!(
                                "applet {:?} produced {:?} outcome (status {status})",
                                manifest.name_for(call),
                                outcome.name
                            );
                        }
                        if let Some(name) = &call.status_binding {
                            bindings.insert(name.clone(), Value::Integer(i64::from(status)));
                        }
                        if call.cache && !hit && outcome.kind == OutcomeKind::Success {
                            let entry = cached_output(&manifest, &output, status);
                            if self.cache.len() >= 256
                                || self.cache.values().map(Vec::len).sum::<usize>() + entry.len()
                                    > 4 * LIMIT
                            {
                                self.cache.clear();
                            }
                            self.cache.insert(key.expect("cache requested"), entry);
                        }
                        data = output;
                        if let (Some(name), Some(value)) = (&call.binding, typed_value) {
                            bindings.insert(name.clone(), value);
                        } else {
                            bind_applet_output(call, &manifest, &data, status, &mut bindings)?;
                        }
                        let output_bytes = data.len();
                        if !call.cache
                            && call.program_stdout_unbound()
                            && !crate::ast::has_stdout_consumer(&program.steps, index + 1)
                        {
                            response.append_stdout(&data)?;
                            data.clear();
                        }
                        response.push_trace(|| serde_json::json!({"line":step.line,"operation":binary,"argv":argv,"input_bytes":input_bytes,"output_bytes":output_bytes,"cache_hit":hit,"status":status}));
                    }
                }
                if data.len() > LIMIT {
                    bail!("output exceeds 1 MiB browser limit");
                }
                Ok(())
            })();
            execution.with_context(|| format!("line {}", step.line))?;
            if let Some(eligible) = step.kind.program_stdout() {
                program_stdout = eligible;
            }
            index = jump.unwrap_or(index + 1);
        }
        if program_stdout {
            response.append_stdout(&data)?;
        }
        // Workflow cwd is local, matching native execute. Terminal cd persists separately.
        Ok(())
    }

    fn write(&mut self, path: &str, bytes: Vec<u8>) -> Result<()> {
        if self.directories.contains(path) {
            bail!("cannot overwrite a directory: {path}");
        }
        let parent = path
            .rsplit_once('/')
            .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
            .unwrap_or("/");
        if !self.directories.contains(parent) {
            bail!("parent directory does not exist: {parent}");
        }
        let total = self.files.values().map(Vec::len).sum::<usize>()
            - self.files.get(path).map_or(0, Vec::len)
            + bytes.len();
        if total > 4 * LIMIT
            || bytes.len() > LIMIT
            || (self.files.len() >= 128 && !self.files.contains_key(path))
        {
            bail!("virtual filesystem limit: 128 files, 1 MiB each, 4 MiB total");
        }
        self.files.insert(path.into(), bytes);
        Ok(())
    }

    fn read(&self, path: &str, cwd: &str) -> Result<Vec<u8>> {
        let path = absolute(path, cwd)?;
        self.files
            .get(&path)
            .cloned()
            .ok_or_else(|| anyhow!("file does not exist: {path}"))
    }
}

fn bind_text(
    name: Option<&str>,
    bytes: &[u8],
    bindings: &mut BTreeMap<String, Value>,
) -> Result<()> {
    if let Some(name) = name {
        bindings.insert(
            name.into(),
            Value::String(
                String::from_utf8(bytes.to_vec()).context("named results must be UTF-8 text")?,
            ),
        );
    }
    Ok(())
}

fn read_cached_output(manifest: &Manifest, entry: &[u8]) -> Result<(Vec<u8>, i32)> {
    if manifest.execution_contract().is_none() {
        return Ok((entry.to_vec(), 0));
    }
    let header = VERSIONED_CACHE_MAGIC.len() + std::mem::size_of::<i32>();
    if entry.len() < header || !entry.starts_with(VERSIONED_CACHE_MAGIC) {
        bail!("cached versioned applet result has an invalid header");
    }
    let status_at = VERSIONED_CACHE_MAGIC.len();
    let status = i32::from_le_bytes(
        entry[status_at..status_at + 4]
            .try_into()
            .expect("fixed cache status width"),
    );
    if manifest.outcome_for(status).kind != OutcomeKind::Success {
        bail!("cached versioned applet result has a non-success status {status}");
    }
    Ok((entry[header..].to_vec(), status))
}

fn cached_output(manifest: &Manifest, output: &[u8], status: i32) -> Vec<u8> {
    if manifest.execution_contract().is_none() {
        return output.to_vec();
    }
    let mut entry = Vec::with_capacity(VERSIONED_CACHE_MAGIC.len() + 4 + output.len());
    entry.extend_from_slice(VERSIONED_CACHE_MAGIC);
    entry.extend_from_slice(&status.to_le_bytes());
    entry.extend_from_slice(output);
    entry
}

fn bind_applet_output(
    call: &Invocation,
    manifest: &Manifest,
    bytes: &[u8],
    status: i32,
    bindings: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let Some(name) = call.binding.as_deref() else {
        return Ok(());
    };
    if manifest.execution_contract().is_none() {
        return bind_text(Some(name), bytes, bindings);
    }
    let value = match manifest.decode_output(bytes, status)? {
        AppletOutput::Value(value) => value,
        AppletOutput::Bytes(bytes) => {
            Value::String(String::from_utf8(bytes).context("named results must be UTF-8 text")?)
        }
    };
    bindings.insert(name.to_owned(), value);
    Ok(())
}

fn absolute(path: &str, cwd: &str) -> Result<String> {
    if path.is_empty() || path.contains('\0') || path.len() > 4096 {
        bail!("invalid virtual path");
    }
    let full = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{cwd}/{path}")
    };
    let mut parts = Vec::new();
    for part in full.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("path escapes virtual root");
                }
            }
            part => parts.push(part),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

/// Quoted literal arguments only. This is a launcher, not a second shell language.
fn command_words(source: &str) -> Result<Vec<String>> {
    if source.len() > 64 * 1024 {
        bail!("command exceeds 64 KiB");
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = source.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if quote != Some('\'') => {
                word.push(chars.next().context("unfinished escape")?);
                started = true;
            }
            '\'' | '"' if quote == Some(ch) => quote = None,
            '\'' | '"' if quote.is_none() => {
                quote = Some(ch);
                started = true;
            }
            ch if ch.is_whitespace() && quote.is_none() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            ch => {
                word.push(ch);
                started = true;
            }
        }
    }
    if quote.is_some() {
        bail!("unterminated quote");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[path = "browser/applets.rs"]
mod applets;
#[path = "browser/core.rs"]
mod core;
