//! Live native pipelines with bounded final-line framing.
use super::*;

#[derive(Debug, Default)]
pub struct StreamResult {
    pub records: usize,
    pub launched_processes: usize,
}

type Emit<'a> = dyn FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()> + 'a;

pub(super) struct LineSink<'a> {
    emit: &'a mut Emit<'a>,
    limit: Option<usize>,
    max_record_bytes: usize,
    pending: Vec<u8>,
    records: usize,
}

impl<'a> LineSink<'a> {
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

/// Run each native stage once, emitting newline-delimited final output as it
/// arrives. The callback provides backpressure and must observe cancellation
/// while blocked. Native programs control when their own stdout is flushed.
pub fn run_stream(
    program: &crate::stream::StreamProgram,
    registry: &Registry,
    options: &RunOptions,
    max_record_bytes: usize,
    mut emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<StreamResult> {
    crate::stream::validate(program, registry)?;
    let configured = configured_inputs(&program.pipeline, registry, options)?;
    let options = configured.as_ref();
    if max_record_bytes == 0 {
        bail!("stream record limit must be greater than zero");
    }
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
    let signals = Signals::register()?;
    let mut sink = LineSink {
        emit: &mut emit,
        limit: program.line_limit,
        max_record_bytes,
        pending: Vec::new(),
        records: 0,
    };
    let mut launched = 0;
    execute_group_with_sink(
        &calls,
        Input::Empty,
        None,
        &options.inputs,
        &options.working_directory,
        options,
        &mut launched,
        &signals,
        Some(&mut sink),
    )?;
    Ok(StreamResult {
        records: sink.records,
        launched_processes: launched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> Signals {
        Signals::with_cancellation(Arc::new(AtomicBool::new(false)), false).unwrap()
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
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
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
