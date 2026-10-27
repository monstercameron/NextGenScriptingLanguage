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
        // `set` gets the descriptor of the `reader` that `listen` owns. That reader is declared
        // before the guard, so the fd stays open for the guard's whole life.
        let original = crate::platform::unix_ffi::set_nonblocking(fd)
            .context("setting listener nonblocking")?;
        Ok(Self { fd, original })
    }
}
impl Drop for Nonblocking {
    fn drop(&mut self) {
        // self.fd is the descriptor given to `set`. Its owner, `reader`, is declared before this
        // guard and so is dropped after it, leaving the fd open. The restored flags are the ones
        // that set read before adding O_NONBLOCK.
        let _ = crate::platform::unix_ffi::set_status_flags(self.fd, self.original);
    }
}

fn listen(
    source: &ListenSource,
    binding: &str,
    run: FiniteRun<'_>,
    limits: &ServiceLimits,
    emit: &mut impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
    result: &mut ServiceResult,
    mut journal: Option<&mut Journal>,
) -> Result<()> {
    let FiniteRun {
        program: body,
        registry,
        options,
        signals,
    } = run;
    let mut children = Children::default();
    let mut source_manifest: Option<&Manifest> = None;
    let mut reader = match source {
        ListenSource::Stdin => Reader::Stdin(std::io::stdin()),
        ListenSource::Applet(call) => {
            let manifest = registry.resolve_call(call)?;
            source_manifest = Some(manifest);
            crate::security::authorize_declared_filesystem_write(
                registry,
                manifest,
                &signals.sensitivity_floor(),
            )?;
            let binary = binary_path(manifest.binary_for(call), &options.working_directory)?;
            let argv = arguments_for_call(call, manifest, &options.inputs)?;
            stopped(signals)?;
            let mut command = crate::capabilities::command_for(
                &binary,
                argv,
                registry,
                &manifest.name,
                &crate::working_dir::WorkingDirectory::plain(options.working_directory.clone()),
            )?;
            let mut child = command
                .current_dir(&options.working_directory)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .process_group(0)
                .spawn()
                .with_context(|| format!("starting listener {:?}", manifest.name))?;
            signals.record_process_launch();
            result.launched_processes += 1;
            let pid = child.id();
            let reader = Reader::Applet(child.stdout.take().expect("listener stdout"));
            children.0.push(ManagedChild {
                child,
                reaped: false,
                stderr: None,
                diagnostic: Vec::new(),
                diagnostic_truncated: false,
            });
            stopped(signals)?;
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
    let mut failure_status = None;
    loop {
        stopped(signals)?;
        for managed in &mut children.0 {
            if !managed.reaped {
                if let Some(status) = reap_exited(&mut managed.child)? {
                    managed.reaped = true;
                    if !source_manifest.is_some_and(|manifest| {
                        listener_source_exited_successfully(manifest, status)
                    }) {
                        // The child can exit after writing data that is still buffered in
                        // stdout. Drain and deliver that data before reporting its status.
                        failure_status = Some(status);
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
                            FiniteRun {
                                program: body,
                                registry,
                                options,
                                signals,
                            },
                            Some((binding, std::mem::take(&mut record))),
                            emit,
                            result,
                            journal.as_deref_mut(),
                        )?;
                        if finish_listener_at_limit(limits, result, failure_status)? {
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
                                FiniteRun {
                                    program: body,
                                    registry,
                                    options,
                                    signals,
                                },
                                Some((binding, std::mem::take(&mut record))),
                                emit,
                                result,
                                journal.as_deref_mut(),
                            )?;
                            if finish_listener_at_limit(limits, result, failure_status)? {
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
            finish_listener(failure_status)?;
            return Ok(());
        }
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` is one live pollfd and nfds is 0 or 1, so poll reads at most
        // that entry. `fd` is the open descriptor of `reader`, which is alive in this loop.
        let ready = unsafe { libc::poll(&mut descriptor, if eof { 0 } else { 1 }, 20) };
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("polling listener");
            }
        }
    }
}

#[cfg(all(test, feature = "test-fixtures"))]
mod native_listener_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    struct FixtureListenerRun {
        result: Result<()>,
        output: Vec<Vec<u8>>,
    }

    fn run_fixture_status_source(status: i32, outcomes: serde_json::Value) -> FixtureListenerRun {
        let directory = tempfile::tempdir().unwrap();
        let fixture = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("native-fixture{}", std::env::consts::EXE_SUFFIX));
        assert!(fixture.is_file(), "native fixture binary is missing");
        let manifest = serde_json::json!({
            "name": "test.producer",
            "binary": fixture,
            "parameters": {},
            "arguments": [
                {"literal": "produce-exit"},
                {"literal": status.to_string()}
            ],
            "contract": {
                "version": 1,
                "output": {"type": "bytes", "encoding": "raw"},
                "outcomes": outcomes
            }
        });
        let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
        let program = crate::parser::parse(
            "listen to test.producer\nwhenever a line arrives as event\n  then encode event as json\n",
        )
        .unwrap();
        let (_, body) = split_program(&program);
        let Trigger::Listen {
            source, binding, ..
        } = program.trigger.as_ref().unwrap()
        else {
            panic!("listener trigger")
        };
        let signals = Signals::with_cancellation(Arc::new(AtomicBool::new(false)), false).unwrap();
        let options = RunOptions {
            max_output_bytes: 1024,
            command_timeout: Some(Duration::from_secs(1)),
            ..RunOptions::new(directory.path().to_path_buf())
        };
        let mut output = Vec::new();
        let result = listen(
            source,
            binding,
            FiniteRun {
                program: &body,
                registry: &registry,
                options: &options,
                signals: &signals,
            },
            &ServiceLimits {
                max_runs: None,
                max_record_bytes: 1024,
            },
            &mut |bytes, _, _| {
                output.push(bytes.to_vec());
                Ok(())
            },
            &mut ServiceResult::default(),
            None,
        );
        FixtureListenerRun { result, output }
    }

    #[test]
    fn native_listener_drains_records_before_classifying_producer_exit() {
        let successful = run_fixture_status_source(
            7,
            serde_json::json!([{"status": 7, "name": "complete", "kind": "success"}]),
        );
        successful.result.unwrap();
        assert_eq!(successful.output.len(), 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&successful.output[0]).unwrap(),
            serde_json::json!("event\n")
        );

        let explicit_failure = run_fixture_status_source(
            8,
            serde_json::json!([
                {"status": 7, "name": "complete", "kind": "success"},
                {"status": 8, "name": "failed", "kind": "failure"}
            ]),
        );
        assert!(format!("{:#}", explicit_failure.result.unwrap_err()).contains("listener failed"));
        assert_eq!(explicit_failure.output.len(), 1);

        let unmapped = run_fixture_status_source(
            9,
            serde_json::json!([{"status": 7, "name": "complete", "kind": "success"}]),
        );
        assert!(format!("{:#}", unmapped.result.unwrap_err()).contains("listener failed"));
        assert_eq!(unmapped.output.len(), 1);
    }
}
