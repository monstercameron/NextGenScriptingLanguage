// Included only on Windows; the parser, validation, cache, branches, JSON,
// same-file calls and checkpoint state machine remain shared with Unix.
use crate::platform::windows::{available, Job};
use std::os::windows::io::AsRawHandle;

struct ManagedChild {
    child: Child,
    job: Job,
    reaped: bool,
    stderr: Option<std::process::ChildStderr>,
    diagnostic: Vec<u8>,
}
#[derive(Default)]
struct Children(Vec<ManagedChild>);
impl Drop for Children {
    fn drop(&mut self) {
        for child in &self.0 {
            child.job.terminate();
        }
        for child in &mut self.0 {
            if !child.reaped {
                let _ = child.child.wait();
            }
        }
    }
}
fn drain_diagnostic(child: &mut ManagedChild) -> Result<()> {
    if let Some(reader) = child.stderr.as_mut() {
        let mut buffer = [0u8; 8192];
        loop {
            match available(reader.as_raw_handle())? {
                None => {
                    child.stderr = None;
                    break;
                }
                Some(0) => break,
                Some(count) => {
                    let count = reader.read(&mut buffer[..count.min(8192)])?;
                    let keep = count.min(65536usize.saturating_sub(child.diagnostic.len()));
                    child.diagnostic.extend_from_slice(&buffer[..keep]);
                    if count == 0 {
                        child.stderr = None;
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}
fn windows_statuses(
    children: &mut Children,
    calls: &[(&Invocation, &Manifest, usize)],
    options: &RunOptions,
    signals: &Signals,
    last_status: &mut Option<i32>,
    started: Instant,
) -> Result<()> {
    for (index, managed) in children.0.iter_mut().enumerate() {
        drain_diagnostic(managed)?;
        if !managed.reaped {
            if let Some(status) = managed.child.try_wait()? {
                managed.reaped = true;
                managed.job.terminate();
                if index + 1 == calls.len() {
                    *last_status = status.code();
                }
                lazy_event(
                    options.trace,
                    signals,
                    || serde_json::json!({"event":"exit","line":calls[index].2,"applet":calls[index].1.name_for(calls[index].0),"status":status.code(),"group_elapsed_us":started.elapsed().as_micros()}),
                )?;
                if !status.success() && calls[index].0.status_binding.is_none() {
                    drain_diagnostic(managed)?;
                    return Err(CommandFailed {
                        line: calls[index].2,
                        operation: calls[index].1.name_for(calls[index].0).to_owned(),
                        status,
                        diagnostic: String::from_utf8_lossy(&managed.diagnostic)
                            .trim()
                            .to_owned(),
                    }
                    .into());
                }
            }
        }
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn execute_group_with_sink(
    calls: &[(&Invocation, &Manifest, usize)],
    input: Input,
    mut prepared_first_call: Option<(PathBuf, Vec<String>)>,
    bindings: &BTreeMap<String, Value>,
    cwd: &Path,
    options: &RunOptions,
    launched: &mut usize,
    signals: &Signals,
    mut sink: Option<&mut streaming::LineSink<'_>>,
) -> Result<GroupOutput> {
    let mut children = Children(Vec::with_capacity(calls.len()));
    let mut input = Some(input);
    let mut previous = None;
    let mut writer = None;
    let started = Instant::now();
    let bounded = Signals {
        cancelled: signals.cancelled.clone(),
        registrations: Vec::new(),
        capture_diagnostics: signals.capture_diagnostics,
        deadline: options
            .command_timeout
            .map(|d| {
                started
                    .checked_add(d)
                    .context("command timeout is too large")
            })
            .transpose()?,
    };
    let signals = &bounded;
    let result = (|| -> Result<GroupOutput> {
        for (index, (call, manifest, line)) in calls.iter().enumerate() {
            check_deadline(signals.deadline)?;
            if signals.cancelled.load(Ordering::Relaxed) {
                bail!("workflow cancelled by signal");
            }
            let (binary, argv) = match prepared_first_call.take() {
                Some((path, argv)) => (path, GroupArguments::Owned(argv)),
                None => (
                    binary_path(manifest.binary_for(call), cwd)?,
                    arguments_for_group_call(call, manifest, bindings)?,
                ),
            };
            let mut command = Command::new(&binary);
            match argv {
                GroupArguments::Owned(argv) => {
                    command.args(argv);
                }
                GroupArguments::Borrowed(argv) => {
                    command.args(argv.iter().map(|v| v.as_ref()));
                }
            }
            command.current_dir(cwd).stdout(Stdio::piped()).stderr(
                if signals.capture_diagnostics {
                    Stdio::piped()
                } else {
                    Stdio::inherit()
                },
            );
            let mut bytes = None;
            let stdin = if index > 0 {
                Stdio::from(previous.take().expect("prior native stdout"))
            } else {
                match input.take().expect("initial input") {
                    Input::File(file) => Stdio::from(file),
                    Input::Stdin => Stdio::inherit(),
                    Input::Bytes(data) if !data.is_empty() => {
                        bytes = Some(data);
                        Stdio::piped()
                    }
                    _ => Stdio::null(),
                }
            };
            command.stdin(stdin);
            let (mut child, job) = Job::spawn(&mut command)
                .with_context(|| format!("line {line}: spawning {}", binary.display()))?;
            let pid = child.id();
            if let Some(data) = bytes {
                let mut target = child.stdin.take().expect("native stdin");
                writer = Some(std::thread::spawn(move || -> std::io::Result<()> {
                    match target.write_all(&data) {
                        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                        other => other,
                    }
                }));
            }
            previous = child.stdout.take();
            let stderr = child.stderr.take();
            children.0.push(ManagedChild {
                child,
                job,
                reaped: false,
                stderr,
                diagnostic: Vec::new(),
            });
            *launched += 1;
            lazy_event(
                options.trace,
                signals,
                || serde_json::json!({"event":"spawn","line":line,"applet":manifest.name_for(call),"binary":binary,"pid":pid}),
            )?;
        }
        let mut stdout = previous.take().expect("nonempty pipeline");
        let mut output = Vec::new();
        let mut chunk = [0u8; 32768];
        let mut eof = false;
        let mut status = None;
        loop {
            check_deadline(signals.deadline)?;
            if signals.cancelled.load(Ordering::Relaxed) {
                bail!("workflow cancelled by signal");
            }
            windows_statuses(&mut children, calls, options, signals, &mut status, started)?;
            if !eof {
                match available(stdout.as_raw_handle())? {
                    None => eof = true,
                    Some(0) => {}
                    Some(count) => {
                        let length = chunk.len().min(count);
                        let count = stdout.read(&mut chunk[..length])?;
                        if count == 0 {
                            eof = true;
                        } else {
                            if let Some(sink) = sink.as_deref_mut() {
                                if sink.push(&chunk[..count], signals)? {
                                    windows_statuses(
                                        &mut children,
                                        calls,
                                        options,
                                        signals,
                                        &mut status,
                                        started,
                                    )?;
                                    return Ok(GroupOutput {
                                        bytes: output,
                                        status,
                                    });
                                }
                            } else {
                                if count > options.max_output_bytes.saturating_sub(output.len()) {
                                    bail!(
                                        "output exceeds maximum of {} bytes",
                                        options.max_output_bytes
                                    );
                                }
                                output.extend_from_slice(&chunk[..count]);
                            }
                            continue;
                        }
                    }
                }
            }
            if eof && children.0.iter().all(|c| c.reaped) {
                if let Some(sink) = sink.as_deref_mut() {
                    sink.finish(signals)?;
                }
                return Ok(GroupOutput {
                    bytes: output,
                    status,
                });
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    // Kill jobs before joining a potentially blocked stdin writer. All inherited
    // pipe readers belong to these jobs, so cancellation releases the writer.
    drop(children);
    if let Some(writer) = writer {
        let write_result = writer
            .join()
            .map_err(|_| anyhow!("input writer panicked"))?;
        if result.is_ok() {
            write_result.context("feeding native stdin")?;
        }
    }
    result
}

/// Native CLI delegation retains stdio and an owned job rather than orphaning
/// a model request if its Windows parent is cancelled.
pub fn model_cli(arguments: &[String]) -> Result<i32> {
    let signals = Signals::register()?;
    let mut command = Command::new(bundled_model_applet()?);
    command
        .args(arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let (mut child, job) = Job::spawn(&mut command)?;
    let result = (|| -> Result<i32> {
        loop {
            if signals.cancelled.load(Ordering::Relaxed) {
                bail!("model request cancelled; its remote outcome may be uncertain");
            }
            if let Some(status) = child.try_wait()? {
                return Ok(status.code().unwrap_or(1));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    job.terminate();
    let _ = child.wait();
    result
}
