use std::os::windows::io::AsHandle;

// Native child pipes can be observed directly. Only inherited stdin needs a
// cancellable reader worker; a quiet producer must never block a worker join.
enum WindowsListenerReader {
    Stdin(crate::platform::windows::Pump),
    Child(std::process::ChildStdout),
}
impl Read for WindowsListenerReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        match self {
            Self::Stdin(reader) => reader.read(buffer),
            Self::Child(reader) => match crate::platform::windows::available(reader.as_handle())? {
                None => Ok(0),
                Some(0) => Err(std::io::ErrorKind::WouldBlock.into()),
                Some(count) => {
                    let length = buffer.len().min(count);
                    reader.read(&mut buffer[..length])
                }
            },
        }
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
        ListenSource::Stdin => {
            WindowsListenerReader::Stdin(crate::platform::windows::Pump::new(std::io::stdin()))
        }
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
            command
                .current_dir(&options.working_directory)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            let (mut child, job) = crate::platform::windows::Job::spawn(&mut command)
                .with_context(|| format!("starting listener {:?}", manifest.name))?;
            signals.record_process_launch();
            result.launched_processes += 1;
            let pid = child.id();
            let reader = child.stdout.take().expect("listener stdout");
            children.0.push(ManagedChild {
                child,
                job,
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
            WindowsListenerReader::Child(reader)
        }
    };
    let mut record = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut eof = false;
    loop {
        stopped(signals)?;
        for managed in &mut children.0 {
            if !managed.reaped {
                if let Some(status) = managed.child.try_wait()? {
                    managed.job.terminate();
                    managed.reaped = true;
                    if !source_manifest.is_some_and(|manifest| {
                        listener_source_exited_successfully(manifest, status)
                    }) {
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
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod native_listener_tests {
    use super::*;

    struct FixtureListenerRun {
        result: Result<()>,
        output: Vec<Vec<u8>>,
        accounting: ServiceResult,
    }

    fn run_fixture_status_source(status: i32, outcomes: serde_json::Value) -> FixtureListenerRun {
        let directory = tempfile::tempdir().unwrap();
        let fixture = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/native-fixture.exe");
        assert!(fixture.is_file(), "build the native fixture example first");
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
            inputs: Default::default(),
            working_directory: directory.path().to_path_buf(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024,
            command_timeout: Some(Duration::from_secs(1)),
            total_timeout: None,
        };
        let mut output = Vec::new();
        let mut result = ServiceResult::default();
        let listener_result = listen(
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
            &mut result,
            None,
        );
        FixtureListenerRun {
            result: listener_result,
            output,
            accounting: result,
        }
    }

    #[test]
    fn a_quiet_native_listener_cancels_without_waiting_for_producer_eof() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/native-fixture.exe");
        assert!(fixture.is_file(), "build the native fixture example first");
        let binary = serde_json::to_string(fixture.to_str().unwrap()).unwrap();
        let program = crate::parser::parse(&format!(
            "listen to {binary} with arguments [\"sleep\", \"5000\"]\nwhenever a line arrives as event\n  run {binary} with arguments [\"copy\"]\n"
        )).unwrap();
        let (_, body) = split_program(&program);
        let Trigger::Listen {
            source, binding, ..
        } = program.trigger.as_ref().unwrap()
        else {
            panic!("listener trigger")
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancelled.clone();
        let cancellation = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            worker_cancel.store(true, Ordering::Relaxed);
        });
        let signals = Signals::with_cancellation(cancelled, false).unwrap();
        let options = RunOptions {
            inputs: Default::default(),
            working_directory: directory.path().to_path_buf(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024,
            command_timeout: Some(Duration::from_secs(1)),
            total_timeout: None,
        };
        let started = Instant::now();
        let result = listen(
            source,
            binding,
            FiniteRun {
                program: &body,
                registry: &Registry::default(),
                options: &options,
                signals: &signals,
            },
            &ServiceLimits {
                max_runs: None,
                max_record_bytes: 1024,
            },
            &mut |_, _, _| Ok(()),
            &mut ServiceResult::default(),
            None,
        );
        cancellation.join().unwrap();
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "quiet producer blocked cancellation"
        );
    }

    #[test]
    fn native_listener_uses_declared_success_statuses_and_rejects_other_outcomes() {
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
        assert_eq!(successful.accounting.launched_processes, 1);

        let explicit_failure = run_fixture_status_source(
            8,
            serde_json::json!([
                {"status": 7, "name": "complete", "kind": "success"},
                {"status": 8, "name": "failed", "kind": "failure"}
            ]),
        );
        assert!(format!("{:#}", explicit_failure.result.unwrap_err()).contains("listener failed"));
        assert_eq!(explicit_failure.accounting.launched_processes, 1);

        let unmapped = run_fixture_status_source(
            9,
            serde_json::json!([{"status": 7, "name": "complete", "kind": "success"}]),
        );
        assert!(format!("{:#}", unmapped.result.unwrap_err()).contains("listener failed"));
        assert_eq!(unmapped.accounting.launched_processes, 1);
    }
}
