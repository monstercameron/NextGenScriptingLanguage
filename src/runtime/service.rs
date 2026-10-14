//! Reuse a parsed body for interval ticks, calendar ticks, or streamed line events.
use super::checkpoint::{CheckpointOptions, Journal};
use super::resume;
use super::*;
use crate::schedule::CronSchedule;
use chrono::Local;
use std::os::fd::RawFd;
use std::process::ChildStdout;
use std::time::Duration;

pub struct ServiceLimits {
    pub max_runs: Option<usize>,
    pub max_record_bytes: usize,
}
#[derive(Default)]
pub struct ServiceResult {
    pub runs: usize,
    pub cache_hits: usize,
    pub launched_processes: usize,
}

pub(super) fn split_program(program: &Program) -> (Program, Program) {
    let line = match program.trigger.as_ref().expect("triggered program") {
        Trigger::Every { line, .. } | Trigger::Cron { line, .. } | Trigger::Listen { line, .. } => {
            *line
        }
    };
    let setup_length = program
        .steps
        .iter()
        .take_while(|step| step.line < line)
        .count();
    (
        Program {
            definitions: program.definitions.clone(),
            inputs: program.inputs.clone(),
            steps: program.steps[..setup_length].to_vec(),
            trigger: None,
        },
        Program {
            definitions: program.definitions.clone(),
            inputs: program.inputs.clone(),
            steps: program.steps[setup_length..].to_vec(),
            trigger: None,
        },
    )
}

fn stopped(signals: &Signals) -> Result<()> {
    if signals.cancelled.load(Ordering::Relaxed) {
        bail!("program cancelled by signal");
    }
    Ok(())
}
fn limit_reached(limits: &ServiceLimits, result: &ServiceResult) -> bool {
    limits.max_runs.is_some_and(|limit| result.runs >= limit)
}
fn trace_event(
    enabled: bool,
    signals: &Signals,
    value: impl FnOnce() -> serde_json::Value,
) -> Result<()> {
    if enabled {
        event(true, value(), signals)
    } else {
        Ok(())
    }
}

/// Find the next contiguous part of a listener record without scanning beyond
/// the configured record limit. The returned length includes a newline when
/// one is present.
fn next_record_segment(
    bytes: &[u8],
    record_len: usize,
    max_record_bytes: usize,
) -> Result<(usize, bool)> {
    if bytes.is_empty() {
        return Ok((0, false));
    }
    let remaining = max_record_bytes.saturating_sub(record_len);
    let probe_len = bytes.len().min(remaining.saturating_add(1));
    if let Some(newline) = bytes[..probe_len].iter().position(|byte| *byte == b'\n') {
        let length = newline + 1;
        if length > remaining {
            bail!("listener record exceeds {max_record_bytes} bytes");
        }
        return Ok((length, true));
    }
    if bytes.len() > remaining {
        bail!("listener record exceeds {max_record_bytes} bytes");
    }
    Ok((bytes.len(), false))
}

