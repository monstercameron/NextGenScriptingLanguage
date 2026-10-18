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
    loop {
        stopped(signals)?;
        for managed in &mut children.0 {
            if !managed.reaped {
                if let Some(status) = reap_exited(&mut managed.child)? {
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
