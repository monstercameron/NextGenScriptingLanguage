//! The native command and pipeline execution path: stdio wiring, the spawned children,
//! output capture, and the classification of a finished group into a step result.

#[cfg(windows)]
use super::execute_group_with_sink;
use super::{
    execution_result, invocation_io, terminal, Children, CommandFailed, Input, RunOptions, Signals,
    FORCE_STDERR_CAPTURE, INHERIT_TERMINAL_STDOUT,
};
use crate::ast::{Invocation, Value};
use crate::contracts::AppletBackend;
use crate::manifest::{Manifest, Registry};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

#[derive(Clone)]
pub(super) struct GroupOutput {
    pub(super) bytes: Vec<u8>,
    pub(super) typed_json: Option<serde_json::Value>,
    pub(super) status: Option<i32>,
    pub(super) diagnostic: Vec<u8>,
    pub(super) stdout_captured: bool,
    pub(super) stages: Vec<execution_result::StageResult>,
}

pub(super) fn resolved_call_stdio(
    call: &Invocation,
    manifest: &Manifest,
    signals: &Signals,
) -> invocation_io::ResolvedStdio {
    if terminal::requested() {
        // An interactive command keeps all three streams of the terminal.
        return invocation_io::ResolvedStdio::resolve(
            None,
            invocation_io::StdioDisposition::Inherit,
            invocation_io::StdioDisposition::Inherit,
        )
        .discard_stderr(call.discard_stderr);
    }
    let inherit = INHERIT_TERMINAL_STDOUT.with(|flag| flag.get())
        && manifest.execution_backend() == AppletBackend::Command;
    invocation_io::ResolvedStdio::resolve(
        manifest.stdio(),
        if inherit {
            invocation_io::StdioDisposition::Inherit
        } else {
            invocation_io::StdioDisposition::Capture
        },
        if signals.capture_diagnostics || FORCE_STDERR_CAPTURE.with(|flag| flag.get()) {
            invocation_io::StdioDisposition::Capture
        } else {
            invocation_io::StdioDisposition::Inherit
        },
    )
    .discard_stderr(call.discard_stderr)
}

pub(super) fn captured_stage_stderr(
    call: &Invocation,
    manifest: &Manifest,
    signals: &Signals,
) -> bool {
    resolved_call_stdio(call, manifest, signals).captures_stderr()
}

pub(super) fn collect_native_stages(
    calls: &[(&Invocation, &Manifest, usize)],
    observed: &[Option<execution_result::StageResult>],
    children: &Children,
    signals: &Signals,
) -> Vec<execution_result::StageResult> {
    calls
        .iter()
        .enumerate()
        .map(|(index, (call, manifest, line))| {
            let mut stage = observed
                .get(index)
                .and_then(Clone::clone)
                .unwrap_or_else(|| {
                    if index < children.0.len() {
                        execution_result::StageResult::unknown(*line, manifest.name_for(call))
                    } else {
                        execution_result::StageResult::not_started(*line, manifest.name_for(call))
                    }
                });
            if captured_stage_stderr(call, manifest, signals) {
                stage.stderr = children.0.get(index).map(|child| child.diagnostic.clone());
            }
            stage
        })
        .collect()
}

fn native_execution_failure(
    kind: execution_result::ExecutionKind,
    message: impl Into<String>,
    calls: &[(&Invocation, &Manifest, usize)],
    children: &Children,
    observed: &[Option<execution_result::StageResult>],
    signals: &Signals,
) -> execution_result::ExecutionFailure {
    let stages = collect_native_stages(calls, observed, children, signals);
    let mut result = execution_result::ExecutionResult::runtime_failure(
        kind,
        message,
        if children.0.is_empty() {
            execution_result::EffectCertainty::NotStarted
        } else {
            execution_result::EffectCertainty::Uncertain
        },
    );
    result.stderr = stages.iter().rev().find_map(|stage| stage.stderr.clone());
    result.stages = stages;
    execution_result::ExecutionFailure::new(result)
}

pub(super) fn native_output_limit_failure(
    message: impl Into<String>,
    output: Vec<u8>,
    calls: &[(&Invocation, &Manifest, usize)],
    children: &Children,
    observed: &[Option<execution_result::StageResult>],
    signals: &Signals,
) -> execution_result::ExecutionFailure {
    let mut result = native_execution_failure(
        execution_result::ExecutionKind::OutputLimit,
        message,
        calls,
        children,
        observed,
        signals,
    )
    .result;
    result.stdout = Some(output);
    execution_result::ExecutionFailure::new(result)
}

