//! Live native pipelines with bounded final-line framing.
use super::*;

#[derive(Debug, Default)]
pub struct StreamResult {
    pub records: usize,
    pub launched_processes: usize,
}

/// Execution context inherited by a native stream node.
///
/// The optional module snapshot and sensitivity set let graph adapters preserve
/// their whole-plan preflight and per-node provenance instead of reloading or
/// reconstructing either at stream execution time.
#[derive(Clone, Default)]
pub struct StreamExecutionContext {
    cancelled: Option<Arc<AtomicBool>>,
    deadline: Option<Instant>,
    module_session: Option<Arc<crate::modules::ProgramModuleSession>>,
    sensitivity_floor: Option<crate::security::SensitivitySet>,
}

impl StreamExecutionContext {
    pub fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled: Some(cancelled),
            ..Self::default()
        }
    }

    pub fn with_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.deadline = deadline;
        self
    }

    pub fn with_module_session(
        mut self,
        module_session: Arc<crate::modules::ProgramModuleSession>,
    ) -> Self {
        self.module_session = Some(module_session);
        self
    }

    pub fn with_sensitivity_floor(
        mut self,
        sensitivity_floor: crate::security::SensitivitySet,
    ) -> Self {
        self.sensitivity_floor = Some(sensitivity_floor);
        self
    }
}

type Emit<'a> = dyn FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()> + 'a;

fn stream_sensitivity_floor(
    program: &crate::stream::StreamProgram,
    registry: &Registry,
    module_session: &crate::modules::ProgramModuleSession,
    cwd: &Path,
    inherited: Option<&crate::security::SensitivitySet>,
) -> Result<crate::security::SensitivitySet> {
    let mut floor =
        super::workflow_sensitivity_floor(&program.pipeline, registry, module_session, cwd, None)?;
    if let Some(inherited) = inherited {
        // The enclosing graph/run context can add provenance, but it cannot
        // erase labels found in the stream's own source or module bodies.
        floor.extend(inherited);
    }
    Ok(floor)
}

pub(super) struct LineSink<'a> {
    emit: &'a mut Emit<'a>,
    limit: Option<usize>,
    max_record_bytes: usize,
    pending: Vec<u8>,
    records: usize,
}

impl<'a> LineSink<'a> {
    #[cfg(any(feature = "live", test))]
    pub(super) fn new(
        emit: &'a mut Emit<'a>,
        limit: Option<usize>,
        max_record_bytes: usize,
    ) -> Self {
        Self {
            emit,
            limit,
            max_record_bytes,
            pending: Vec::new(),
            records: 0,
        }
    }
    fn emit_record(emit: &mut Emit<'_>, bytes: &[u8], signals: &Signals) -> Result<()> {
        check_deadline(signals.deadline)?;
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        emit(bytes, &signals.cancelled, signals.deadline)?;
        check_deadline(signals.deadline)?;
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        Ok(())
    }

    fn emit_pending(&mut self, signals: &Signals) -> Result<bool> {
        Self::emit_record(self.emit, &self.pending, signals)?;
        self.pending.clear();
        self.records += 1;
        Ok(self.limit.is_some_and(|limit| self.records >= limit))
    }

    pub(super) fn push(&mut self, bytes: &[u8], signals: &Signals) -> Result<bool> {
        let mut part_start = 0;
        for newline in memchr::memchr_iter(b'\n', bytes) {
            let part = &bytes[part_start..=newline];
            part_start = newline + 1;
            if part.len() > self.max_record_bytes.saturating_sub(self.pending.len()) {
                bail!("stream record exceeds {} bytes", self.max_record_bytes);
            }
            if self.pending.is_empty() {
                // The callback is synchronous, so complete records can borrow
                // the current read buffer. Preserve the failed record exactly
                // as the buffered path does if any callback/check fails.
                if let Err(error) = Self::emit_record(self.emit, part, signals) {
                    self.pending.extend_from_slice(part);
                    return Err(error);
                }
                self.records += 1;
                if self.limit.is_some_and(|limit| self.records >= limit) {
                    return Ok(true);
                }
            } else {
                self.pending.extend_from_slice(part);
                if self.emit_pending(signals)? {
                    return Ok(true);
                }
            }
        }
        let tail = &bytes[part_start..];
        if !tail.is_empty() {
            if tail.len() > self.max_record_bytes.saturating_sub(self.pending.len()) {
                bail!("stream record exceeds {} bytes", self.max_record_bytes);
            }
            self.pending.extend_from_slice(tail);
        }
        Ok(false)
    }

