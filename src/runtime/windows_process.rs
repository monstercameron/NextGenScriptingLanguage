// Included only on Windows; the parser, validation, cache, branches, JSON,
// same-file calls and checkpoint state machine remain shared with Unix.
use crate::platform::windows::{available, Job};
use std::os::windows::io::AsHandle;

struct ManagedChild {
    child: Child,
    job: Job,
    reaped: bool,
    stderr: Option<std::process::ChildStderr>,
    diagnostic: Vec<u8>,
    diagnostic_truncated: bool,
}
#[derive(Default)]
struct Children(Vec<ManagedChild>, Option<Arc<Mutex<Vec<bool>>>>);
impl Drop for Children {
    fn drop(&mut self) {
        if let Some(target) = &self.1 {
            if let Ok(mut target) = target.lock() {
                for (target, child) in target.iter_mut().zip(&self.0) {
                    *target = child.diagnostic_truncated;
                }
            }
        }
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
fn drain_diagnostic(child: &mut ManagedChild, signals: &Signals) -> Result<()> {
    if let Some(reader) = child.stderr.as_mut() {
        let mut buffer = [0u8; 8192];
        // A continuously writable stderr must yield to stdout, status,
        // cancellation and deadline checks in the supervising loop.
        for _ in 0..16 {
            match available(reader.as_handle())? {
                None => {
                    child.stderr = None;
                    break;
                }
                Some(0) => break,
                Some(count) => {
                    let count = reader.read(&mut buffer[..count.min(8192)])?;
                    let limit = if FORCE_STDERR_CAPTURE.with(std::cell::Cell::get) {
                        FORCED_STDERR_CAPTURE_LIMIT
                    } else {
                        MAX_DIAGNOSTIC_BYTES_PER_STAGE
                    };
                    let remaining = limit.saturating_sub(child.diagnostic.len());
                    let keep = count.min(remaining);
                    if let Some((budget, limit)) = &signals.group_diagnostic_budget {
                        reserve_group_capture(budget, *limit, keep)?;
                    }
                    child.diagnostic.extend_from_slice(&buffer[..keep]);
                    child.diagnostic_truncated |= keep < count;
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
/// Reads only what a pipe already holds, so draining a failed command's stdout never
/// waits for a writer that is still alive.
struct AvailableStdout<'a>(&'a mut std::process::ChildStdout);

impl Read for AvailableStdout<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let available = available(self.0.as_handle())
            .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
        match available {
            None | Some(0) => Ok(0),
            Some(count) => {
                let length = buffer.len().min(count);
                self.0.read(&mut buffer[..length])
            }
        }
    }
}

fn windows_statuses(
    children: &mut Children,
    calls: &[(&Invocation, &Manifest, usize)],
    observed_stages: &mut [Option<execution_result::StageResult>],
    options: &RunOptions,
    signals: &Signals,
    last_status: &mut Option<i32>,
    started: Instant,
) -> Result<()> {
    for (index, managed) in children.0.iter_mut().enumerate() {
        drain_diagnostic(managed, signals)?;
        if !managed.reaped {
            if let Some(status) = managed.child.try_wait()? {
                managed.reaped = true;
                managed.job.terminate();
                drain_exited_diagnostic(managed, signals)?;
                let code = status.code();
                if index + 1 == calls.len() {
                    *last_status = code.map(|code| {
                        if code == (13 << 8) && INHERIT_TERMINAL_STDOUT.with(|flag| flag.get()) {
                            141
                        } else {
                            code
                        }
                    });
                }
                let stderr = captured_stage_stderr(calls[index].0, calls[index].1, signals)
                    .then(|| managed.diagnostic.clone());
                observed_stages[index] = Some(if let Some(code) = code {
                    execution_result::StageResult::exited(
                        calls[index].2,
                        calls[index].1.name_for(calls[index].0),
                        code,
                        calls[index].1.outcome_for(code),
                        stderr,
                    )
                } else {
                    execution_result::StageResult::signaled(
                        calls[index].2,
                        calls[index].1.name_for(calls[index].0),
                        execution_result::exit_signal(status),
                        stderr,
                    )
                });
                signals.record_job_stage_result(
                    index,
                    observed_stages[index]
                        .as_ref()
                        .expect("stage result was just set"),
                );
                lazy_event(
                    options.trace,
                    signals,
                    || serde_json::json!({"event":"exit","line":calls[index].2,"applet":calls[index].1.name_for(calls[index].0),"status":status.code(),"group_elapsed_us":started.elapsed().as_micros()}),
                )?;
                let implicit_success = code.is_some_and(|code| {
                    calls[index].1.outcome_for(code).kind == crate::contracts::OutcomeKind::Success
                });
                let broken_pipe = code == Some(13 << 8);
                let inherited_reader_closed =
                    index + 1 == calls.len() && INHERIT_TERMINAL_STDOUT.with(|flag| flag.get());
                if broken_pipe && (index + 1 < calls.len() || inherited_reader_closed) {
                    continue;
                }
                if !(implicit_success || calls[index].0.status_binding.is_some() && code.is_some())
                {
                    drain_diagnostic(managed, signals)?;
                    return Err(CommandFailed {
                        line: calls[index].2,
                        operation: calls[index].1.name_for(calls[index].0).to_owned(),
                        status,
                        outcome: code.map(|code| calls[index].1.outcome_for(code)),
                        diagnostic: String::from_utf8_lossy(&managed.diagnostic)
                            .trim()
                            .to_owned(),
                        stderr: captured_stage_stderr(calls[index].0, calls[index].1, signals)
                            .then(|| managed.diagnostic.clone()),
                        stdout: None,
                        stages: collect_native_stages(calls, observed_stages, children, signals),
                    }
                    .into());
                }
            }
        }
    }
    Ok(())
}
fn execute_group_with_sink(
    request: GroupRequest<'_>,
    mut sink: Option<&mut streaming::LineSink<'_>>,
) -> Result<GroupOutput> {
    let GroupRequest {
        registry,
        calls,
        input,
        mut prepared_first_call,
        bindings,
        cwd,
        options,
        launched,
        signals,
    } = request;
    let launch_cwd = cwd;
    let cwd: &Path = cwd;
    let mut children = Children(
        Vec::with_capacity(calls.len()),
        signals.job_diagnostic_truncation.clone(),
    );
    let mut observed_stages = vec![None; calls.len()];
    let mut input = Some(input);
    let mut previous = None;
    let mut writer = None;
    let started = Instant::now();
    let background_job = signals.job_launch_ack.is_some();
    let mut pending_spawn_events = Vec::new();
    let bounded = Signals {
        cancelled: signals.cancelled.clone(),
        drain: Arc::clone(&signals.drain),
        drain_stop: None,
        registrations: Vec::new(),
        capture_diagnostics: signals.capture_diagnostics,
        graph_service_cancellation: signals.graph_service_cancellation,
        background_jobs_allowed: signals.background_jobs_allowed,
        program_modules: Arc::clone(&signals.program_modules),
        sensitivity_floor: Arc::clone(&signals.sensitivity_floor),
        process_launches: Arc::clone(&signals.process_launches),
        process_observer: Arc::clone(&signals.process_observer),
        worker_pool: signals.worker_pool.clone(),
        outer_cancelled: signals.outer_cancelled.clone(),
        cancel_wakes: signals.cancel_wakes.clone(),
        unnotified_cancellation: signals.unnotified_cancellation,
        job_cancelled: signals.job_cancelled.clone(),
        job_launch_ack: signals.job_launch_ack.clone(),
        job_output_budget: signals.job_output_budget.clone(),
        job_output_limit: signals.job_output_limit,
        group_diagnostic_budget: signals.group_diagnostic_budget.clone(),
        prepared_job_commands: signals.prepared_job_commands.clone(),
        job_diagnostic_truncation: signals.job_diagnostic_truncation.clone(),
        job_stage_results: signals.job_stage_results.clone(),
        job_run_context: Arc::clone(&signals.job_run_context),
        output_writers: Arc::clone(&signals.output_writers),
        #[cfg(test)]
        job_panic_before_ack: signals.job_panic_before_ack.clone(),
        deadline: {
            let command_deadline = options
                .command_timeout
                .map(|d| {
                    started
                        .checked_add(d)
                        .context("command timeout is too large")
                })
                .transpose()?;
            match (signals.deadline, command_deadline) {
                (Some(total), Some(command)) => Some(total.min(command)),
                (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
                (None, None) => None,
            }
        },
    };
    let signals = &bounded;
    let result = (|| -> Result<GroupOutput> {
        if calls.is_empty() {
            bail!("cannot execute an empty native command group");
        }
        let sensitivities = signals.sensitivity_floor();
        for (_, manifest, _) in calls {
            crate::security::authorize_declared_filesystem_write(
                registry,
                manifest,
                &sensitivities,
            )?;
        }
        for (index, (_, manifest, line)) in calls.iter().enumerate() {
            if index + 1 != calls.len()
                && manifest.stdio().is_some_and(|stdio| {
                    stdio
                        .stdout
                        .is_some_and(|mode| mode != invocation_io::StdioDisposition::Capture)
                })
            {
                bail!("line {line}: only the final pipeline applet can discard or inherit stdout");
            }
        }
        let terminal_io = invocation_io::ResolvedStdio::resolve(
            calls.last().expect("nonempty group").1.stdio(),
            invocation_io::StdioDisposition::Capture,
            if signals.capture_diagnostics {
                invocation_io::StdioDisposition::Capture
            } else {
                invocation_io::StdioDisposition::Inherit
            },
        );
        if sink.is_some() && !terminal_io.captures_terminal_stdout() {
            bail!("streaming requires the final applet's stdout to be captured");
        }
        let (terminal_call, terminal_manifest, terminal_line) =
            calls.last().expect("nonempty group");
        if !terminal_io.captures_terminal_stdout()
            && (terminal_call.binding.is_some()
                || calls.iter().any(|(call, _, _)| call.cache)
                || terminal_manifest.output_kind().is_some())
        {
            bail!(
                "line {terminal_line}: discarded or inherited stdout cannot be bound, cached, or decoded as a typed output"
            );
        }
        for (index, (call, manifest, line)) in calls.iter().enumerate() {
            check_native_abort(calls, &children, &observed_stages, signals)?;
            let binary;
            let mut command = if let Some(prepared) = &signals.prepared_job_commands {
                let prepared = prepared.get(index).with_context(|| {
                    format!("line {line}: missing preflighted background command")
                })?;
                binary = Cow::Borrowed(prepared.0.as_path());
                spawn::PreparedLaunch::new(prepared).command(
                    registry,
                    &manifest.name,
                    launch_cwd,
                )?
            } else {
                let (resolved_binary, argv) = match prepared_first_call.take() {
                    Some((path, argv)) => (path, GroupArguments::Owned(argv)),
                    None => (
                        binary_path_for_launch(manifest.binary_for(call), cwd, registry).map_err(
                            |error| {
                                native_spawn_failure(
                                    format!(
                                        "line {line}: resolving executable for {}: {error:#}",
                                        manifest.name_for(call)
                                    ),
                                    index,
                                    calls,
                                    &children,
                                    &mut observed_stages,
                                    signals,
                                )
                            },
                        )?,
                        arguments_for_group_call(call, manifest, bindings)?,
                    ),
                };
                binary = Cow::Owned(resolved_binary);
                match &argv {
                    GroupArguments::Owned(argv) => crate::capabilities::command_for(
                        &binary,
                        argv.iter().map(|argument| OsStr::new(argument.as_str())),
                        registry,
                        &manifest.name,
                        launch_cwd,
                    )?,
                    GroupArguments::Borrowed(argv) => crate::capabilities::command_for(
                        &binary,
                        argv.iter().map(|argument| OsStr::new(argument.as_ref())),
                        registry,
                        &manifest.name,
                        launch_cwd,
                    )?,
                }
            };
            let resolved_io = resolved_call_stdio(call, manifest, signals);
            let stdout_disposition = resolved_io.stdout_for_stage(index + 1 == calls.len());
            command
                .current_dir(cwd)
                .stdout(invocation_io::process_stdio(stdout_disposition))
                .stderr(invocation_io::process_stdio(resolved_io.stderr));
            if index + 1 == calls.len() {
                let (stdout, stderr) = redirect::take_redirected_stdio()?;
                if let Some(stdout) = stdout {
                    command.stdout(stdout);
                }
                if let Some(stderr) = stderr {
                    command.stderr(stderr);
                }
            }
            let mut bytes = None;
            let stdin = if index > 0 {
                Stdio::from(previous.take().expect("prior native stdout"))
            } else if terminal::requested() {
                // The command owns the console, so its input is the console.
                Stdio::inherit()
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
            let (mut child, job) = Job::spawn(&mut command).map_err(|error| {
                native_spawn_failure(
                    format!("line {line}: spawning {}: {error:#}", binary.display()),
                    index,
                    calls,
                    &children,
                    &mut observed_stages,
                    signals,
                )
            })?;
            let pid = child.id();
            signals.record_process_launch();
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
                diagnostic_truncated: false,
            });
            signals.record_job_stage_launch(jobs::StageLaunch {
                line: *line,
                applet: manifest.name_for(call).to_owned(),
                pid,
            });
            *launched += 1;
            if background_job {
                if options.trace {
                    pending_spawn_events.push((
                        *line,
                        manifest.name_for(call).to_owned(),
                        binary,
                        pid,
                    ));
                }
            } else {
                lazy_event(
                    options.trace,
                    signals,
                    || serde_json::json!({"event":"spawn","line":line,"applet":manifest.name_for(call),"binary":binary,"pid":pid}),
                )?;
            }
        }
        signals.acknowledge_job_launch(calls, &children)?;
        for (line, applet, binary, pid) in pending_spawn_events {
            lazy_event(
                options.trace,
                signals,
                || serde_json::json!({"event":"spawn","line":line,"applet":applet,"binary":binary,"pid":pid}),
            )?;
        }
        let mut stdout = previous.take();
        let mut output = Vec::new();
        let mut chunk = [0u8; 32768];
        let mut eof = stdout.is_none();
        let mut status = None;
        // A failed command's captured stdout travels with its failure, so a caught
        // failure record shows what it printed, as `keep result` would.
        let attach_stdout = sink.is_none() && terminal_io.captures_terminal_stdout();
        loop {
            check_native_abort(calls, &children, &observed_stages, signals)?;
            windows_statuses(
                &mut children,
                calls,
                &mut observed_stages,
                options,
                signals,
                &mut status,
                started,
            )
            .map_err(|error| {
                let mut pipe = stdout.as_mut().filter(|_| !eof).map(AvailableStdout);
                spawn::attach_failed_stdout(
                    error,
                    spawn::FailedStdout {
                        attach: attach_stdout,
                        pipe: pipe.as_mut(),
                        output: &mut output,
                        limit: options.max_output_bytes,
                        signals,
                    },
                )
            })?;
            if !eof {
                match available(stdout.as_ref().expect("captured stdout pipe").as_handle())? {
                    None => eof = true,
                    Some(0) => {}
                    Some(count) => {
                        let length = chunk.len().min(count);
                        let count = stdout
                            .as_mut()
                            .expect("captured stdout pipe")
                            .read(&mut chunk[..length])?;
                        if count == 0 {
                            eof = true;
                        } else {
                            if let Some(sink) = sink.as_deref_mut() {
                                if sink.push(&chunk[..count], signals)? {
                                    windows_statuses(
                                        &mut children,
                                        calls,
                                        &mut observed_stages,
                                        options,
                                        signals,
                                        &mut status,
                                        started,
                                    )?;
                                    return Ok(GroupOutput {
                                        bytes: output,
                                        typed_json: None,
                                        status,
                                        diagnostic: children
                                            .0
                                            .last()
                                            .map_or_else(Vec::new, |child| {
                                                child.diagnostic.clone()
                                            }),
                                        stdout_captured: false,
                                        stages: collect_native_stages(
                                            calls,
                                            &observed_stages,
                                            &children,
                                            signals,
                                        ),
                                    });
                                }
                            } else {
                                let remaining =
                                    options.max_output_bytes.saturating_sub(output.len());
                                let job_exceeded = count > remaining;
                                let requested = count.min(remaining);
                                let retained = signals.reserve_job_output(requested);
                                output.extend_from_slice(&chunk[..retained]);
                                if retained < count {
                                    let message = if job_exceeded {
                                        crate::runtime::output_limit_message(
                                            false,
                                            options.max_output_bytes,
                                        )
                                    } else {
                                        crate::runtime::output_limit_message(
                                            true,
                                            signals
                                                .job_output_limit
                                                .unwrap_or(options.max_output_bytes),
                                        )
                                    };
                                    return Err(native_output_limit_failure(
                                        message,
                                        output,
                                        calls,
                                        &children,
                                        &observed_stages,
                                        signals,
                                    )
                                    .into());
                                }
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
                    typed_json: None,
                    status,
                    diagnostic: children
                        .0
                        .last()
                        .map_or_else(Vec::new, |child| child.diagnostic.clone()),
                    stdout_captured: terminal_io.captures_terminal_stdout() && sink.is_none(),
                    stages: collect_native_stages(calls, &observed_stages, &children, signals),
                });
            }
            // `available` avoids blocking on an empty pipe. Pipe readiness is
            // picked up within this bounded 10 Hz fallback interval.
            std::thread::sleep(Duration::from_millis(100));
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
    let signals = Signals::register_with_deadline(None)?;
    let mut command = std::process::Command::new(bundled_model_applet()?);
    crate::platform::add_arguments(&mut command, arguments);
    crate::capabilities::apply_host_child_environment(&mut command)?;
    command
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

/// Exit status for a run ended by an interrupt (128 plus the signal number).
pub fn interrupt_exit_code() -> Option<i32> {
    crate::platform::cancel_exit_code()
}

/// Record the signal that interrupted a run, so `main` can exit with 128 plus it.
/// Intended for handlers that only perform atomic stores.
pub fn record_interrupt(signal: i32) {
    crate::platform::record_cancel_signal(signal);
}