#[allow(clippy::too_many_arguments)]
fn fire(
    body: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    input: Option<(&str, Vec<u8>)>,
    emit: &mut impl FnMut(&[u8], &AtomicBool) -> Result<()>,
    result: &mut ServiceResult,
    mut journal: Option<&mut Journal>,
) -> Result<()> {
    stopped(signals)?;
    let started = Instant::now();
    let (input, binding) = match input {
        Some((name, bytes)) => {
            let text =
                String::from_utf8(bytes.clone()).context("listener records must be UTF-8 text")?;
            (
                Input::Bytes(bytes),
                Some((name.to_owned(), Value::String(text))),
            )
        }
        None => (Input::Empty, None),
    };
    let record_bytes = match &input {
        Input::Bytes(bytes) => bytes.len(),
        _ => 0,
    };
    let state_source = if let Some(journal) = journal.as_deref_mut() {
        if journal.has_pending() {
            resume::prepare(body, registry, journal)?;
            FiniteStateSource::Restore
        } else {
            let snapshot_output = match &input {
                Input::Bytes(bytes) => bytes.as_slice(),
                _ => &[],
            };
            let snapshot_bindings = binding.as_ref().map(|(name, value)| (name.as_str(), value));
            journal.begin_execution_parts(
                &options.working_directory,
                snapshot_output,
                snapshot_bindings,
            )?;
            FiniteStateSource::Fresh
        }
    } else {
        FiniteStateSource::Fresh
    };
    if binding.is_some() {
        trace_event(
            options.trace,
            signals,
            || serde_json::json!({"event":"line_received","bytes":record_bytes}),
        )?;
    }
    trace_event(
        options.trace,
        signals,
        || serde_json::json!({"event":"handler_start","run":result.runs + 1}),
    )?;
    let finite = if let Some(journal) = journal.as_deref_mut() {
        run_finite_inner_with_state_source(
            body,
            registry,
            options,
            signals,
            input,
            binding,
            Some(journal),
            state_source,
        )?
    } else {
        run_finite(body, registry, options, signals, input, binding)?
    };
    if let Err(error) = emit(&finite.output, &signals.cancelled) {
        if let Some(journal) = journal.as_deref_mut() {
            let _ = journal.fail(
                &format!("{error:#}"),
                signals.cancelled.load(Ordering::Relaxed),
            );
        }
        return Err(error);
    }
    if let Some(journal) = journal {
        journal.finish()?;
    }
    result.runs += 1;
    result.cache_hits += finite.cache_hits;
    result.launched_processes += finite.launched_processes;
    trace_event(
        options.trace,
        signals,
        || serde_json::json!({"event":"handler_end","run":result.runs,"elapsed_us":started.elapsed().as_micros(),"bytes":finite.output.len()}),
    )?;
    Ok(())
}
fn wait_until(deadline: Instant, signals: &Signals) -> Result<()> {
    loop {
        stopped(signals)?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}
pub(crate) fn next_interval(due: Instant, now: Instant, interval: Duration) -> Result<Instant> {
    // Keep the original phase but skip missed ticks rather than overlap/catch up.
    let remaining =
        interval.as_nanos() - now.saturating_duration_since(due).as_nanos() % interval.as_nanos();
    let wait = Duration::new(
        (remaining / 1_000_000_000).try_into()?,
        (remaining % 1_000_000_000) as u32,
    );
    now.checked_add(wait)
        .ok_or_else(|| anyhow!("interval deadline is out of range"))
}

/// Run a foreground service. No OS cron, launchd or systemd configuration is changed.
pub fn run_service(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    mut emit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<ServiceResult> {
    run_service_with_cancellation(program, registry, options, limits, |bytes, _| emit(bytes))
}

/// Run a service with a cooperative output sink. Blocking sinks must poll the
/// cancellation flag themselves; cancellation cannot interrupt arbitrary code.
pub fn run_service_with_cancellation(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    emit: impl FnMut(&[u8], &AtomicBool) -> Result<()>,
) -> Result<ServiceResult> {
    run_service_internal(program, registry, options, limits, None, emit)
}

/// Run a service with durable handler checkpoints in `checkpoint`.
pub fn run_service_checkpointed(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    checkpoint: &CheckpointOptions,
    emit: impl FnMut(&[u8], &AtomicBool) -> Result<()>,
) -> Result<ServiceResult> {
    run_service_internal(program, registry, options, limits, Some(checkpoint), emit)
}

fn run_service_internal(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    checkpoint: Option<&CheckpointOptions>,
    mut emit: impl FnMut(&[u8], &AtomicBool) -> Result<()>,
) -> Result<ServiceResult> {
    validate(program, registry)?;
    if checkpoint.is_some() {
        resume::validate_checkpoint_controls(program, registry)?;
    }
    let configured = configured_inputs(program, registry, options)?;
    let options = configured.as_ref();
    let trigger = program
        .trigger
        .as_ref()
        .ok_or_else(|| anyhow!("service requires a listener or schedule"))?;
    if limits.max_runs == Some(0) || limits.max_record_bytes == 0 {
        bail!("service limits must be positive");
    }
    let (setup, body) = split_program(program);
    if body.steps.is_empty() {
        bail!("service body must contain a statement");
    }
    let configured_cwd = fs::canonicalize(&options.working_directory)?;
    let mut cwd = configured_cwd.clone();
    for step in setup.steps {
        match step.kind {
            StepKind::UseFolder { path } => {
                cwd = fs::canonicalize(path_value(&path, &options.inputs, &cwd)?)?;
                if !cwd.is_dir() {
                    bail!("line {}: use folder requires a directory", step.line);
                }
            }
            StepKind::Invoke { call, .. }
                if registry.resolve_call(&call)?.builtin == Some(Builtin::Cd) =>
            {
                let args =
                    arguments_for_call(&call, registry.resolve_call(&call)?, &options.inputs)?;
                cwd = fs::canonicalize(cwd.join(&args[0]))
                    .with_context(|| format!("line {}: cd to {:?}", step.line, args[0]))?;
                if !cwd.is_dir() {
                    bail!("line {}: cd requires a directory", step.line);
                }
            }
            _ => bail!("service setup requires a folder directive or the cd builtin"),
        }
    }
    let options = RunOptions {
        inputs: options.inputs.clone(),
        working_directory: cwd,
        cache_directory: options
            .cache_directory
            .as_ref()
            .map(|path| configured_cwd.join(path)),
        trace: options.trace,
        max_output_bytes: options.max_output_bytes,
        command_timeout: options.command_timeout,
    };
    let mut journal = if let Some(checkpoint) = checkpoint {
        let fingerprint =
            resume::trigger_fingerprint(trigger, registry, &options.working_directory)?;
        let mut checkpoint = checkpoint.clone();
        if checkpoint.directory.is_relative() {
            checkpoint.directory = configured_cwd.join(&checkpoint.directory);
        }
        Some(Journal::open_with_inputs(
            &checkpoint,
            &options.working_directory,
            &format!("service:{fingerprint}"),
            options.max_output_bytes,
            input_signature(program, &options.inputs)?,
        )?)
    } else {
        None
    };
    let signals = Signals::register()?;
    let mut result = ServiceResult::default();

    // A checkpointed handler is independent of the event that originally
    // triggered it. Finish it before starting a new listener or waiting for a
    // future schedule tick.
    if journal.as_ref().is_some_and(Journal::has_pending) {
        fire(
            &body,
            registry,
            &options,
            &signals,
            None,
            &mut emit,
            &mut result,
            journal.as_mut(),
        )?;
        if limit_reached(limits, &result) {
            return Ok(result);
        }
    }
    match trigger {
        Trigger::Every { interval_ms, .. } => {
            let interval = Duration::from_millis(*interval_ms);
            let mut deadline = Instant::now()
                .checked_add(interval)
                .ok_or_else(|| anyhow!("interval deadline is out of range"))?;
            loop {
                wait_until(deadline, &signals)?;
                trace_event(
                    options.trace,
                    &signals,
                    || serde_json::json!({"event":"interval_tick","lateness_us":deadline.elapsed().as_micros()}),
                )?;
                fire(
                    &body,
                    registry,
                    &options,
                    &signals,
                    None,
                    &mut emit,
                    &mut result,
                    journal.as_mut(),
                )?;
                if limit_reached(limits, &result) {
                    return Ok(result);
                }
                deadline = next_interval(deadline, Instant::now(), interval)?;
            }
        }
        Trigger::Cron { expression, .. } => {
            let schedule = CronSchedule::parse(expression)?;
            // Begin at the next calendar minute, never replay historical ticks.
            let mut last_minute = Local::now().timestamp().div_euclid(60);
            loop {
                stopped(&signals)?;
                let now = Local::now();
                let minute = now.timestamp().div_euclid(60);
                if minute > last_minute {
                    last_minute = minute;
                    if schedule.matches(&now) {
                        trace_event(
                            options.trace,
                            &signals,
                            || serde_json::json!({"event":"calendar_tick","local_time":now.to_rfc3339()}),
                        )?;
                        fire(
                            &body,
                            registry,
                            &options,
                            &signals,
                            None,
                            &mut emit,
                            &mut result,
                            journal.as_mut(),
                        )?;
                        if limit_reached(limits, &result) {
                            return Ok(result);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Trigger::Listen {
            source, binding, ..
        } => {
            listen(
                source,
                binding,
                &body,
                registry,
                &options,
                limits,
                &signals,
                &mut emit,
                &mut result,
                journal.as_mut(),
            )?;
            Ok(result)
        }
    }
}

enum Reader {
    Stdin(std::io::Stdin),
    Applet(ChildStdout),
}
impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdin(stdin) => stdin.read(buffer),
            Self::Applet(stdout) => stdout.read(buffer),
        }
    }
}
impl AsRawFd for Reader {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Stdin(stdin) => stdin.as_raw_fd(),
            Self::Applet(stdout) => stdout.as_raw_fd(),
        }
    }
}
struct Nonblocking {
    fd: RawFd,
    original: libc::c_int,
}
impl Nonblocking {
    fn set(fd: RawFd) -> Result<Self> {
        let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if original == -1
            || unsafe { libc::fcntl(fd, libc::F_SETFL, original | libc::O_NONBLOCK) } == -1
        {
            return Err(std::io::Error::last_os_error()).context("setting listener nonblocking");
        }
        Ok(Self { fd, original })
    }
}
impl Drop for Nonblocking {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(self.fd, libc::F_SETFL, self.original);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn listen(
    source: &ListenSource,
    binding: &str,
    body: &Program,
    registry: &Registry,
    options: &RunOptions,
    limits: &ServiceLimits,
    signals: &Signals,
    emit: &mut impl FnMut(&[u8], &AtomicBool) -> Result<()>,
    result: &mut ServiceResult,
    mut journal: Option<&mut Journal>,
) -> Result<()> {
    let mut children = Children::default();
    let mut reader = match source {
        ListenSource::Stdin => Reader::Stdin(std::io::stdin()),
        ListenSource::Applet(call) => {
            let manifest = registry.resolve_call(call)?;
            let binary = binary_path(manifest.binary_for(call), &options.working_directory)?;
            let argv = arguments_for_call(call, manifest, &options.inputs)?;
            let mut child = Command::new(&binary)
                .args(argv)
                .current_dir(&options.working_directory)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .process_group(0)
                .spawn()
                .with_context(|| format!("starting listener {:?}", manifest.name))?;
            let pid = child.id();
            result.launched_processes += 1;
            let reader = Reader::Applet(child.stdout.take().expect("listener stdout"));
            children.0.push(ManagedChild {
                child,
                reaped: false,
                stderr: None,
                diagnostic: Vec::new(),
            });
            trace_event(
                options.trace,
                signals,
                || serde_json::json!({"event":"listener_spawn","applet":manifest.name_for(call),"pid":pid}),
            )?;
            reader
        }
    };
    let fd = reader.as_raw_fd();
    let _nonblocking = Nonblocking::set(fd)?;
    let mut record = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut eof = false;
    loop {
        stopped(signals)?;
        for managed in &mut children.0 {
            if !managed.reaped {
                if let Some(status) = managed.child.try_wait()? {
                    kill_group(&managed.child);
                    managed.reaped = true;
                    if !status.success() {
                        bail!("listener failed ({status})");
                    }
                }
            }
        }
        if !eof {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    if !record.is_empty() {
                        fire(
                            body,
                            registry,
                            options,
                            signals,
                            Some((binding, std::mem::take(&mut record))),
                            emit,
                            result,
                            journal.as_deref_mut(),
                        )?;
                        if limit_reached(limits, result) {
                            return Ok(());
                        }
                    }
                }
                Ok(count) => {
                    let mut offset = 0;
                    while offset < count {
                        let (length, terminated) = next_record_segment(
                            &buffer[offset..count],
                            record.len(),
                            limits.max_record_bytes,
                        )?;
                        record.extend_from_slice(&buffer[offset..offset + length]);
                        offset += length;
                        if terminated {
                            fire(
                                body,
                                registry,
                                options,
                                signals,
                                Some((binding, std::mem::take(&mut record))),
                                emit,
                                result,
                                journal.as_deref_mut(),
                            )?;
                            if limit_reached(limits, result) {
                                return Ok(());
                            }
                        }
                    }
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("reading listener"),
            }
        }
        if eof && children.0.iter().all(|child| child.reaped) {
            return Ok(());
        }
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, if eof { 0 } else { 1 }, 20) };
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("polling listener");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_segments_include_newlines_and_allow_chunk_continuation() {
        assert_eq!(next_record_segment(b"abc\ndef\n", 0, 8).unwrap(), (4, true));
        assert_eq!(next_record_segment(b"def\n", 0, 8).unwrap(), (4, true));
        assert_eq!(next_record_segment(b"ab", 0, 4).unwrap(), (2, false));
        assert_eq!(next_record_segment(b"c\n", 2, 4).unwrap(), (2, true));
        assert_eq!(next_record_segment(b"", 0, 4).unwrap(), (0, false));
    }

    #[test]
    fn record_limit_counts_newline_and_carries_partial_record_length() {
        assert_eq!(next_record_segment(b"abc\n", 0, 4).unwrap(), (4, true));
        assert!(next_record_segment(b"abcd\n", 0, 4).is_err());
        assert!(next_record_segment(b"bc", 3, 4).is_err());
    }

    #[test]
    fn disabled_trace_does_not_construct_event_payload() {
        let signals = Signals {
            cancelled: Arc::new(AtomicBool::new(false)),
            registrations: Vec::new(),
            capture_diagnostics: false,
            deadline: None,
        };
        let constructed = std::cell::Cell::new(false);
        trace_event(false, &signals, || {
            constructed.set(true);
            serde_json::json!({"event":"unused"})
        })
        .unwrap();
        assert!(!constructed.get());
    }

    #[test]
    fn skips_missed_intervals_without_drifting_phase() {
        let due = Instant::now();
        let interval = Duration::from_millis(10);
        let now = due + Duration::from_millis(35);
        assert_eq!(
            next_interval(due, now, interval).unwrap(),
            due + Duration::from_millis(40)
        );
        assert_eq!(
            next_interval(due, now + Duration::from_micros(125), interval).unwrap(),
            due + Duration::from_millis(40)
        );
    }
}