    pub(super) fn finish(&mut self, signals: &Signals) -> Result<()> {
        if !self.pending.is_empty() {
            self.emit_pending(signals)?;
        }
        Ok(())
    }
}

/// Run each native stage once, emitting newline-delimited final output as it
/// arrives. The callback provides backpressure and must observe cancellation
/// while blocked. Native programs control when their own stdout is flushed.
pub fn run_stream(
    program: &crate::stream::StreamProgram,
    registry: &Registry,
    options: &RunOptions,
    max_record_bytes: usize,
    emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<StreamResult> {
    run_stream_with_context(
        program,
        registry,
        options,
        max_record_bytes,
        StreamExecutionContext::default(),
        emit,
    )
}

/// Run a native stream while sharing an enclosing runtime's cancellation flag.
/// The enclosing total deadline, module snapshot, and sensitivity floor can be
/// supplied through [`StreamExecutionContext`] using
/// [`run_stream_with_context`].
pub fn run_stream_with_cancellation(
    program: &crate::stream::StreamProgram,
    registry: &Registry,
    options: &RunOptions,
    max_record_bytes: usize,
    cancelled: Arc<AtomicBool>,
    emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<StreamResult> {
    run_stream_with_context(
        program,
        registry,
        options,
        max_record_bytes,
        StreamExecutionContext::new(cancelled),
        emit,
    )
}

/// Run a native stream with inherited cancellation, deadline, and run-scoped
/// policy/preflight context. `options.total_timeout` is measured from entry and
/// min-composed with the supplied absolute parent deadline; each native group
/// further min-composes its `command_timeout`.
pub fn run_stream_with_context(
    program: &crate::stream::StreamProgram,
    registry: &Registry,
    options: &RunOptions,
    max_record_bytes: usize,
    context: StreamExecutionContext,
    emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<StreamResult> {
    run_stream_with_context_and_process_observer(
        program,
        registry,
        options,
        max_record_bytes,
        context,
        None,
        emit,
    )
}

/// Internal graph entry point for retaining launch metrics even when stream
/// cancellation or downstream delivery fails before a StreamResult is returned.
pub(crate) fn run_stream_with_context_and_process_observer(
    program: &crate::stream::StreamProgram,
    registry: &Registry,
    options: &RunOptions,
    max_record_bytes: usize,
    context: StreamExecutionContext,
    process_observer: Option<super::ProcessObserver>,
    mut emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<StreamResult> {
    let local_deadline = super::deadline_after(options.total_timeout, "total timeout")?;
    let deadline = match (context.deadline, local_deadline) {
        (Some(parent), Some(local)) => Some(parent.min(local)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    };
    let cancelled = context
        .cancelled
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let signals = Signals::register_with_cancellation_and_deadline(cancelled, deadline)?;
    if let Some(observer) = process_observer {
        signals.set_process_observer(observer)?;
    }
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    crate::stream::validate(program, registry)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    let capability_cwd = fs::canonicalize(&options.working_directory)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    crate::capabilities::preflight(&program.pipeline, registry, &capability_cwd)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    let configured = configured_inputs(&program.pipeline, registry, options)?;
    let options = configured.as_ref();
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    if max_record_bytes == 0 {
        bail!("stream record limit must be greater than zero");
    }
    let module_session = if let Some(session) = context.module_session {
        super::preflight_module_session(&session, registry, &capability_cwd)?;
        let mut slot = signals
            .program_modules
            .lock()
            .map_err(|_| anyhow!("program module session lock is poisoned"))?;
        *slot = Some(Arc::clone(&session));
        session
    } else {
        super::preload_program_modules(&program.pipeline, registry, &signals, &capability_cwd)?
    };
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    let sensitivity_floor = stream_sensitivity_floor(
        program,
        registry,
        &module_session,
        &capability_cwd,
        context.sensitivity_floor.as_ref(),
    )?;
    signals.set_sensitivity_floor(sensitivity_floor);
    let calls = program
        .pipeline
        .steps
        .iter()
        .map(|step| {
            let StepKind::Invoke { call, .. } = &step.kind else {
                unreachable!("validated native pipeline")
            };
            Ok((call, registry.resolve_call(call)?, step.line))
        })
        .collect::<Result<Vec<_>>>()?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    let mut sink = LineSink {
        emit: &mut emit,
        limit: program.line_limit,
        max_record_bytes,
        pending: Vec::new(),
        records: 0,
    };
    let mut launched = 0;
    super::redact_command_failure(
        registry,
        execute_group_with_sink(
            registry,
            &calls,
            Input::Empty,
            None,
            &options.inputs,
            &crate::working_dir::WorkingDirectory::plain(options.working_directory.clone()),
            options,
            &mut launched,
            &signals,
            Some(&mut sink),
        ),
    )?;
    Ok(StreamResult {
        records: sink.records,
        launched_processes: launched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn signals() -> Signals {
        Signals::with_cancellation(Arc::new(AtomicBool::new(false)), false).unwrap()
    }

    #[test]
    fn inherited_stream_sensitivity_cannot_hide_a_secret_source() {
        let manifest = json!({
            "name": "secret.source",
            "binary": "/bin/true",
            "parameters": {},
            "arguments": [],
            "contract": {
                "version": 1,
                "output": {"type": "text", "encoding": "raw"},
                "ports": {
                    "inputs": {},
                    "outputs": {
                        "value": {
                            "schema": {"type": "text"},
                            "delivery": "single",
                            "sensitivity": "secret"
                        }
                    },
                    "events": {}
                }
            }
        })
        .to_string();
        let registry = Registry::from_sources(&[manifest]).unwrap();
        let program = crate::stream::parse("stream from secret.source\nshow each line\n").unwrap();
        let inherited = crate::security::SensitivitySet::from_labels([
            crate::contracts::Sensitivity::Public,
            crate::contracts::Sensitivity::Unknown,
        ]);

        let floor = stream_sensitivity_floor(
            &program,
            &registry,
            &crate::modules::ProgramModuleSession::default(),
            Path::new("."),
            Some(&inherited),
        )
        .unwrap();

        assert!(floor.contains(&crate::contracts::Sensitivity::Secret));
        assert!(floor.contains(&crate::contracts::Sensitivity::Public));
        assert!(floor.contains(&crate::contracts::Sensitivity::Unknown));
    }

    #[test]
    fn line_sink_preserves_binary_split_lines_and_ignores_after_limit() {
        let mut output = Vec::new();
        let mut emit = |bytes: &[u8], _: &AtomicBool, _: Option<Instant>| {
            output.extend_from_slice(bytes);
            Ok(())
        };
        let mut sink = LineSink {
            emit: &mut emit,
            limit: Some(3),
            max_record_bytes: 8,
            pending: Vec::new(),
            records: 0,
        };
        let signals = signals();
        assert!(!sink.push(b"a\0", &signals).unwrap());
        assert!(!sink.push(b"\n\xff\n", &signals).unwrap());
        assert!(sink.push(b"last\nignored\n", &signals).unwrap());
        assert_eq!(sink.records, 3);
        assert!(sink.pending.is_empty());
        drop(sink);
        assert_eq!(output, b"a\0\n\xff\nlast\n");
    }

    #[test]
    fn line_sink_checks_limit_before_emitting_and_counts_unterminated_eof() {
        let mut output = Vec::new();
        let mut emit = |bytes: &[u8], _: &AtomicBool, _: Option<Instant>| {
            output.extend_from_slice(bytes);
            Ok(())
        };
        let signals = signals();
        let mut sink = LineSink {
            emit: &mut emit,
            limit: None,
            max_record_bytes: 3,
            pending: Vec::new(),
            records: 0,
        };
        assert!(!sink.push(b"ab", &signals).unwrap());
        assert!(sink.push(b"cd\n", &signals).is_err());
        drop(sink);
        assert!(output.is_empty());
        let mut emit = |bytes: &[u8], _: &AtomicBool, _: Option<Instant>| {
            output.extend_from_slice(bytes);
            Ok(())
        };
        let mut sink = LineSink {
            emit: &mut emit,
            limit: Some(1),
            max_record_bytes: 3,
            pending: Vec::new(),
            records: 0,
        };
        assert!(!sink.push(b"abc", &signals).unwrap());
        sink.finish(&signals).unwrap();
        sink.finish(&signals).unwrap();
        assert_eq!(sink.records, 1);
        drop(sink);
        assert_eq!(output, b"abc");
    }
    #[test]
    fn final_record_callback_cancellation_does_not_become_limit_success() {
        let signals = signals();
        let mut emit = |_: &[u8], cancel: &AtomicBool, _: Option<Instant>| {
            cancel.store(true, Ordering::Relaxed);
            Ok(())
        };
        let mut sink = LineSink {
            emit: &mut emit,
            limit: Some(1),
            max_record_bytes: 8,
            pending: Vec::new(),
            records: 0,
        };
        assert!(sink
            .push(b"one\n", &signals)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
    }

    #[cfg(unix)]
    #[test]
    fn completed_native_failure_during_final_emit_is_propagated() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("fail.json"),
            serde_json::json!({
                "name":"fail", "binary":"/bin/sh", "arguments":[
                    {"literal":"-c"}, {"literal":r#"printf '%s' "$$" > child.pid; printf 'one\n'; while [ ! -e release ]; do sleep 0.01; done; exit 7"#}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let registry = Registry::load(directory.path()).unwrap();
        let program =
            crate::stream::parse("stream from fail\ntake the first 1 lines\nshow each line\n")
                .unwrap();
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: directory.path().into(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 8,
            command_timeout: Some(Duration::from_secs(3)),
            total_timeout: None,
        };
        let mut emitted = Vec::new();
        let result = run_stream(&program, &registry, &options, 8, |bytes, _, _| {
            emitted.extend_from_slice(bytes);
            fs::write(directory.path().join("release"), b"go").unwrap();
            // Observe the actual exit without consuming its wait status. A
            // fixed sleep races a loaded CI host; the runtime must remain the
            // owner that reaps this exact child after the callback returns.
            let pid: libc::pid_t = fs::read_to_string(directory.path().join("child.pid"))
                .unwrap()
                .parse()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                // SAFETY: siginfo_t is a plain C struct whose fields are integers and a union of
                // integers and pointers. The all-zero bit pattern is valid for every member.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                // SAFETY: `info` is a live, writable siginfo_t, as waitid requires. The pid is the
                // fixture's own `$$`, the direct child that run_stream spawned, so it is a child
                // of this process. WNOWAIT keeps the child unreaped for the runtime, and WNOHANG
                // means the call does not block.
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                    )
                };
                if result < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    panic!("observing owned fixture exit: {error}");
                }
                if info.si_signo == libc::SIGCHLD && info.si_code == libc::CLD_EXITED {
                    break;
                }
                assert!(Instant::now() < deadline, "owned fixture did not exit");
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(emitted, b"one\n");
        assert!(format!("{:#}", result.unwrap_err()).contains("failed"));
    }
}

#[cfg(test)]
mod framing_evidence {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, hint::black_box};

    pub(super) struct LegacySink<'a> {
        emit: &'a mut Emit<'a>,
        limit: Option<usize>,
        max_record_bytes: usize,
        pending: Vec<u8>,
        records: usize,
    }

    impl<'a> LegacySink<'a> {
        pub(super) fn new(
            emit: &'a mut Emit<'a>,
            limit: Option<usize>,
            max_record_bytes: usize,
        ) -> Self {
            Self {
                emit,
                limit,
                max_record_bytes,
                pending: Vec::new(),
                records: 0,
            }
        }
        fn emit_pending(&mut self, signals: &Signals) -> Result<bool> {
            check_deadline(signals.deadline)?;
            if signals.cancelled.load(Ordering::Relaxed) {
                bail!("workflow cancelled by signal");
            }
            (self.emit)(&self.pending, &signals.cancelled, signals.deadline)?;
            check_deadline(signals.deadline)?;
            if signals.cancelled.load(Ordering::Relaxed) {
                bail!("workflow cancelled by signal");
            }
            self.pending.clear();
            self.records += 1;
            Ok(self.limit.is_some_and(|limit| self.records >= limit))
        }

        pub(super) fn push(&mut self, bytes: &[u8], signals: &Signals) -> Result<bool> {
            for part in bytes.split_inclusive(|byte| *byte == b'\n') {
                if part.len() > self.max_record_bytes.saturating_sub(self.pending.len()) {
                    bail!("stream record exceeds {} bytes", self.max_record_bytes);
                }
                self.pending.extend_from_slice(part);
                if part.last() == Some(&b'\n') && self.emit_pending(signals)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }

        pub(super) fn finish(&mut self, signals: &Signals) -> Result<()> {
            if !self.pending.is_empty() {
                self.emit_pending(signals)?;
            }
            Ok(())
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Observation {
        output: Vec<Vec<u8>>,
        steps: Vec<(std::result::Result<bool, String>, Vec<u8>, usize)>,
    }

    fn observe(
        chunks: &[&[u8]],
        limit: Option<usize>,
        max: usize,
        mode: usize,
        legacy: bool,
    ) -> Observation {
        let mut signals =
            Signals::with_cancellation(Arc::new(AtomicBool::new(mode == 3)), false).unwrap();
        if mode == 4 {
            signals.deadline = Some(Instant::now() - Duration::from_secs(1));
        }
        let mut output = Vec::new();
        let mut emit = |bytes: &[u8], cancel: &AtomicBool, _: Option<Instant>| {
            output.push(bytes.to_vec());
            if output.len() == 2 && mode == 1 {
                bail!("fixture callback failure");
            }
            if output.len() == 2 && mode == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
            Ok(())
        };
        let mut steps = Vec::new();
        macro_rules! observe_sink {
            ($sink:expr) => {{
                let mut sink = $sink;
                for chunk in chunks {
                    let result = sink.push(chunk, &signals).map_err(|e| format!("{e:#}"));
                    steps.push((result, sink.pending.clone(), sink.records));
                }
                for _ in 0..2 {
                    signals.cancelled.store(false, Ordering::Relaxed);
                    signals.deadline = None;
                    let result = sink
                        .finish(&signals)
                        .map(|()| false)
                        .map_err(|e| format!("{e:#}"));
                    steps.push((result, sink.pending.clone(), sink.records));
                }
            }};
        }
        if legacy {
            observe_sink!(LegacySink::new(&mut emit, limit, max));
        } else {
            observe_sink!(LineSink::new(&mut emit, limit, max));
        }
        Observation { output, steps }
    }

    #[test]
    fn borrowed_record_framing_matches_legacy_failures_and_bytes() {
        let mut cases = 0;
        for input in [
            b"".as_slice(),
            b"\n",
            b"a\n\nlast",
            b"a\0\n\xff\r\nlast\nignored\n",
            b"1234567\n12345678\nend",
        ] {
            for split in 0..=input.len() {
                let chunks = [&input[..split], &input[split..]];
                for max in [0, 1, 3, 8, 8192] {
                    for limit in [None, Some(0), Some(1), Some(3)] {
                        for mode in 0..5 {
                            assert_eq!(
                                observe(&chunks, limit, max, mode, false),
                                observe(&chunks, limit, max, mode, true),
                                "input={input:?} split={split} max={max} limit={limit:?} mode={mode}"
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert!(cases > 5000);
    }

    #[test]
    #[ignore = "explicit paired release performance measurements"]
    fn stream_record_framing() {
        let signals = Signals::with_cancellation(Arc::new(AtomicBool::new(false)), false).unwrap();
        let mut results = BTreeMap::new();
        for (name, record_size, chunk_size, tail) in [
            ("complete_8", 8, 32768, false),
            ("complete_1k", 1024, 32768, false),
            ("complete_8k", 8192, 32768, false),
            ("fragmented_8", 8, 3, false),
            ("fragmented_8k", 8192, 4096, false),
            ("unterminated_8k", 8192, 32768, true),
        ] {
            let mut input = vec![b'x'; 256 * 1024];
            for index in (record_size - 1..input.len()).step_by(record_size) {
                input[index] = b'\n';
            }
            if tail {
                *input.last_mut().unwrap() = b'z';
            }
            let run = || {
                let mut bytes = 0;
                let mut records = 0;
                let mut emit = |part: &[u8], _: &AtomicBool, _: Option<Instant>| {
                    black_box(part);
                    bytes += part.len();
                    records += 1;
                    Ok(())
                };
                let mut sink = LineSink::new(&mut emit, None, 65536);
                for chunk in input.chunks(chunk_size) {
                    assert!(!sink.push(black_box(chunk), &signals).unwrap());
                }
                sink.finish(&signals).unwrap();
                let count = sink.records;
                drop(sink);
                (bytes, records, count)
            };
            assert_eq!(
                run(),
                (
                    input.len(),
                    input.len() / record_size,
                    input.len() / record_size
                )
            );
            let mut raw = Vec::new();
            for batch in 0..40 {
                let start = Instant::now();
                for _ in 0..5 {
                    black_box(run());
                }
                if batch >= 8 {
                    raw.push(start.elapsed().as_secs_f64() * 1e9 / 5.0);
                }
            }
            let mut sorted = raw.clone();
            sorted.sort_by(f64::total_cmp);
            results.insert(name,json!({"p50_ns":(sorted[15]+sorted[16])/2.0,"p95_ns":sorted[30],"raw_batch_average_ns":raw,"bytes":input.len(),"record_bytes":record_size,"chunk_bytes":chunk_size,"input_sha256":format!("{:x}",Sha256::digest(&input))}));
        }
        let report = json!({"method":"Private native stream framing: 256KiB fixed bytes per operation, synchronous counting callback, no child/IO. Input and cancellation setup/hash excluded; sink creation, framing, checks, callback, destruction included. Eight warmup and32 measured batches, five ops each. Warm process; not native IO latency or throughput.","results":results});
        fs::write(
            std::env::var("OPT_BENCH_REPORT").unwrap(),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}