pub(super) fn check_native_abort(
    calls: &[(&Invocation, &Manifest, usize)],
    children: &Children,
    observed: &[Option<execution_result::StageResult>],
    signals: &Signals,
) -> Result<()> {
    if signals.is_cancelled() {
        let message = if signals
            .job_cancelled
            .as_ref()
            .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
        {
            "background job cancelled by handle"
        } else {
            "workflow cancelled by signal"
        };
        return Err(native_execution_failure(
            execution_result::ExecutionKind::Cancelled,
            message,
            calls,
            children,
            observed,
            signals,
        )
        .into());
    }
    if signals
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Err(native_execution_failure(
            execution_result::ExecutionKind::Timeout,
            "command pipeline timed out; external effects may be incomplete",
            calls,
            children,
            observed,
            signals,
        )
        .into());
    }
    Ok(())
}

pub(super) fn native_spawn_failure(
    message: String,
    index: usize,
    calls: &[(&Invocation, &Manifest, usize)],
    children: &Children,
    observed: &mut [Option<execution_result::StageResult>],
    signals: &Signals,
) -> anyhow::Error {
    let (call, manifest, line) = calls[index];
    observed[index] = Some(execution_result::StageResult::spawn_failed(
        line,
        manifest.name_for(call),
    ));
    native_execution_failure(
        execution_result::ExecutionKind::SpawnError,
        message,
        calls,
        children,
        observed,
        signals,
    )
    .into()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_group(
    registry: &Registry,
    calls: &[(&Invocation, &Manifest, usize)],
    input: Input,
    prepared_first_call: Option<(PathBuf, Vec<String>)>,
    bindings: &BTreeMap<String, Value>,
    cwd: &crate::working_dir::WorkingDirectory,
    options: &RunOptions,
    launched: &mut usize,
    signals: &Signals,
) -> Result<GroupOutput> {
    redact_command_failure(
        registry,
        execute_group_with_sink(
            registry,
            calls,
            input,
            prepared_first_call,
            bindings,
            cwd,
            options,
            launched,
            signals,
            None,
        ),
    )
}

pub(super) fn redact_command_failure<T>(registry: &Registry, mut result: Result<T>) -> Result<T> {
    if let Err(error) = &mut result {
        if let Some(failure) = error.downcast_mut::<CommandFailed>() {
            failure.diagnostic =
                crate::security::secret_redactor(registry).redact_text(&failure.diagnostic);
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
#[cfg(unix)]
pub(super) fn execute_group_with_sink(
    registry: &Registry,
    calls: &[(&Invocation, &Manifest, usize)],
    input: Input,
    prepared_first_call: Option<(PathBuf, Vec<String>)>,
    bindings: &BTreeMap<String, Value>,
    cwd: &crate::working_dir::WorkingDirectory,
    options: &RunOptions,
    launched: &mut usize,
    signals: &Signals,
    mut sink: Option<&mut streaming::LineSink<'_>>,
) -> Result<GroupOutput> {
    let mut writer = InputWriter::default();
    let mut children = Children(
        Vec::with_capacity(calls.len()),
        signals.job_diagnostic_truncation.clone(),
    );
    let mut observed_stages = vec![None; calls.len()];
    let mut input = Some(input);
    let mut prepared_first_call = prepared_first_call;
    let mut previous = None;
    let started = Instant::now();
    let mut child_observer = ChildObserver::new();
    let mut observer_ready = false;
    let background_job = signals.job_launch_ack.is_some();
    let mut pending_spawn_events = Vec::new();
    let bounded_signals = Signals {
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
        job_cancelled: signals.job_cancelled.clone(),
        job_launch_ack: signals.job_launch_ack.clone(),
        job_output_budget: signals.job_output_budget.clone(),
        job_output_limit: signals.job_output_limit,
        prepared_job_commands: signals.prepared_job_commands.clone(),
        job_diagnostic_truncation: signals.job_diagnostic_truncation.clone(),
        job_stage_results: signals.job_stage_results.clone(),
        job_run_context: Arc::clone(&signals.job_run_context),
        #[cfg(test)]
        job_panic_before_ack: signals.job_panic_before_ack.clone(),
        deadline: {
            let command_deadline = options
                .command_timeout
                .map(|duration| {
                    started
                        .checked_add(duration)
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
    let signals = &bounded_signals;
    if calls.is_empty() {
        bail!("cannot execute an empty native command group");
    }
    let sensitivities = signals.sensitivity_floor();
    for (_, manifest, _) in calls {
        crate::security::authorize_declared_filesystem_write(registry, manifest, &sensitivities)?;
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
    let (terminal_call, terminal_manifest, terminal_line) = calls.last().expect("nonempty group");
    if !terminal_io.captures_terminal_stdout()
        && (terminal_call.binding.is_some()
            || calls.iter().any(|(call, _, _)| call.cache)
            || (terminal_manifest.output_kind().is_some()
                && terminal_call.result_binding.is_none()))
    {
        bail!(
            "line {terminal_line}: discarded or inherited stdout cannot be bound, cached, or decoded as a typed output"
        );
    }
    // Assigned for each command before its first use, so it needs no initial value.
    let mut terminal_foreground: Option<TerminalForeground>;
    for (index, (call, manifest, line)) in calls.iter().enumerate() {
        check_native_abort(calls, &children, &observed_stages, signals)?;
        let (binary, argv) = if let Some(prepared) = &signals.prepared_job_commands {
            let (binary, argv) = prepared
                .get(index)
                .with_context(|| format!("line {line}: missing preflighted background command"))?;
            (binary.clone(), GroupArguments::Owned(argv.clone()))
        } else {
            match prepared_first_call.take() {
                Some((binary, argv)) => (binary, GroupArguments::Owned(argv)),
                None => {
                    let argv = arguments_for_group_call(call, manifest, bindings)?;
                    let binary = binary_path(manifest.binary_for(call), cwd).map_err(|error| {
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
                    })?;
                    (binary, argv)
                }
            }
        };
        let mut command = match &argv {
            GroupArguments::Owned(argv) => crate::capabilities::command_for(
                &binary,
                argv.iter().map(|argument| OsStr::new(argument.as_str())),
                registry,
                &manifest.name,
                cwd,
            )?,
            GroupArguments::Borrowed(argv) => crate::capabilities::command_for(
                &binary,
                argv.iter().map(|argument| OsStr::new(argument.as_ref())),
                registry,
                &manifest.name,
                cwd,
            )?,
        };
        let resolved_io = resolved_call_stdio(call, manifest, signals);
        let stdout_disposition = resolved_io.stdout_for_stage(index + 1 == calls.len());
        command
            .stdout(invocation_io::process_stdio(stdout_disposition))
            .stderr(invocation_io::process_stdio(resolved_io.stderr));
        terminal_foreground = terminal::requested()
            .then(|| TerminalForeground::begin(&mut command))
            .flatten();
        if terminal_foreground.is_none() {
            command.process_group(0);
        }
        let mut bytes = None;
        let mut prefilled_input = None;
        let stdin = if index != 0 {
            Stdio::from(previous.take().expect("prior child stdout"))
        } else if terminal::requested() {
            // The command owns the terminal, so its input is the terminal.
            Stdio::inherit()
        } else {
            match input.take().expect("first input") {
                Input::File(file) => Stdio::from(file),
                Input::Stdin => Stdio::inherit(),
                Input::Bytes(data) if !data.is_empty() && data.len() <= libc::PIPE_BUF => {
                    let (reader, mut pipe_writer) =
                        std::io::pipe().context("creating applet stdin pipe")?;
                    pipe_writer
                        .write_all(&data)
                        .context("prefilling applet stdin pipe")?;
                    // Close before spawn so the child observes EOF after the
                    // buffered input without a background writer thread.
                    drop(pipe_writer);
                    Stdio::from(reader)
                }
                Input::Bytes(data)
                    if !data.is_empty() && data.len() <= SMALL_STDIN_PREFILL_LIMIT =>
                {
                    let (reader, remainder) = prefill_stdin_pipe(data)?;
                    prefilled_input = remainder;
                    Stdio::from(reader)
                }
                Input::Bytes(data) if !data.is_empty() => {
                    bytes = Some(data);
                    Stdio::piped()
                }
                _ => Stdio::null(),
            }
        };
        command.stdin(stdin);
        check_native_abort(calls, &children, &observed_stages, signals)?;
        let mut child = command.spawn().map_err(|error| {
            native_spawn_failure(
                format!("line {line}: spawning {}: {error}", binary.display()),
                index,
                calls,
                &children,
                &mut observed_stages,
                signals,
            )
        })?;
        if let Some(foreground) = terminal_foreground.as_ref() {
            if let Err(error) = foreground.give(child.id()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("handing the terminal to an interactive command");
            }
        }
        let observer_failed = child_observer
            .as_ref()
            .and_then(|observer| observer.observe(child.id()).err())
            .is_some_and(|error| error.raw_os_error() != Some(libc::ESRCH));
        if observer_failed {
            child_observer = None;
        }
        signals.record_process_launch();
        *launched += 1;
        let child_stdin = bytes
            .as_ref()
            .map(|_| child.stdin.take().expect("piped child stdin"));
        previous = child.stdout.take();
        let stderr = child.stderr.take();
        children.0.push(ManagedChild {
            child,
            reaped: false,
            stderr,
            diagnostic: Vec::new(),
            diagnostic_truncated: false,
        });
        signals.record_job_stage_launch(jobs::StageLaunch {
            line: *line,
            applet: manifest.name_for(call).to_owned(),
            pid: children.0.last().expect("just pushed child").child.id(),
        });
        if let Some((pipe_writer, data, offset)) = prefilled_input {
            writer.spawn(
                pipe_writer,
                data,
                offset,
                signals.cancelled.clone(),
                signals.deadline,
            );
        } else if let Some(data) = bytes {
            writer.spawn(
                child_stdin.expect("piped child stdin"),
                data,
                0,
                signals.cancelled.clone(),
                signals.deadline,
            );
        }
        if let Some(stderr) = &children.0.last().unwrap().stderr {
            // `stderr` borrows the pipe from `children`, and nothing is removed from `children`
            // before this call, so the descriptor is open.
            crate::platform::unix_ffi::set_nonblocking(stderr.as_raw_fd())
                .context("setting diagnostics nonblocking")?;
        }
        // Parameter values are deliberately omitted from traces (may be secrets).
        let pid = children.0.last().expect("just pushed child").child.id();
        if background_job {
            if options.trace {
                pending_spawn_events.push((line, manifest.name_for(call).to_owned(), binary, pid));
            }
        } else {
            lazy_event(
                options.trace,
                signals,
                || serde_json::json!({"event":"spawn", "line":line,"applet":manifest.name_for(call),"binary":binary,"pid":pid}),
            )?;
        }
    }
    signals.acknowledge_job_launch(calls, &children)?;
    for (line, applet, binary, pid) in pending_spawn_events {
        lazy_event(
            options.trace,
            signals,
            || serde_json::json!({"event":"spawn", "line":line,"applet":applet,"binary":binary,"pid":pid}),
        )?;
    }
    let mut stdout = previous;
    // Only a captured terminal pipe is monitored. Intermediate bytes travel
    // directly between child descriptors; the interpreter never copies them.
    let (fd, mut eof) = if let Some(stdout) = stdout.as_ref() {
        let fd = stdout.as_raw_fd();
        // `fd` is the stdout pipe of the previous stage. It is held in `stdout` for the whole
        // loop, so it is open while the flags change.
        crate::platform::unix_ffi::set_nonblocking(fd)
            .context("setting terminal pipe nonblocking")?;
        (fd, false)
    } else {
        (-1, true)
    };
    let capture_diagnostics = children.0.iter().any(|child| child.stderr.is_some());
    let mut diagnostic_poll_descriptors = Vec::with_capacity(if capture_diagnostics {
        children.0.len() + 2
    } else {
        0
    });
    if capture_diagnostics {
        for _ in 0..children.0.len() + 2 {
            diagnostic_poll_descriptors.push(libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            });
        }
    }
    let mut output = Vec::new();
    let mut chunk = [0u8; 32768];
    loop {
        check_native_abort(calls, &children, &observed_stages, signals)?;
        scan_child_statuses(
            &mut children,
            calls,
            &mut observed_stages,
            options.trace,
            signals,
            &mut child_observer,
            &mut observer_ready,
            started,
        )?;
        let mut reached_eof = false;
        if !eof {
            loop {
                match stdout
                    .as_mut()
                    .expect("captured stdout pipe")
                    .read(&mut chunk)
                {
                    Ok(0) => {
                        eof = true;
                        reached_eof = true;
                        if let Some(sink) = sink.as_deref_mut() {
                            sink.finish(signals)?;
                        }
                        break;
                    }
                    Ok(count) => {
                        if let Some(sink) = sink.as_deref_mut() {
                            if sink.push(&chunk[..count], signals)? {
                                // Account for failures that became observable while
                                // reading or emitting the final requested record.
                                scan_child_statuses(
                                    &mut children,
                                    calls,
                                    &mut observed_stages,
                                    options.trace,
                                    signals,
                                    &mut child_observer,
                                    &mut observer_ready,
                                    started,
                                )?;
                                // The script's output limit is deliberate successful
                                // termination. Children owns kill/reap on this return;
                                // killed upstream stages do not become native failures.
                                return Ok(GroupOutput {
                                    bytes: output,
                                    typed_json: None,
                                    status: None,
                                    diagnostic: children
                                        .0
                                        .last()
                                        .map_or_else(Vec::new, |child| child.diagnostic.clone()),
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
                            let remaining = options.max_output_bytes.saturating_sub(output.len());
                            let job_exceeded = count > remaining;
                            let requested = count.min(remaining);
                            let retained = signals.reserve_job_output(requested);
                            output.extend_from_slice(&chunk[..retained]);
                            if retained < count {
                                let message = if job_exceeded {
                                    output_limit_message(false, options.max_output_bytes)
                                } else {
                                    output_limit_message(
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
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error).context("reading terminal pipe"),
                }
                check_native_abort(calls, &children, &observed_stages, signals)?;
            }
        }
        if reached_eof && children.0.iter().any(|child| !child.reaped) {
            // stdout EOF can precede child exit. Recheck once before falling
            // back to the short no-descriptor poll for cancellation/deadline.
            check_native_abort(calls, &children, &observed_stages, signals)?;
            scan_child_statuses(
                &mut children,
                calls,
                &mut observed_stages,
                options.trace,
                signals,
                &mut child_observer,
                &mut observer_ready,
                started,
            )?;
        }
        if eof && children.0.iter().all(|child| child.reaped) {
            break;
        }
        // Keep a short timeout for external cancellation and observer fallback.
        // Active kqueue observation makes a 20 ms EOF fallback sufficient;
        // unavailable observers retain the previous 1 ms EOF rescan.
        let maximum_wait_ms = if eof && child_observer.is_none() {
            1
        } else {
            20
        };
        let timeout_ms = deadline_poll_timeout(signals.deadline, maximum_wait_ms);
        let observer_notified;
        let ready = if capture_diagnostics {
            let descriptors = &mut diagnostic_poll_descriptors;
            descriptors[0].fd = if eof { -1 } else { fd };
            descriptors[0].events = libc::POLLIN;
            descriptors[0].revents = 0;
            descriptors[1].fd = child_observer.as_ref().map_or(-1, ChildObserver::raw_fd);
            descriptors[1].events = libc::POLLIN;
            descriptors[1].revents = 0;
            for (descriptor, child) in descriptors[2..].iter_mut().zip(&children.0) {
                descriptor.fd = child
                    .stderr
                    .as_ref()
                    .map_or(-1, |stderr| stderr.as_raw_fd());
                descriptor.events = libc::POLLIN;
                descriptor.revents = 0;
            }
            let ready =
                // SAFETY: `descriptors` is a Vec of pollfd whose length was set before the loop to
                // children + 2, and poll writes only revents within it. Each fd is -1, which poll
                // ignores, or a pipe held open by `stdout`, `children` or `child_observer` for
                // this iteration.
                unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, timeout_ms) };
            observer_notified = ready > 0 && descriptors[1].revents != 0;
            ready
        } else {
            let mut descriptors = [
                libc::pollfd {
                    fd: if eof { -1 } else { fd },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: child_observer.as_ref().map_or(-1, ChildObserver::raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: `descriptors` is a live array of two pollfd, and the count 2 matches it.
            // Entry 0 is the stdout pipe or -1, and entry 1 is the observer or -1. poll ignores
            // entries with a negative fd, as POSIX specifies.
            let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout_ms) };
            observer_notified = ready > 0 && descriptors[1].revents != 0;
            ready
        };
        observer_ready |= observer_notified;
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("polling terminal pipe");
            }
        }
    }

    writer
        .join()
        .with_context(|| format!("line {}: feeding applet stdin", calls[0].2))?;
    let status = observed_stages
        .last()
        .and_then(Option::as_ref)
        .and_then(|stage| stage.status);
    Ok(GroupOutput {
        bytes: output,
        typed_json: None,
        status,
        diagnostic: children
            .0
            .last()
            .map_or_else(Vec::new, |child| child.diagnostic.clone()),
        stdout_captured: terminal_io.captures_terminal_stdout() && sink.is_none(),
        stages: collect_native_stages(calls, &observed_stages, &children, signals),
    })
}
