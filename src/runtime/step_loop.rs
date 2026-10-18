//! The finite step loop: the statement dispatch that runs one program's steps,
//! its control flow, and the step arms for every statement kind.

use super::*;

/// What a finite run reads from its caller: the program, its registry and options, and the
/// signals that can cancel it. Nested runs receive a copy, so it is `Copy`.
#[derive(Clone, Copy)]
pub(super) struct FiniteRun<'a> {
    pub(super) program: &'a Program,
    pub(super) registry: &'a Registry,
    pub(super) options: &'a RunOptions,
    pub(super) signals: &'a Signals,
}

/// How a finite run starts: its input and bindings, the checkpoint it journals to, and where
/// its state is restored from or captured to.
pub(super) struct FiniteStart<'a> {
    pub(super) input: Input,
    pub(super) bindings: BTreeMap<String, Value>,
    pub(super) journal: Option<&'a mut checkpoint::Journal>,
    pub(super) state_source: FiniteStateSource,
    pub(super) state_out: Option<&'a mut live::NodeState>,
}

pub(super) fn run_finite(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    signals: &Signals,
    initial_input: Input,
    initial_binding: Option<(String, Value)>,
) -> Result<RunResult> {
    run_finite_inner(
        FiniteRun {
            program,
            registry,
            options,
            signals,
        },
        initial_input,
        initial_binding,
        None,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FiniteStateSource {
    Fresh,
    Restore,
}

pub(super) fn run_finite_inner(
    run: FiniteRun<'_>,
    initial_input: Input,
    initial_binding: Option<(String, Value)>,
    journal: Option<&mut checkpoint::Journal>,
) -> Result<RunResult> {
    handoff::validate(run.program, journal.is_some())?;
    let state_source = if journal.is_some() {
        FiniteStateSource::Restore
    } else {
        FiniteStateSource::Fresh
    };
    run_finite_inner_with_state_source(
        run,
        FiniteStart {
            input: initial_input,
            bindings: initial_binding.into_iter().collect(),
            journal,
            state_source,
            state_out: None,
        },
    )
}

pub(super) fn run_finite_inner_with_state_source(
    run: FiniteRun<'_>,
    start: FiniteStart<'_>,
) -> Result<RunResult> {
    run_finite_state(run, start)
}

pub(super) fn run_finite_state(run: FiniteRun<'_>, start: FiniteStart<'_>) -> Result<RunResult> {
    let FiniteRun {
        program,
        registry,
        options,
        signals,
    } = run;
    let FiniteStart {
        input: initial_input,
        bindings: initial_bindings,
        mut journal,
        state_source,
        state_out,
    } = start;
    // Nested finite module and task calls share one admission/output budget.
    // The last returning lexical run drops the slot, so repeated service
    // handlers begin with a fresh per-run budget while sharing their Signals.
    let (_finite_depth, depth) = FiniteDepth::enter();
    // Only the CLI's top-level run streams. Module, handler and library runs
    // keep capturing, because their callers consume the bytes.
    let stream_top = depth == 0 && STREAM_REQUEST.with(|request| request.get());
    let _job_run = signals.begin_job_run(options.max_output_bytes)?;
    let mut cwd =
        crate::working_dir::WorkingDirectory::plain(fs::canonicalize(&options.working_directory)?);
    let module_session = preload_program_modules(program, registry, signals, &cwd)?;
    // A fresh run resolves its native commands before any of them can start. A
    // restore does not, since completed nodes may no longer need every tool.
    let resuming = journal
        .as_deref()
        .is_some_and(|journal| !journal.completed_nodes().is_empty());
    if !resuming {
        preflight_executables(program, registry, &cwd)?;
    }
    if (!signals.background_jobs_allowed
        || state_out.is_some()
        || (journal.is_some() && !resume::checkpoint_jobs_supported(program)))
        && (has_job_operations(program) || module_session.has_background_job_operations())
    {
        bail!("finite-scope background job handles are unsupported in this execution lifetime, including imported program modules");
    }
    let mut job_supervisor = jobs::Supervisor::new(signals, options.max_output_bytes)?;
    let mut sensitivity_floor = workflow_sensitivity_floor(
        program,
        registry,
        &module_session,
        &cwd,
        signals.worker_pool.as_deref(),
    )?;
    // Caller-provided initial bytes have no trusted provenance declaration.
    if !matches!(&initial_input, Input::Empty) {
        sensitivity_floor.insert(crate::contracts::Sensitivity::Unknown);
    }
    signals.set_sensitivity_floor(sensitivity_floor);
    let cache_override = options.cache_directory.as_ref().map(|path| cwd.join(path));
    // Component and managed handlers may supply values without `accept`
    // declarations. Validate their known loop inputs before earlier effects.
    let preflight_context =
        !initial_bindings.is_empty() && state_source == FiniteStateSource::Fresh;
    let _step_ids = StepIdScope::enter(&program.steps, options.trace || observe::is_installed());
    let mut bindings = initial_bindings;
    // Raw captures from `then capture bytes as NAME`, kept apart from text bindings.
    let mut byte_bindings: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut data = initial_input;
    // Set by a `Redirect` step and consumed by the very next `Invoke`.
    let mut pending_redirect: Option<PendingRedirect> = None;
    // Set by an `Environment` step and consumed by the next command.
    let mut pending_env: Option<crate::capabilities::CommandEnvOverlay> = None;
    let mut pending_handoff = false;
    let mut pending_process: Option<handoff::ProcessRequest> = None;
    // Detached launches started in this run, bounded by handoff::MAX_DETACHED_LAUNCHES.
    let mut detached_launches = 0usize;
    let mut result = RunResult {
        output: Vec::new(),
        cache_hits: 0,
        launched_processes: 0,
        executions: Vec::new(),
        dropped_executions: 0,
        dropped_stages: 0,
        exit: None,
    };
    let mut execution_history = execution_result::ExecutionHistory::default();
    // The step index of a declared retry in progress, and its failed attempts so far.
    let mut retry_failures: Option<(usize, u32)> = None;
    let mut index = 0;
    let mut call_frames: Vec<calls::Frame<'_>> = Vec::new();
    let mut loop_frames = loops::LoopStack::default();
    let mut temp_scopes = temp::ScopeStack::default();
    let mut control_state = control_flow::RuntimeState::default();
    if let Some(journal) = journal.as_deref_mut() {
        if state_source == FiniteStateSource::Restore {
            let mut state = journal.take_restored_state()?;
            index = journal.completed_nodes().last().map_or(0, |node| node.end);
            // A top-level for each with committed elements continues after them.
            if let Some(resumed) =
                checkpoint::elements::for_each_resume(journal, program, index, &state, registry)?
            {
                loop_frames.restore_for_each(
                    index,
                    &resumed.binding,
                    resumed.outer,
                    resumed.items,
                    resumed.consumed,
                )?;
                state = resumed.state;
            }
            cwd = enter_directory(registry, &state.cwd, &cwd)?;
            bindings = state.bindings;
            data = Input::Bytes(state.output);
            // A completed exit or fail is a terminal node: nothing after it ran,
            // so the restored run ends here with the same status. Its operands
            // are already in the restored bindings.
            let terminal = journal
                .completed_nodes()
                .last()
                .filter(|node| node.end == node.start + 1)
                .and_then(|node| program.steps.get(node.start))
                .map(|step| program_exit(&step.kind, &bindings))
                .transpose()?
                .flatten();
            if let Some(exit) = terminal {
                control_state.set_exit(exit);
                index = program.steps.len();
            }
            if index == program.steps.len() {
                journal.discard_removed_tail()?;
            }
        } else {
            // The service just committed this exact path with the event snapshot.
            cwd = crate::working_dir::WorkingDirectory::plain(options.working_directory.clone());
        }
        if !cwd.is_dir() {
            bail!("checkpoint working directory no longer exists");
        }
        lazy_event(
            options.trace,
            signals,
            || serde_json::json!({"event":"checkpoint_resume","restored_nodes":journal.completed_nodes().len(),"next_step":index}),
        )?;
    }
    if !program.inputs.is_empty() {
        for (name, value) in &options.inputs {
            bindings
                .entry(name.clone())
                .or_insert_with(|| value.clone());
        }
        bindings.extend(crate::inputs::resolve(&program.inputs, &bindings, true)?);
    }
    if preflight_context {
        preflight_inputs(program, registry, &bindings)?;
    }
    let mut journal_end = None;
    let mut journal_uncertain = false;
    macro_rules! control_context {
        () => {
            ControlContext {
                program,
                state: &mut control_state,
                call_frames: &mut call_frames,
                loop_frames: &mut loop_frames,
                bindings: &mut bindings,
                cwd: &mut cwd,
                job_supervisor: &mut job_supervisor,
            }
        };
    }
    // A journal node records its first step's id, so resume matches it by statement.
    let journal_step_ids = journal
        .is_some()
        .then(|| crate::ast::step_ids(&program.steps));
    while index < program.steps.len() {
        if signals.cancelled.load(Ordering::Relaxed) && !control_state.cleanup_active() {
            let error = call_frames.iter().rev().fold(
                anyhow!("workflow cancelled by signal"),
                |error, frame| {
                    error.context(format!(
                        "program {:?}, called at line {}",
                        frame.name, frame.line
                    ))
                },
            );
            let action = control_state.fail(error, true)?;
            match apply_control_action(action, control_context!())? {
                AppliedControl::Continue => {}
                AppliedControl::Jump(target) => {
                    index = target;
                    continue;
                }
                AppliedControl::Rethrow(error) => {
                    if let Some(journal) = journal.as_deref_mut() {
                        if !journal.persistence_failed() {
                            journal.fail(&format!("{error:#}"), true)?;
                        }
                    }
                    return Err(error);
                }
            }
        }
        let parent_signals = signals;
        let was_cleanup = control_state.cleanup_active();
        let cleanup_signals = if was_cleanup {
            let deadline = match (parent_signals.deadline, control_state.cleanup_deadline()) {
                (Some(parent), Some(cleanup)) => Some(parent.min(cleanup)),
                (Some(parent), None) | (None, Some(parent)) => Some(parent),
                (None, None) => None,
            };
            Some(parent_signals.cleanup_scope(deadline))
        } else {
            None
        };
        let signals = cleanup_signals.as_ref().unwrap_or(parent_signals);
        // An interrupt that requested a drain unwinds from this statement boundary, as
        // `exit` does: finally blocks run, including `on interrupt` and `on exit`.
        // Running steps are not cancelled; the run stops before starting the next one.
        // A `try` opener is not a boundary: entering it creates the frame the drain
        // unwinds through, so the drain starts at the first statement inside it.
        if !control_state.cleanup_active()
            && !matches!(program.steps[index].kind, StepKind::Try { .. })
            && parent_signals.begin_drain()
        {
            let exit = ProgramExit {
                status: drain_exit_status(),
                message: None,
            };
            let action = control_state.transfer(
                control_flow::TransferKind::Exit,
                program.steps.len(),
                control_flow::UnwindTo::new(0, 0),
            )?;
            if !matches!(action, control_flow::ControlAction::Continue { .. }) {
                control_state.set_exit(exit);
            }
            match apply_control_action(action, control_context!())? {
                AppliedControl::Continue => {}
                AppliedControl::Jump(target) => {
                    index = target;
                    continue;
                }
                AppliedControl::Rethrow(error) => return Err(error),
            }
        }
        let step = &program.steps[index];
        set_current_step(index);
        // Markers only steer control flow; the statements they close are the stops.
        if depth == 0
            && !matches!(
                step.kind,
                StepKind::EndLoop { .. }
                    | StepKind::EndIf
                    | StepKind::EndCall
                    | StepKind::EndTempScope { .. }
                    | StepKind::Otherwise { .. }
                    | StepKind::OnInterrupt { .. }
            )
        {
            statement_hook::before_statement(step.line, &bindings)?;
        }
        if let Some(journal) = journal.as_deref_mut().filter(|_| journal_end.is_none()) {
            let end = resume::node_end(program, index);
            journal_end = Some(end);
            // Element nodes record uncertainty per element in their element log, so one
            // uncertain element does not make the whole node uncertain.
            let tracks_elements = checkpoint::elements::tracks_new_node(journal, program, index);
            journal_uncertain = !tracks_elements
                && (matches!(
                    step.kind,
                    StepKind::If { .. }
                        | StepKind::IfCommand { .. }
                        | StepKind::CallProgram { .. }
                        | StepKind::Loop { .. }
                ) || resume::uncertain_on_failure(program, index, end, registry)?);
            let module_session = current_module_session(signals)?;
            let fingerprint = resume::node_fingerprint_with_modules(
                program,
                index,
                end,
                registry,
                &cwd,
                module_session.fingerprints(),
            )?;
            let step_id = journal_step_ids
                .as_ref()
                .and_then(|ids| ids.get(index))
                .cloned();
            journal.begin_node_record(checkpoint::NodeBegin {
                fingerprint: &fingerprint,
                start: index,
                end,
                line: step.line,
                cwd_before: &cwd,
                retry_safe: tracks_elements || resume::retry_safe(program, index, end, registry)?,
                step_id,
            })?;
        }
        let mut jump_to = None;
        let mut control_rethrow = None;
        let execution = (|| -> Result<()> {
            if control_state.cleanup_active() {
                control_state.charge_cleanup_step()?;
                check_deadline(signals.deadline)?;
            }
            match &step.kind {
                StepKind::Loop { kind, end } => {
                    control_state.enter_loop(index, index + end)?;
                    let entered = loop_frames
                        .header_with_commands(
                            index,
                            kind,
                            &mut bindings,
                            &mut |call, expect_success, values| {
                                run_command_condition(
                                    ConditionRequest {
                                        call,
                                        expect_success,
                                        line: step.line,
                                        bindings: values,
                                        cwd: &cwd,
                                        options,
                                        signals,
                                        registry,
                                    },
                                    &mut result.launched_processes,
                                )
                            },
                        )
                        .with_context(|| format!("line {}", step.line))?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"loop", "line":step.line, "entered":entered}),
                    )?;
                    if entered {
                        if let Some(journal) = journal.as_deref_mut() {
                            checkpoint::elements::for_each_element_begin(
                                journal,
                                program,
                                registry,
                                &loop_frames,
                                index,
                            )?;
                        }
                    }
                    if !entered {
                        control_state.leave_loop(index)?;
                        jump_to = Some(index + end);
                    }
                }
                StepKind::TempScope { kind, binding, .. } => {
                    if bindings.contains_key(binding) {
                        bail!(
                            "line {}: temporary binding {binding:?} is already defined",
                            step.line
                        );
                    }
                    // The policy must allow the temporary directory before the scope creates
                    // anything there, and before its stale sweep can remove anything.
                    crate::capabilities::authorize_host_path(
                        registry,
                        &std::env::temp_dir(),
                        crate::capabilities::HostAccess::Write,
                        &cwd,
                    )
                    .with_context(|| format!("line {}", step.line))?;
                    let path = temp_scopes
                        .open(index, *kind)
                        .with_context(|| format!("line {}", step.line))?;
                    let path = path.into_os_string().into_string().map_err(|_| {
                        anyhow!("line {}: temporary path is not valid UTF-8", step.line)
                    })?;
                    bindings.insert(binding.clone(), Value::String(path));
                }
                StepKind::EndTempScope { back } => {
                    let header = index
                        .checked_sub(*back)
                        .context("invalid temporary scope end")?;
                    if let StepKind::TempScope { binding, .. } = &program.steps[header].kind {
                        bindings.remove(binding);
                    }
                    // Removes this scope's path now, and any scope a jump left open.
                    temp_scopes.close(header);
                }
                StepKind::EndLoop { back } => {
                    let header = index.checked_sub(*back).context("invalid loop back edge")?;
                    loop_frames
                        .end_iteration(header, &mut bindings)
                        .with_context(|| format!("line {}", step.line))?;
                    if let Some(journal) = journal.as_deref_mut() {
                        if checkpoint::elements::for_each_element_due(journal, program, header) {
                            // The same persistence gate as a node: an element's state reaches
                            // the journal only if the checkpoint policy allows it.
                            authorize_checkpoint_persistence(
                                registry,
                                &signals.sensitivity_floor(),
                            )?;
                            let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                                options.max_output_bytes,
                                signals,
                                options.command_timeout,
                            )?;
                            checkpoint::elements::for_each_element_commit(
                                journal,
                                &loop_frames,
                                header,
                                &cwd,
                                &bytes,
                                &bindings,
                            )?;
                            data = Input::Bytes(bytes);
                        }
                    }
                    jump_to = Some(header);
                }
                StepKind::If {
                    condition,
                    otherwise,
                    ..
                } => {
                    let matched = control::evaluate(condition, &bindings)
                        .with_context(|| format!("line {}", step.line))?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"condition","line":step.line,"matched":matched}),
                    )?;
                    if !matched {
                        jump_to = Some(index + otherwise);
                    }
                }
                StepKind::IfCommand {
                    call,
                    expect_success,
                    otherwise,
                    ..
                } => {
                    let matched = run_command_condition(
                        ConditionRequest {
                            call,
                            expect_success: *expect_success,
                            line: step.line,
                            bindings: &bindings,
                            cwd: &cwd,
                            options,
                            signals,
                            registry,
                        },
                        &mut result.launched_processes,
                    )?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"command_condition","line":step.line,"command":call.operation,"matched":matched}),
                    )?;
                    if !matched {
                        jump_to = Some(index + otherwise);
                    }
                }
                StepKind::Otherwise { end } => jump_to = Some(index + end),
                StepKind::OnInterrupt { end } => {
                    if !signals.drain_body_runs() {
                        jump_to = Some(index + end);
                    } else if signals.begin_drain() {
                        // The interrupt arrived after the last statement boundary: the
                        // body still runs, and the run ends with the interrupt status.
                        control_state.set_exit(ProgramExit {
                            status: drain_exit_status(),
                            message: None,
                        });
                    }
                }
                StepKind::EndIf => {}
                StepKind::CallProgram {
                    call,
                    inputs,
                    connection,
                    end,
                } => {
                    let child = calls::bind(call, inputs, &bindings).with_context(|| {
                        format!("program {:?}, called at line {}", call.operation, step.line)
                    })?;
                    let job_scope = job_supervisor.enter_scope()?;
                    control_state.enter_call(index + end);
                    call_frames.push(calls::Frame {
                        name: &call.operation,
                        line: step.line,
                        binding: call.binding.as_deref(),
                        job_scope,
                        bindings: std::mem::replace(&mut bindings, child),
                        cwd: cwd.duplicate()?,
                    });
                    if *connection == Connection::Start {
                        data = Input::Empty;
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"program_enter", "program":call.operation, "line":step.line}),
                    )?;
                }
                StepKind::EndCall => {
                    let scope = call_frames
                        .last()
                        .context("program end without call")?
                        .job_scope;
                    job_supervisor.finish_scope(scope).with_context(|| {
                        format!("line {}: closing reusable-program job scope", step.line)
                    })?;
                    control_state.leave_call(index + 1)?;
                    let frame = call_frames.pop().context("program return without call")?;
                    bindings = frame.bindings;
                    cwd = frame.cwd;
                    if let Some(name) = frame.binding {
                        let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                            options.max_output_bytes,
                            signals,
                            options.command_timeout,
                        )?;
                        bindings.insert(
                            name.to_owned(),
                            Value::String(
                                String::from_utf8(bytes.clone())
                                    .context("named program results must be UTF-8 text")?,
                            ),
                        );
                        data = Input::Bytes(bytes);
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"program_return", "program":frame.name, "line":frame.line}),
                    )?;
                }
                StepKind::BreakLoop => {
                    let target = control_state.current_loop()?;
                    let action = control_state.transfer(
                        control_flow::TransferKind::Break,
                        target.finish,
                        control_flow::UnwindTo::new(control_state.call_depth(), target.depth - 1),
                    )?;
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::ContinueLoop => {
                    let target = control_state.current_loop()?;
                    let action = control_state.transfer(
                        control_flow::TransferKind::Continue,
                        target.finish - 1,
                        control_flow::UnwindTo::new(control_state.call_depth(), target.depth),
                    )?;
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::Return => {
                    let scope = call_frames
                        .last()
                        .context("return outside a reusable program")?
                        .job_scope;
                    job_supervisor.cancel_scope(scope).with_context(|| {
                        format!("line {}: cancelling jobs before return", step.line)
                    })?;
                    let target = control_state.current_call()?;
                    let action = control_state.transfer(
                        control_flow::TransferKind::Return,
                        target.end_marker,
                        control_flow::UnwindTo::new(target.call_depth, target.loop_depth),
                    )?;
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::Exit { .. } | StepKind::Fail { .. } => {
                    let exit = program_exit(&step.kind, &bindings)
                        .with_context(|| format!("line {}", step.line))?
                        .context("exit handler received a non-exit step")?;
                    lazy_event(options.trace, signals, || match &exit.message {
                        Some(message) => {
                            serde_json::json!({"event":"fail","line":step.line,"status":exit.status,"message_bytes":message.len()})
                        }
                        None => {
                            serde_json::json!({"event":"exit","line":step.line,"status":exit.status})
                        }
                    })?;
                    // Like return, the transfer unwinds crossed finally blocks and
                    // call scopes. Unlike an error it is not caught, and it reaches
                    // the end of the run instead of a loop or call boundary.
                    let action = control_state.transfer(
                        control_flow::TransferKind::Exit,
                        program.steps.len(),
                        control_flow::UnwindTo::new(0, 0),
                    )?;
                    // A cancelled cleanup ignores the transfer; then nothing exits.
                    if !matches!(action, control_flow::ControlAction::Continue { .. }) {
                        control_state.set_exit(exit);
                    }
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::Try {
                    catch,
                    finally,
                    end,
                } => {
                    let catch = catch
                        .map(|offset| {
                            let marker = index + offset;
                            let Some(crate::ast::Step {
                                kind: StepKind::Catch { binding, .. },
                                ..
                            }) = program.steps.get(marker)
                            else {
                                bail!("invalid catch marker at line {}", step.line);
                            };
                            Ok((marker, binding.clone()))
                        })
                        .transpose()?;
                    control_state.enter_try(
                        index,
                        catch,
                        finally.map(|offset| index + offset),
                        index + end,
                    )?;
                }
                StepKind::Catch { .. } => {
                    let action = control_state.catch_marker(index)?;
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::Finally => {
                    let action = control_state.finally_marker(index)?;
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::EndTry => {
                    // `Try.end` is a relative offset to the instruction after
                    // EndTry, so the runtime marker uses the same half-open
                    // boundary convention.
                    let action = control_state.end_try(index + 1)?;
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::UseFolder { path } => {
                    let folder = path_value(path, &bindings, &cwd)?;
                    crate::capabilities::authorize_host_path(
                        registry,
                        &folder,
                        crate::capabilities::HostAccess::Read,
                        &cwd,
                    )?;
                    cwd = enter_directory(registry, &folder, &cwd)?;
                    if !cwd.is_dir() {
                        bail!("line {}: use folder requires a directory", step.line);
                    }
                }
                StepKind::UseValue { source } => {
                    data = Input::Bytes(text_value_bytes(source, &bindings, step.line)?);
                }
                StepKind::CaptureBytes { binding } => {
                    let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                        options.max_output_bytes,
                        signals,
                        options.command_timeout,
                    )?;
                    byte_bindings.insert(binding.clone(), bytes.clone());
                    data = Input::Bytes(bytes);
                }
                StepKind::UseBytes { binding } => {
                    let bytes = byte_bindings.get(binding).cloned().with_context(|| {
                        format!(
                            "line {}: byte binding {binding:?} is not defined",
                            step.line
                        )
                    })?;
                    data = Input::Bytes(bytes);
                }
                StepKind::Read { binding, .. }
                | StepKind::ReadStdin { binding }
                | StepKind::ReadCallInput { binding } => {
                    data = match &step.kind {
                        StepKind::Read { path, .. } => {
                            let path = path_value(path, &bindings, &cwd)?;
                            crate::capabilities::authorize_host_path(
                                registry,
                                &path,
                                crate::capabilities::HostAccess::Read,
                                &cwd,
                            )?;
                            let file = File::open(&path).with_context(|| {
                                format!("line {}: reading {}", step.line, path.display())
                            })?;
                            // The grant covers the file that was opened. Authorize the resolved
                            // path too, and check that the handle is that file, so a link swapped
                            // in after the first check cannot redirect the read.
                            let resolved = fs::canonicalize(&path)?;
                            crate::capabilities::authorize_host_path(
                                registry,
                                &resolved,
                                crate::capabilities::HostAccess::Read,
                                &cwd,
                            )?;
                            verify_opened_identity(&file, &resolved)?;
                            Input::File(file)
                        }
                        StepKind::ReadCallInput { .. } => {
                            std::mem::replace(&mut data, Input::Empty)
                        }
                        _ => Input::Stdin,
                    };
                    if let Some(name) = binding {
                        let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                            options.max_output_bytes,
                            signals,
                            options.command_timeout,
                        )?;
                        bindings.insert(
                            name.clone(),
                            Value::String(
                                String::from_utf8(bytes.clone())
                                    .context("named results must be UTF-8 text")?,
                            ),
                        );
                        data = Input::Bytes(bytes);
                    }
                }
                StepKind::ParseJson { binding } => {
                    let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                        options.max_output_bytes,
                        signals,
                        options.command_timeout,
                    )?;
                    let value = crate::structured::parse(&bytes)
                        .with_context(|| format!("line {}", step.line))?;
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"parse_json", "line":step.line, "input_bytes":bytes.len()}),
                    )?;
                    data = Input::Bytes(bytes);
                }
                StepKind::TakeJson {
                    pointer,
                    source,
                    kind,
                    binding,
                } => {
                    let source = structured_source(source, &bindings)?;
                    let value = crate::structured::take(pointer, &source, *kind, step.line)
                        .with_context(|| format!("line {}", step.line))?;
                    let value = value.clone();
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"take_json", "line":step.line, "type":kind.to_string()}),
                    )?;
                }
                StepKind::TakeJsonDynamic {
                    pointer,
                    source,
                    kind,
                    binding,
                } => {
                    let pointer = resolve(pointer, &bindings)?;
                    let Value::String(pointer) = pointer else {
                        bail!("line {}: dynamic JSON pointer must be text", step.line);
                    };
                    let source = structured_source(source, &bindings)?;
                    let value = crate::structured::take(&pointer, &source, *kind, step.line)
                        .with_context(|| format!("line {}", step.line))?
                        .clone();
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"take_json", "line":step.line, "type":kind.to_string()}),
                    )?;
                }
                StepKind::EncodeJson { source } => {
                    let source = structured_source(source, &bindings)?;
                    let bytes = crate::structured::encode(&source, options.max_output_bytes)
                        .with_context(|| format!("line {}", step.line))?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"encode_json", "line":step.line, "output_bytes":bytes.len()}),
                    )?;
                    data = Input::Bytes(bytes);
                }
                StepKind::TaskGroup {
                    mode,
                    limit,
                    binding,
                    tasks,
                    each,
                } => {
                    let config = tasks::TaskGroupConfig {
                        mode: *mode,
                        limit: *limit,
                        outer_deadline: signals.deadline,
                    };
                    let group = match each {
                        Some(each) => {
                            match checkpoint::elements::group_journal(
                                journal.as_deref_mut(),
                                program,
                                index,
                                each,
                                &bindings,
                                registry,
                                &signals.sensitivity_floor(),
                            )? {
                                Some((mut sink, skip)) => tasks::run_each(
                                    tasks::TaskGroupRequest {
                                        config,
                                        registry,
                                        bindings: &bindings,
                                        options,
                                        cancelled: Arc::clone(&signals.cancelled),
                                        draining: Arc::clone(&signals.drain),
                                        context: AppletInvocationContext::from_signals(signals)?,
                                    },
                                    each,
                                    &tasks[0],
                                    tasks::ElementPlan {
                                        skip,
                                        sink: Some(&mut sink as &mut dyn tasks::GroupSink),
                                    },
                                ),
                                None => tasks::run_each(
                                    tasks::TaskGroupRequest {
                                        config,
                                        registry,
                                        bindings: &bindings,
                                        options,
                                        cancelled: Arc::clone(&signals.cancelled),
                                        draining: Arc::clone(&signals.drain),
                                        context: AppletInvocationContext::from_signals(signals)?,
                                    },
                                    each,
                                    &tasks[0],
                                    tasks::ElementPlan::none(),
                                ),
                            }
                        }
                        None => tasks::run_group(
                            tasks::TaskGroupRequest {
                                config,
                                registry,
                                bindings: &bindings,
                                options,
                                cancelled: Arc::clone(&signals.cancelled),
                                draining: Arc::clone(&signals.drain),
                                context: AppletInvocationContext::from_signals(signals)?,
                            },
                            tasks,
                        ),
                    };
                    let group = match group {
                        Ok(group) => group,
                        Err(error) => {
                            if let Some(failure) = error.downcast_ref::<tasks::TaskGroupFailure>() {
                                result.launched_processes += failure.launched_processes;
                                signals.account_child_process_launches(failure.launched_processes);
                            }
                            return Err(error)
                                .with_context(|| format!("line {}: task group", step.line));
                        }
                    };
                    result.launched_processes += group.launched_processes;
                    signals.account_child_process_launches(group.launched_processes);
                    let value = group.value;
                    let bytes = crate::structured::encode(&value, options.max_output_bytes)
                        .with_context(|| format!("line {}: task group output", step.line))?;
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"task_group","line":step.line,"mode":mode,"tasks":tasks.len(),"output_bytes":bytes.len()}),
                    )?;
                    data = Input::Bytes(bytes);
                }
                StepKind::SpawnJob { name, stages } => {
                    let scope = call_frames.last().map_or(0, |frame| frame.job_scope);
                    match job_supervisor.spawn(
                        scope,
                        name,
                        stages,
                        super::jobs::JobEnv {
                            registry,
                            bindings: &bindings,
                            cwd: &cwd,
                            options,
                            signals,
                        },
                    ) {
                        Ok(launched) => {
                            result.launched_processes += launched;
                            // Job children own independent Signals. Their launch
                            // observer already updates graph metrics at spawn;
                            // mirror the count into this caller's counter for
                            // nested module failure accounting without notifying
                            // that observer a second time.
                            signals.account_child_process_launches(launched);
                        }
                        Err(error) => {
                            let error = if error.downcast_ref::<jobs::JobSpawnFailure>().is_none() {
                                let mut failure = execution_result::result_for_error(&error);
                                failure.effect_certainty =
                                    execution_result::EffectCertainty::NotStarted;
                                anyhow::Error::new(jobs::JobSpawnFailure {
                                    message: format!("{error:#}"),
                                    result: failure,
                                    launched_processes: 0,
                                })
                            } else {
                                error
                            };
                            if let Some(failure) = error.downcast_ref::<jobs::JobSpawnFailure>() {
                                result.launched_processes += failure.launched_processes;
                                signals.account_child_process_launches(failure.launched_processes);
                            }
                            return Err(error).with_context(|| {
                                format!("line {}: starting background job {name:?}", step.line)
                            });
                        }
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"job_spawn","line":step.line,"job":name,"stages":stages.len()}),
                    )?;
                }
                StepKind::InspectJob { name, binding } => {
                    let scope = call_frames.last().map_or(0, |frame| frame.job_scope);
                    let value = job_supervisor
                        .inspect(scope, name)
                        .with_context(|| format!("line {}: inspecting job {name:?}", step.line))?;
                    let bytes = crate::structured::encode(&value, options.max_output_bytes)
                        .with_context(|| format!("line {}: job inspection output", step.line))?;
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"job_inspect","line":step.line,"job":name,"output_bytes":bytes.len()}),
                    )?;
                }
                StepKind::AwaitJob {
                    name,
                    status_binding,
                    result_binding,
                } => {
                    let scope = call_frames.last().map_or(0, |frame| frame.job_scope);
                    let mut terminal = job_supervisor
                        .await_job(scope, name)
                        .with_context(|| format!("line {}: awaiting job {name:?}", step.line))?;
                    let stage_failure = terminal
                        .stages
                        .iter()
                        .take(terminal.stages.len().saturating_sub(1))
                        .find(|stage| {
                            stage.kind.is_some_and(|kind| {
                                kind != execution_result::ExecutionKind::Success
                            })
                        });
                    if let Some(stage) = stage_failure {
                        terminal.kind = execution_result::ExecutionKind::Failure;
                        terminal.message = Some(format!(
                            "background pipeline stage {:?} ended with {:?}",
                            stage.applet, stage.kind
                        ));
                        return Err(execution_result::ExecutionFailure::new(terminal).into());
                    }
                    let kept_terminal_failure = status_binding.is_some()
                        && terminal.status.is_some()
                        && matches!(
                            terminal.kind,
                            execution_result::ExecutionKind::Absence
                                | execution_result::ExecutionKind::Failure
                                | execution_result::ExecutionKind::Retryable
                        );
                    if terminal.kind != execution_result::ExecutionKind::Success
                        && !kept_terminal_failure
                    {
                        if terminal.message.is_none() && terminal.status.is_some() {
                            terminal.message = Some(format!(
                                "background job {name:?} completed with {:?} outcome ({})",
                                terminal.kind,
                                terminal.status.unwrap_or_default()
                            ));
                        }
                        return Err(execution_result::ExecutionFailure::new(terminal).into());
                    }
                    let status = terminal
                        .status
                        .context("background job finished without an exit status")?;
                    if let Some(binding) = status_binding {
                        bindings.insert(binding.clone(), Value::Integer(i64::from(status)));
                    }
                    if let Some(binding) = result_binding {
                        let value =
                            job_supervisor.result_value(scope, name, step.line, &terminal)?;
                        crate::structured::encode(&value, options.max_output_bytes)
                            .with_context(|| format!("line {}: job result record", step.line))?;
                        bindings.insert(binding.clone(), value);
                    }
                    data = Input::Bytes(terminal.stdout.take().unwrap_or_default());
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"job_await","line":step.line,"job":name,"status":status}),
                    )?;
                }
                StepKind::CancelJob { name } => {
                    let scope = call_frames.last().map_or(0, |frame| frame.job_scope);
                    job_supervisor
                        .cancel(scope, name)
                        .with_context(|| format!("line {}: cancelling job {name:?}", step.line))?;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"job_cancel","line":step.line,"job":name}),
                    )?;
                }
                // A replayed run leaves files alone; the pipeline value still flows.
                StepKind::Append { .. } | StepKind::Save { .. } | StepKind::Redirect { .. }
                    if observe::suppresses_writes(step.line) => {}
                StepKind::Append { path } => {
                    let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                        options.max_output_bytes,
                        signals,
                        options.command_timeout,
                    )?;
                    let destination = path_value(path, &bindings, &cwd)?;
                    crate::capabilities::authorize_host_path(
                        registry,
                        &destination,
                        crate::capabilities::HostAccess::Write,
                        &cwd,
                    )?;
                    crate::security::authorize_persistence_set(
                        registry,
                        "workflow.append",
                        crate::security::PersistenceKind::Save,
                        &signals.sensitivity_floor(),
                    )?;
                    let mut file = open_output_file(&destination, true, registry, &cwd)?;
                    append_to_open_file(&mut file, &destination, &bytes)?;
                    if journal.is_some() {
                        File::open(destination.parent().unwrap_or(&cwd))?.sync_all()?;
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"append","line":step.line,"bytes":bytes.len()}),
                    )?;
                    // Like save, the appended value remains the pipeline value.
                    data = Input::Bytes(bytes);
                }
                StepKind::Interactive => {
                    // The next native command takes the terminal; its Invoke step
                    // consumes this request.
                    pending_handoff = true;
                }
                StepKind::HandOver => {
                    // The next native command replaces this script; its Invoke step consumes this.
                    pending_process = Some(handoff::ProcessRequest::HandOver);
                }
                StepKind::LaunchDetached { output } => {
                    pending_process = Some(handoff::ProcessRequest::Detach(output.clone()));
                }
                StepKind::Environment { set, unset } => {
                    let mut overlay = crate::capabilities::CommandEnvOverlay {
                        set: BTreeMap::new(),
                        unset: unset.clone(),
                    };
                    for (name, value) in set {
                        let Value::String(text) = resolve(value, &bindings)
                            .with_context(|| format!("line {}", step.line))?
                        else {
                            bail!(
                                "line {}: environment value for {name:?} must be text",
                                step.line
                            );
                        };
                        overlay.set.insert(name.clone(), text);
                    }
                    pending_env = Some(overlay);
                }
                StepKind::Redirect { target, path } => {
                    // The file is opened now, before the command can start, and the
                    // command's stderr is written through this same handle later.
                    let file = match path {
                        Some(path) => {
                            let destination = path_value(path, &bindings, &cwd)?;
                            let append = *target == crate::ast::RedirectTarget::StderrAppend;
                            Some(open_redirect_target(
                                &destination,
                                append,
                                registry,
                                signals,
                                &cwd,
                            )?)
                        }
                        None => None,
                    };
                    pending_redirect = Some(PendingRedirect {
                        target: *target,
                        file,
                    });
                }
                StepKind::Save { path, .. } => {
                    let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                        options.max_output_bytes,
                        signals,
                        options.command_timeout,
                    )?;
                    let destination = path_value(path, &bindings, &cwd)?;
                    crate::capabilities::authorize_host_path(
                        registry,
                        &destination,
                        crate::capabilities::HostAccess::Write,
                        &cwd,
                    )?;
                    crate::security::authorize_persistence_set(
                        registry,
                        "workflow.save",
                        crate::security::PersistenceKind::Save,
                        &signals.sensitivity_floor(),
                    )?;
                    atomic_write(&destination, &bytes)?;
                    if journal.is_some() {
                        File::open(destination.parent().unwrap_or(&cwd))?.sync_all()?;
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"save","line":step.line,"bytes":bytes.len()}),
                    )?;
                    // Preserve the pipeline value for later steps and final stdout.
                    data = Input::Bytes(bytes);
                }
                StepKind::Invoke { call, connection } => {
                    let mut redirect = pending_redirect.take();
                    let env = pending_env.take();
                    let handoff = std::mem::take(&mut pending_handoff);
                    let process = pending_process.take();
                    let manifest = registry.resolve_call(call)?;
                    if handoff
                        && (manifest.execution_backend() != AppletBackend::Command || call.cache)
                    {
                        bail!(
                            "line {}: interactively applies to one native command that is not cached",
                            step.line
                        );
                    }
                    if let Some(request) = process {
                        let parameters = call
                            .parameters
                            .iter()
                            .map(|(name, value)| Ok((name.clone(), resolve(value, &bindings)?)))
                            .collect::<Result<BTreeMap<_, _>>>()?;
                        let argv = manifest.build_arguments(&parameters)?;
                        let binary = binary_path(manifest.binary_for(call), &cwd)?;
                        let command = crate::capabilities::command_for(
                            &binary,
                            argv.iter().map(|argument| OsStr::new(argument.as_str())),
                            registry,
                            &manifest.name,
                            &cwd,
                        )?;
                        match request {
                            handoff::ProcessRequest::HandOver => {
                                // Output of earlier statements is written now, before the
                                // program takes the stdout; a hand-over never returns to
                                // write it at the end of the run.
                                let pending = std::mem::replace(&mut data, Input::Empty)
                                    .materialize(
                                        options.max_output_bytes,
                                        signals,
                                        options.command_timeout,
                                    )?;
                                std::io::stdout()
                                    .write_all(&pending)
                                    .with_context(|| format!("line {}: hand over", step.line))?;
                                let code = handoff::hand_over(command)
                                    .with_context(|| format!("line {}: hand over", step.line))?;
                                // Windows exit codes outside 0-255 cannot be represented
                                // by a script exit and are reported as 1.
                                control_state.set_exit(ProgramExit {
                                    status: u8::try_from(code).unwrap_or(1),
                                    message: None,
                                });
                            }
                            handoff::ProcessRequest::Detach(output) => {
                                detached_launches += 1;
                                if detached_launches > handoff::MAX_DETACHED_LAUNCHES {
                                    bail!(
                                        "line {}: launch detached is limited to {} detached programs per run",
                                        step.line,
                                        handoff::MAX_DETACHED_LAUNCHES
                                    );
                                }
                                // The output is a write target exactly like a redirect.
                                let output = match output {
                                    Some(path) => {
                                        let destination = path_value(&path, &bindings, &cwd)?;
                                        let file = open_redirect_target(
                                            &destination,
                                            false,
                                            registry,
                                            signals,
                                            &cwd,
                                        )?;
                                        file.set_len(0).with_context(|| {
                                            format!("truncating {}", destination.display())
                                        })?;
                                        Some(file)
                                    }
                                    None => None,
                                };
                                handoff::launch_detached(command, output).with_context(|| {
                                    format!("line {}: launch detached", step.line)
                                })?;
                                result.launched_processes += 1;
                                signals.account_child_process_launches(1);
                                data = Input::Empty;
                            }
                        }
                    } else if manifest.builtin == Some(Builtin::Cd) {
                        let parameters = call
                            .parameters
                            .iter()
                            .map(|(name, value)| Ok((name.clone(), resolve(value, &bindings)?)))
                            .collect::<Result<BTreeMap<_, _>>>()?;
                        let args = manifest.build_arguments(&parameters)?;
                        let destination =
                            cwd.join(crate::capabilities::expand_home_path(&args[0])?);
                        cwd = enter_directory(registry, &destination, &cwd)
                            .with_context(|| format!("line {}: cd to {:?}", step.line, args[0]))?;
                        if !cwd.is_dir() {
                            bail!("line {}: cd requires a directory", step.line);
                        }
                        data = Input::Empty;
                    } else {
                        let input = if *connection != Connection::Start && manifest.accepts_stdin {
                            std::mem::replace(&mut data, Input::Empty)
                        } else {
                            Input::Empty
                        };
                        // A redirected command's stdout is pipeline data, so it is
                        // never inherited or streamed.
                        let streamed = redirect.is_none()
                            && stream_top
                            && streams_terminal_stdout(program, index);
                        let mut calls = vec![(call, manifest, step.line)];
                        while pipe_at(program, index + 1) {
                            index += 1;
                            let next = &program.steps[index];
                            if let StepKind::Invoke { call, .. } = &next.kind {
                                calls.push((call, registry.resolve_call(call)?, next.line));
                            }
                        }
                        if calls.len() > 1
                            && calls.iter().any(|(stage, _, _)| stage.retry.is_some())
                        {
                            bail!(
                                "line {}: `and retry` cannot apply to a command in a pipeline",
                                step.line
                            );
                        }
                        let output_call = calls.last().expect("invocation group is nonempty").0;
                        let output_manifest = calls.last().expect("invocation group is nonempty").1;
                        let (bytes, status, stdout_captured, stages, typed_json) = if call.cache {
                            let input = input.materialize(
                                options.max_output_bytes,
                                signals,
                                options.command_timeout,
                            )?;
                            let argv = arguments_for_call(call, manifest, &bindings)?;
                            let module_session =
                                if manifest.execution_backend() == AppletBackend::ProgramModule {
                                    Some(current_module_session(signals)?)
                                } else {
                                    None
                                };
                            let module_fingerprint = module_session
                                .as_ref()
                                .map(|session| {
                                    session
                                        .get_for_manifest(registry, manifest)
                                        .context(
                                            "program module cache key requires a preloaded module",
                                        )
                                        .map(|loaded| loaded.checkpoint_fingerprint.clone())
                                })
                                .transpose()?;
                            let resolved_module_parameters = if module_fingerprint.is_some() {
                                let parameters =
                                    resolved_json_arguments(call, manifest, &bindings)?;
                                Some(serde_json::to_vec(&parameters)?)
                            } else {
                                None
                            };
                            let module_cache_is_closed = module_session
                                .as_deref()
                                .map(program_module_cache_is_closed)
                                .transpose()?
                                .unwrap_or(true);
                            // Program modules are already loaded and pinned by
                            // the run-scoped module session; their sentinel
                            // descriptor is not an executable identity.
                            let executable = if module_fingerprint.is_some() {
                                PathBuf::new()
                            } else {
                                binary_path(manifest.binary_for(call), &cwd)?
                            };
                            let directory = cache_override
                                .clone()
                                .unwrap_or_else(|| cwd.join(".flow-cache"));
                            let sensitivity = signals.sensitivity_floor();
                            let cache_path_allowed =
                                crate::capabilities::authorize_applet_host_path(
                                    registry,
                                    &manifest.name,
                                    &directory,
                                    crate::capabilities::HostAccess::Read,
                                    &cwd,
                                )
                                .is_ok()
                                    && crate::capabilities::authorize_applet_host_path(
                                        registry,
                                        &manifest.name,
                                        &directory,
                                        crate::capabilities::HostAccess::Write,
                                        &cwd,
                                    )
                                    .is_ok();
                            let cache_allowed = module_cache_is_closed
                                && cache_path_allowed
                                && crate::security::authorize_cache_purity(registry, manifest)
                                    .is_ok()
                                && crate::security::authorize_persistence_set(
                                    registry,
                                    &manifest.name,
                                    crate::security::PersistenceKind::Cache,
                                    &sensitivity,
                                )
                                .is_ok();
                            let dependencies_declared = !manifest.cache_dependencies().is_empty();
                            let dependency_fingerprint = if !dependencies_declared {
                                Some([0; 32])
                            } else if cache_allowed {
                                cache::dependency_fingerprint(manifest, &cwd)?
                            } else {
                                None
                            };
                            let mut key = match (cache_allowed, dependency_fingerprint) {
                                (true, Some(fingerprint)) => {
                                    let key = if let Some(module_fingerprint) =
                                        module_fingerprint.as_deref()
                                    {
                                        cache_key_for_program_module(
                                            registry,
                                            manifest,
                                            &argv,
                                            resolved_module_parameters
                                                .as_deref()
                                                .context(
                                                    "program module cache key requires resolved parameters",
                                                )?,
                                            &cwd,
                                            &input,
                                            module_fingerprint,
                                        )?
                                    } else {
                                        cache_key_for_applet(
                                            registry,
                                            manifest,
                                            &argv,
                                            &executable,
                                            &cwd,
                                            &input,
                                        )?
                                    };
                                    Some(if manifest.cache_dependencies().is_empty() {
                                        key
                                    } else {
                                        key_with_dependency_fingerprint(&key, fingerprint)
                                    })
                                }
                                _ => None,
                            };
                            let run_uncached = || {
                                execute_call_group(GroupRequest {
                                    calls: &calls,
                                    input: Input::Bytes(input),
                                    prepared_first_call: Some((executable, argv)),
                                    bindings: &bindings,
                                    cwd: &cwd,
                                    options,
                                    launched: &mut result.launched_processes,
                                    signals,
                                    registry,
                                })
                            };
                            let mut hit = if let Some(cache_key) = key.as_deref() {
                                match cache::open(&directory, cache_key)? {
                                    Some(file) => {
                                        read_cached_output(manifest, file, options.max_output_bytes)
                                            .ok()
                                    }
                                    None => None,
                                }
                            } else {
                                None
                            };
                            if hit.is_some()
                                && dependencies_declared
                                && !dependency_fingerprint.is_some_and(|expected| {
                                    dependency_fingerprint_matches(manifest, &cwd, expected)
                                })
                            {
                                // Do not accept a result if any declared file
                                // changed or became unsafe since the key was
                                // first computed. Execute normally without
                                // storing under the stale identity.
                                hit = None;
                                key = None;
                            }
                            if let Some((bytes, status)) = hit {
                                result.cache_hits += 1;
                                let cache_key = key.as_deref().expect("cache key for hit");
                                lazy_event(
                                    options.trace,
                                    signals,
                                    || serde_json::json!({"event":"cache_hit","line":step.line,"applet":manifest.name_for(call),"key":cache_key,"bytes":bytes.len()}),
                                )?;
                                // Cache entries retain terminal bytes/status,
                                // not historical per-stage diagnostics.
                                (bytes, status, true, Vec::new(), None)
                            } else {
                                let completed = run_uncached()?;
                                if let Some(cache_key) = key.take() {
                                    let dependencies_unchanged = !dependencies_declared
                                        || dependency_fingerprint.is_some_and(|expected| {
                                            dependency_fingerprint_matches(manifest, &cwd, expected)
                                        });
                                    if dependencies_unchanged {
                                        if let Ok(entry) = cache_output(
                                            manifest,
                                            &completed.bytes,
                                            completed.status,
                                        ) {
                                            let stored =
                                                cache::write(&directory, &cache_key, &entry)?;
                                            lazy_event(
                                                options.trace,
                                                signals,
                                                || serde_json::json!({"event":if stored {"cache_store"} else {"cache_skip"},"line":step.line,"applet":manifest.name_for(call),"key":cache_key,"bytes":completed.bytes.len()}),
                                            )?;
                                        }
                                    }
                                }
                                (
                                    completed.bytes,
                                    completed.status,
                                    completed.stdout_captured,
                                    completed.stages,
                                    completed.typed_json,
                                )
                            }
                        } else {
                            // An observer needs the bytes, so a streamed step is captured
                            // and written after it completes, as streaming already does.
                            let _inherit = InheritTerminalStdout::enter(
                                streamed && !observe::captures_streamed_stdout(),
                            );
                            let _env = crate::capabilities::CommandEnvScope::enter(env);
                            let _handoff = terminal::Scope::enter(handoff);
                            let _stderr = ForceStderrCapture::enter(
                                redirect
                                    .as_ref()
                                    .is_some_and(PendingRedirect::captures_stderr),
                            );
                            let completed = match execute_call_group(GroupRequest {
                                calls: &calls,
                                input,
                                prepared_first_call: None,
                                bindings: &bindings,
                                cwd: &cwd,
                                options,
                                launched: &mut result.launched_processes,
                                signals,
                                registry,
                            }) {
                                Ok(completed) => completed,
                                Err(error) => {
                                    // A failed command's stderr still goes to its
                                    // file target before the failure propagates.
                                    if let (Some(redirect), Some(failure)) =
                                        (redirect.as_mut(), error.downcast_ref::<CommandFailed>())
                                    {
                                        let stderr = failure.stderr.clone().unwrap_or_else(|| {
                                            failure.diagnostic.clone().into_bytes()
                                        });
                                        redirect.write_stderr(&stderr)?;
                                    }
                                    return Err(error);
                                }
                            };
                            (
                                completed.bytes,
                                completed.status,
                                completed.stdout_captured,
                                completed.stages,
                                completed.typed_json,
                            )
                        };
                        let stderr = stages
                            .iter()
                            .rev()
                            .find_map(|stage| stage.stderr.as_deref());
                        // Taken before `stages` moves into the execution record.
                        let redirected_stderr = match &redirect {
                            Some(_) => stages
                                .iter()
                                .rev()
                                .find_map(|stage| stage.stderr.clone())
                                .unwrap_or_default(),
                            None => Vec::new(),
                        };
                        if let Some(name) = &call.status_binding {
                            let code = status.context("applet finished without an exit code")?;
                            bindings.insert(name.clone(), Value::Integer(i64::from(code)));
                        }
                        if output_call.result_binding.is_some() && status.is_none() {
                            let mut failure = execution_result::ExecutionResult::runtime_failure(
                                execution_result::ExecutionKind::UnknownError,
                                "applet completed without an exit status; cannot capture a successful execution result",
                                execution_result::EffectCertainty::Uncertain,
                            );
                            failure.stages = stages;
                            return Err(execution_result::ExecutionFailure::new(failure).into());
                        }
                        // A streamed step's stdout went to the terminal rather than into the
                        // result, so its execution record reports no captured output.
                        let reported_stdout_captured = stdout_captured && !streamed;
                        let output_value = if let Some(name) = &output_call.binding {
                            bind_call_output(
                                name,
                                CallOutput {
                                    manifest: output_manifest,
                                    line: calls.last().expect("invocation group is nonempty").2,
                                    bytes: &bytes,
                                    status,
                                    stdout_captured,
                                    stderr,
                                    stages: &stages,
                                    typed_json,
                                },
                                output_call.result_binding.is_some(),
                                &mut bindings,
                            )?
                        } else if output_call.result_binding.is_some()
                            && stdout_captured
                            && status.is_some_and(|status| {
                                output_manifest.result_envelope()
                                    || (output_manifest.outcome_for(status).kind
                                        == OutcomeKind::Success
                                        && output_manifest.output_kind().is_some())
                            })
                        {
                            Some(decode_call_output(CallOutput {
                                manifest: output_manifest,
                                line: calls.last().expect("invocation group is nonempty").2,
                                bytes: &bytes,
                                status,
                                stdout_captured,
                                stderr,
                                stages: &stages,
                                typed_json,
                            })?)
                        } else {
                            None
                        };
                        if let Some(status) = status {
                            let outcome = output_manifest.outcome_for(status);
                            let line = calls.last().expect("invocation group is nonempty").2;
                            let applet = output_manifest.name_for(output_call);
                            let history_record = execution_history.push_outcome(
                                line,
                                applet,
                                &outcome,
                                reported_stdout_captured.then_some(bytes.as_slice()),
                                stderr,
                                &stages,
                            );
                            if options.trace {
                                let execution_value = if let Some(record) = history_record {
                                    record.to_value()
                                } else {
                                    execution_result::ExecutionRecord::from_outcome(
                                        line,
                                        applet,
                                        &outcome,
                                        reported_stdout_captured.then_some(bytes.as_slice()),
                                        stderr,
                                        &stages,
                                    )
                                    .to_value()
                                };
                                let execution_json = crate::structured::to_json(&execution_value)?;
                                lazy_event(true, signals, || {
                                    serde_json::json!({
                                        "event": "execution_result",
                                        "execution": execution_json,
                                    })
                                })?;
                            }
                            if let Some(name) = &output_call.result_binding {
                                // The full output bytes are retained only when
                                // the source program explicitly asks for them.
                                let execution = execution_result::ExecutionResult::from_outcome(
                                    outcome,
                                    reported_stdout_captured.then(|| bytes.clone()),
                                    stderr.map(<[u8]>::to_vec),
                                    output_value,
                                    stages,
                                );
                                let record = execution.to_source_value(line, applet);
                                bindings.insert(name.clone(), record);
                            }
                        }
                        let bytes = match &mut redirect {
                            Some(redirect) => redirect.route(bytes, &redirected_stderr, signals)?,
                            None => bytes,
                        };
                        if streamed {
                            // Nothing consumes this value, so it is written now
                            // and the pipeline value becomes empty.
                            if !observe::suppresses_terminal_output(step.line) {
                                write_streamed_stdout(&bytes, signals)?;
                            }
                            data = Input::Empty;
                        } else {
                            data = Input::Bytes(bytes);
                        }
                    }
                }
            }
            Ok(())
        })();
        // A declared retry runs the command again from its first marker. The journal
        // node stays open, because the step has not completed.
        let execution = match execution {
            Err(error) if control_rethrow.is_none() => {
                match plan_retry(program, index, &error, registry, &mut retry_failures)? {
                    RetryPlan::NotDeclared => Err(error),
                    RetryPlan::Refused(reason) => {
                        Err(error.context(format!("line {}: retry refused: {reason}", step.line)))
                    }
                    RetryPlan::Exhausted { attempts } => Err(error.context(format!(
                        "line {}: retry exhausted after {attempts} attempts",
                        step.line
                    ))),
                    RetryPlan::Again {
                        restart,
                        delay,
                        attempt,
                    } => {
                        if let Some(failure) = error.downcast_ref::<CommandFailed>() {
                            if let Some(outcome) = &failure.outcome {
                                execution_history.push_outcome(
                                    failure.line,
                                    failure.operation.as_str(),
                                    outcome,
                                    None,
                                    failure.stderr.as_deref(),
                                    &failure.stages,
                                );
                            }
                        }
                        lazy_event(options.trace, signals, || {
                            serde_json::json!({
                                "event": "retry",
                                "line": step.line,
                                "attempt": attempt,
                                "delay_ms": u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                            })
                        })?;
                        wait_before_retry(signals, delay)?;
                        index = restart;
                        continue;
                    }
                }
            }
            other => {
                if other.is_ok() {
                    retry_failures = None;
                }
                other
            }
        };
        if let Some(error) = control_rethrow {
            if let Some(journal) = journal.as_deref_mut() {
                if !journal.persistence_failed() {
                    journal.fail(
                        &format!("{error:#}"),
                        parent_signals.cancelled.load(Ordering::Relaxed)
                            || is_timeout_error(&error)
                            || journal_uncertain,
                    )?;
                }
            }
            return Err(error);
        }
        if let Err(error) = execution {
            let error = call_frames.iter().rev().fold(error, |error, frame| {
                error.context(format!(
                    "program {:?}, called at line {}",
                    frame.name, frame.line
                ))
            });
            let cancellation = !was_cleanup && parent_signals.cancelled.load(Ordering::Relaxed);
            let action = control_state.fail(error, cancellation)?;
            match apply_control_action(action, control_context!())? {
                AppliedControl::Continue => {}
                AppliedControl::Jump(target) => jump_to = Some(target),
                AppliedControl::Rethrow(error) => {
                    if let Some(journal) = journal.as_deref_mut() {
                        if !journal.persistence_failed() {
                            journal.fail(
                                &format!("{error:#}"),
                                cancellation
                                    || parent_signals.cancelled.load(Ordering::Relaxed)
                                    || is_timeout_error(&error)
                                    || journal_uncertain,
                            )?;
                        }
                    }
                    return Err(error);
                }
            }
        }
        // An exit ends the run at the program boundary. Checkpoint validation
        // allows it only as a top-level statement, so the in-flight node is the
        // exit step itself and completes here as the terminal node.
        let exited = control_state.exit_pending();
        index = jump_to.unwrap_or(index + 1);
        if let Some(journal) = journal
            .as_deref_mut()
            .filter(|_| journal_end == Some(index) || (exited && journal_end.is_some()))
        {
            journal_end = None;
            journal_uncertain = false;
            authorize_checkpoint_persistence(registry, &signals.sensitivity_floor())?;
            let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                options.max_output_bytes,
                signals,
                options.command_timeout,
            )?;
            journal.complete_node_parts(
                &cwd,
                &bytes,
                bindings.iter().map(|(name, value)| (name.as_str(), value)),
            )?;
            data = Input::Bytes(bytes);
            lazy_event(
                options.trace,
                signals,
                || serde_json::json!({"event":"checkpoint_commit","completed_nodes":journal.completed_nodes().len(),"next_step":index}),
            )?;
        }
    }
    let exit = control_state.take_exit();
    if exit.is_some() {
        // An explicit exit owns its remaining jobs: cancel and reap them rather
        // than requiring each handle to be awaited first.
        job_supervisor
            .cancel_scope(0)
            .context("cancelling background jobs at program exit")?;
    } else {
        job_supervisor
            .finish_scope(0)
            .context("closing finite-program background job scope")?;
    }
    result.exit = exit;
    result.output = std::mem::replace(&mut data, Input::Empty).materialize(
        options.max_output_bytes,
        signals,
        options.command_timeout,
    )?;
    #[cfg(feature = "live")]
    if let Some(state) = state_out {
        *state = live::NodeState {
            cwd: cwd.to_path_buf(),
            bindings,
            data: result.output.clone(),
        };
    }
    result.executions = execution_history.executions;
    result.dropped_executions = execution_history.dropped_executions;
    result.dropped_stages = execution_history.dropped_stages;
    Ok(result)
}
