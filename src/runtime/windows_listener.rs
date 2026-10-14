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
    let reader: Box<dyn Read + Send> = match source {
        ListenSource::Stdin => Box::new(std::io::stdin()),
        ListenSource::Applet(call) => {
            let manifest = registry.resolve_call(call)?;
            let binary = binary_path(manifest.binary_for(call), &options.working_directory)?;
            let argv = arguments_for_call(call, manifest, &options.inputs)?;
            let mut command = Command::new(&binary);
            command
                .args(argv)
                .current_dir(&options.working_directory)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            let (mut child, job) = crate::platform::windows::Job::spawn(&mut command)
                .with_context(|| format!("starting listener {:?}", manifest.name))?;
            let pid = child.id();
            result.launched_processes += 1;
            let reader = Box::new(child.stdout.take().expect("listener stdout"));
            children.0.push(ManagedChild {
                child,
                job,
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
    let mut reader = crate::platform::windows::Pump::new(reader);
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
        std::thread::sleep(Duration::from_millis(2));
    }
}
