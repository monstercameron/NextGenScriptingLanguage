//! The finite step loop: the statement dispatch that runs one program's steps,
//! its control flow, and the step arms for every statement kind.

use super::*;

pub(super) fn merge_environment_overlay(
    target: &mut crate::capabilities::CommandEnvOverlay,
    update: crate::capabilities::CommandEnvOverlay,
) {
    for name in update.unset {
        target
            .set
            .retain(|existing, _| !same_environment_variable(existing, &name));
        if !target
            .unset
            .iter()
            .any(|existing| same_environment_variable(existing, &name))
        {
            target.unset.push(name);
        }
    }
    for (name, value) in update.set {
        target
            .unset
            .retain(|existing| !same_environment_variable(existing, &name));
        target
            .set
            .retain(|existing, _| !same_environment_variable(existing, &name));
        target.set.insert(name, value);
    }
}

#[cfg(windows)]
fn same_environment_variable(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(not(windows))]
fn same_environment_variable(left: &str, right: &str) -> bool {
    left == right
}

fn normalize_environment_variable(name: &str) -> String {
    #[cfg(windows)]
    {
        name.to_ascii_uppercase()
    }
    #[cfg(not(windows))]
    {
        name.to_owned()
    }
}

fn value_references_sensitive(value: &Value, sensitive: &BTreeSet<String>) -> bool {
    match value {
        Value::Reference(name) => sensitive.contains(name),
        Value::List(values) => values
            .iter()
            .any(|value| value_references_sensitive(value, sensitive)),
        Value::Record(values) => values
            .values()
            .any(|value| value_references_sensitive(value, sensitive)),
        _ => false,
    }
}

// Precompute each name's last possible source-level use once. Job operations
// cannot occur directly in loops; repeated calls have fresh job scopes. Names in
// nested scopes can only delay cleanup.
fn job_last_uses(steps: &[crate::ast::Step]) -> BTreeMap<String, usize> {
    let mut last = BTreeMap::new();
    for (index, step) in steps.iter().enumerate() {
        let name = match &step.kind {
            StepKind::InspectJob { name, .. }
            | StepKind::AwaitJob { name, .. }
            | StepKind::CancelJob { name } => name,
            _ => continue,
        };
        last.insert(name.clone(), index);
    }
    last
}

fn current_job_scope(
    control_state: &control_flow::RuntimeState,
    call_frames: &[calls::Frame<'_>],
    loop_frames: &loops::LoopStack,
) -> usize {
    control_state
        .job_scopes(call_frames.len())
        .into_iter()
        .chain(loop_frames.job_scopes())
        .chain(call_frames.last().map(|frame| frame.job_scope))
        .max()
        .unwrap_or(0)
}

fn visible_job_scopes(
    control_state: &control_flow::RuntimeState,
    call_frames: &[calls::Frame<'_>],
    loop_frames: &loops::LoopStack,
) -> Vec<usize> {
    let mut scopes = control_state.job_scopes(call_frames.len());
    scopes.extend(loop_frames.job_scopes());
    scopes.extend(call_frames.last().map(|frame| frame.job_scope));
    if call_frames.is_empty() {
        scopes.push(0);
    }
    scopes
}

/// Sync the directory entry after a checkpointed file write where the platform
/// supports opening directories as ordinary files. Windows requires backup
/// semantics flags to open a directory; the file itself is synced by its writer.
fn sync_destination_parent(destination: &Path, cwd: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let resolved = if destination.is_absolute() {
            destination.to_path_buf()
        } else {
            cwd.join(destination)
        };
        let parent = resolved.parent().unwrap_or(cwd);
        File::open(parent)?.sync_all()?;
    }
    #[cfg(windows)]
    {
        let _ = (destination, cwd);
    }
    Ok(())
}

/// Add the program's declared inputs to `bindings`: supplied values where no binding
/// exists yet, then every declaration resolved (defaults included).
fn merge_declared_inputs(
    program: &Program,
    options: &RunOptions,
    bindings: &mut BTreeMap<String, Value>,
) -> Result<()> {
    if program.inputs.is_empty() {
        return Ok(());
    }
    for (name, value) in &options.inputs {
        bindings
            .entry(name.clone())
            .or_insert_with(|| value.clone());
    }
    let resolved = crate::inputs::resolve(&program.inputs, bindings, true)?;
    bindings.extend(resolved);
    Ok(())
}

/// The exit for a run whose cleanup overran its step budget after its statements all
/// completed, or `None` when the error is anything else.
fn cleanup_overrun_exit(error: &anyhow::Error) -> Option<ProgramExit> {
    let overrun = error
        .root_cause()
        .downcast_ref::<control_flow::CleanupBudgetExceeded>()?;
    overrun.after_success.then(|| ProgramExit {
        status: 1,
        message: Some(format!(
            "execute: {}",
            crate::terminal_text::escape_controls(&format!(
                "{error:#}; the statements before the cleanup completed, so their output was kept"
            ))
        )),
    })
}

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
    pub(super) job_supervisor: Option<&'a mut jobs::Supervisor>,
    pub(super) job_last_uses: Option<BTreeMap<String, usize>>,
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
            job_supervisor: None,
            job_last_uses: None,
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
        job_supervisor: external_job_supervisor,
        job_last_uses: source_job_last_uses,
    } = start;
    // Literal components of explicitly secret-producing calls must be masked
    // before their invocation trace or any dynamic binding can be observed.
    crate::security::register_sensitive_program(registry, program)?;
    let sensitive_bindings = crate::runtime::sensitive_binding_names(program, registry);
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
    // Live snapshots contain bindings and data, not worker handles. A job is
    // owned by this finite live execution: it may continue while the user has
    // paused at a live breakpoint, and must be awaited or cancelled before the
    // execution completes. Dropping the supervisor on stop or error reaps it.
    if (!signals.background_jobs_allowed
        || (journal.is_some() && !resume::checkpoint_jobs_supported(program)))
        && (has_job_operations(program) || module_session.has_background_job_operations())
    {
        bail!(
            "finite-scope background job handles are unsupported in this execution lifetime, including imported program modules"
        );
    }
    let mut owned_job_supervisor = external_job_supervisor
        .is_none()
        .then(|| jobs::Supervisor::new(signals, options.max_output_bytes))
        .transpose()?;
    let mut job_supervisor = match external_job_supervisor {
        Some(supervisor) => supervisor,
        None => owned_job_supervisor
            .as_mut()
            .expect("new job supervisor was just installed"),
    };
    let last_job_uses = source_job_last_uses.unwrap_or_else(|| job_last_uses(&program.steps));
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
    let mut pipeline_sensitive = false;
    // Set by a `Redirect` step and consumed by the very next `Invoke`.
    let mut pending_redirect: Option<PendingRedirect> = None;
    // Set by an `Environment` step and consumed by the next command.
    let mut pending_env: Option<crate::capabilities::CommandEnvOverlay> = None;
    // `use environment` overlays last through the current reusable-program
    // scope. Calls inherit them and restore their caller's overlay on return.
    let mut workflow_env = crate::capabilities::CommandEnvOverlay::default();
    let mut workflow_env_scopes = Vec::new();
    let mut pending_environment_sensitive = false;
    let mut workflow_environment_sensitive = false;
    let mut workflow_environment_sensitive_scopes = Vec::new();
    let mut pending_handoff = false;
    let mut pending_process: Option<handoff::ProcessRequest> = None;
    // Detached launches started in this run, bounded by handoff::MAX_DETACHED_LAUNCHES.
    let mut detached_launches = 0usize;
    let mut result = RunResult {
        output: Vec::new(),
        binding_types: BTreeMap::new(),
        cache_hits: 0,
        launched_processes: 0,
        executions: Vec::new(),
        dropped_executions: 0,
        dropped_stages: 0,
        exit: None,
    };
    let mut execution_history = execution_result::ExecutionHistory::default();
    // This name cannot be written in source. Checkpoints retain stdout eligibility
    // separately from pipeline bytes, which remain available to later consumers.
    const STDOUT_ELIGIBILITY: &str = "@execute/program-stdout";
    let mut program_stdout = true;
    let mut last_program_stdout_line = None;
    // The step index of a declared retry in progress, and its failed attempts so far.
    let mut retry_failures: Option<(usize, u32)> = None;
    let mut index = 0;
    let mut call_frames: Vec<calls::Frame<'_>> = Vec::new();
    // The target call depth for a captured native failure bubbling through
    // nested calls. EndCall markers restore each lexical environment in turn.
    let mut pending_status_unwind: Option<(usize, usize)> = None;
    let mut bound_call_depths = Vec::new();
    let mut loop_frames = loops::LoopStack::with_policy(
        options.max_loop_iterations,
        options.loop_budget_scope == LoopBudgetScope::LegacySharedRun,
    );
    let mut temp_scopes = temp::ScopeStack::default();
    let mut control_state = control_flow::RuntimeState::default();
    if let Some(journal) = journal.as_deref_mut() {
        if state_source == FiniteStateSource::Restore {
            let mut state = journal.take_restored_state()?;
            state.bindings.remove(STDOUT_ELIGIBILITY);
            // Restored bytes remain pipeline input, but a committed value was
            // already delivered by its original run. Only newly executed value
            // producers may make the resumed run's final output eligible.
            program_stdout = false;
            // The declared inputs join the restored bindings before anything reads
            // them: with no completed node yet, the restored state is the initial
            // one, and a first loop over an input resolves its list from it.
            merge_declared_inputs(program, options, &mut state.bindings)?;
            index = journal.completed_nodes().last().map_or(0, |node| node.end);
            // A top-level for each with committed elements continues after them.
            if let Some(resumed) =
                checkpoint::elements::for_each_resume(journal, program, index, &state, registry)?
            {
                loop_frames
                    .restore_for_each(
                        index,
                        &resumed.binding,
                        resumed.outer,
                        resumed.items,
                        resumed.consumed,
                    )
                    .with_context(|| {
                        format!("line {}: restoring loop", program.steps[index].line)
                    })?;
                state = resumed.state;
                state.bindings.remove(STDOUT_ELIGIBILITY);
            }
            cwd = enter_directory(registry, &state.cwd, &cwd)?;
            bindings = state.bindings;
            data = Input::Bytes(state.output);
            workflow_env = resume::restored_workflow_environment(program, index, &bindings)?;
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
            // A resume inside a try body without a catch (an `on exit` program is
            // one) enters its frame again, so its cleanup runs when the body ends.
            for (header, finally, end) in resume::frame_tries_at(program, index)? {
                control_state.enter_try(header, None, finally, end)?;
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
    merge_declared_inputs(program, options, &mut bindings)?;
    // Inputs are merged only after the initial state has been assembled. Register
    // their resolved values before any workflow step can emit a diagnostic or
    // persist a derived value.
    for input in program
        .inputs
        .iter()
        .filter(|input| crate::ast::is_sensitive(input))
    {
        if let Some(value) = bindings.get(&input.name) {
            crate::security::register_sensitive_value(registry, value)?;
        }
    }
    if preflight_context {
        preflight_inputs_with_loop_limit(
            program,
            registry,
            &bindings,
            options.max_loop_iterations,
        )?;
    }
    let mut journal_end = None;
    let mut journal_start = 0;
    let mut journal_uncertain = false;
    // Set once a failure or cancellation leaves its node for cleanup outside it. The
    // failed node stays in flight and nothing after it is recorded: the run ends
    // failed, and a resume retries that node and runs the cleanup again.
    let mut journal_suspended = false;
    // Set once a repeat interrupt during cleanup is recorded, so the diagnostic
    // names the interruption exactly once however many statements fail fast after it.
    let mut cleanup_interrupted_reported = false;
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
                    journal_suspended = true;
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
        // A repeat interrupt during cleanup aborts the cleanup at once instead
        // of running out its budget: the running command is already being
        // killed through the shared flag, and the remaining statements fail
        // fast until the cleanup frames unwind.
        if let Some(cleanup) = &cleanup_signals {
            if !cleanup_interrupted_reported && cleanup.is_cancelled() {
                cleanup_interrupted_reported = true;
                let action = control_state.fail(cleanup_interrupted(), true)?;
                match apply_control_action(action, control_context!())? {
                    AppliedControl::Continue => {}
                    AppliedControl::Jump(target) => {
                        journal_suspended = true;
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
        }
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
                status: parent_signals.drain_exit_status(),
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
                    journal_suspended = true;
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
        if let Some(journal) = journal
            .as_deref_mut()
            .filter(|_| journal_end.is_none() && !journal_suspended)
        {
            let end = resume::node_end(program, index);
            journal_end = Some(end);
            journal_start = index;
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
                        | StepKind::Try { catch: Some(_), .. }
                        | StepKind::Finally
                ) || resume::uncertain_on_failure(program, index, end, registry)?);
            let module_session = current_module_session(signals)?;
            let fingerprint = resume::node_fingerprint_with_modules_and_bindings(
                program,
                index,
                end,
                registry,
                &cwd,
                module_session.fingerprints(),
                &bindings,
            )?;
            let step_id = journal_step_ids
                .as_ref()
                .and_then(|ids| ids.get(index))
                .cloned();
            // Mark element nodes by kind so resolve and resume can scope
            // decisions without the program: sequential loops replay committed
            // output and bindings per element, parallel groups do not.
            let element_kind = if tracks_elements {
                match &step.kind {
                    StepKind::TaskGroup { each: Some(_), .. } => {
                        Some(checkpoint::ElementNodeKind::ParallelForEach)
                    }
                    _ => Some(checkpoint::ElementNodeKind::ForEach),
                }
            } else {
                None
            };
            journal.begin_node_record(checkpoint::NodeBegin {
                fingerprint: &fingerprint,
                start: index,
                end,
                line: step.line,
                cwd_before: &cwd,
                retry_safe: tracks_elements || resume::retry_safe(program, index, end, registry)?,
                step_id,
                element_kind,
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
                                        environment: (!workflow_env.set.is_empty()
                                            || !workflow_env.unset.is_empty())
                                        .then(|| workflow_env.clone()),
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
                        let parent = current_job_scope(&control_state, &call_frames, &loop_frames);
                        let scope = job_supervisor.enter_scope(parent)?;
                        loop_frames.set_job_scope(index, parent, scope)?;
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
                    if let Some((parent, scope)) = loop_frames
                        .end_iteration(header, &mut bindings)
                        .with_context(|| format!("line {}", step.line))?
                    {
                        job_supervisor.collect_loop_iteration(parent, scope)?;
                    }
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
                            let mut element_bindings = bindings.clone();
                            element_bindings
                                .insert(STDOUT_ELIGIBILITY.to_owned(), Value::Bool(program_stdout));
                            checkpoint::elements::for_each_element_commit(
                                journal,
                                &loop_frames,
                                header,
                                &cwd,
                                &bytes,
                                &element_bindings,
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
                            environment: (!workflow_env.set.is_empty()
                                || !workflow_env.unset.is_empty())
                            .then(|| workflow_env.clone()),
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
                            status: signals.drain_exit_status(),
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
                    workflow_env_scopes.push(workflow_env.clone());
                    workflow_environment_sensitive_scopes.push(workflow_environment_sensitive);
                    if *connection == Connection::Start {
                        pipeline_sensitive = false;
                    }
                    let sensitive_call = call.marks_sensitive_output()
                        || (*connection != Connection::Start && pipeline_sensitive)
                        || pending_environment_sensitive
                        || workflow_environment_sensitive
                        || call
                            .binding
                            .as_ref()
                            .is_some_and(|name| sensitive_bindings.contains(name));
                    let child = calls::bind(call, inputs, &bindings).with_context(|| {
                        format!("program {:?}, called at line {}", call.operation, step.line)
                    })?;
                    let parent = current_job_scope(&control_state, &call_frames, &loop_frames);
                    let job_scope = job_supervisor.enter_scope(parent)?;
                    control_state.enter_call(index + end);
                    call_frames.push(calls::Frame {
                        name: &call.operation,
                        line: step.line,
                        binding: call.binding.as_deref(),
                        sensitive_binding: sensitive_call,
                        status_binding: call.status_binding.as_deref(),
                        exit_status: None,
                        job_scope,
                        bindings: std::mem::replace(&mut bindings, child),
                        cwd: cwd.duplicate()?,
                    });
                    if call.binding.is_some() {
                        bound_call_depths.push(call_frames.len());
                    }
                    if *connection == Connection::Start {
                        data = Input::Empty;
                    }
                    pipeline_sensitive = sensitive_call;
                    pending_environment_sensitive = false;
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
                    if frame.binding.is_some() {
                        program_stdout = false;
                    }
                    bindings = frame.bindings;
                    cwd = frame.cwd;
                    workflow_env = workflow_env_scopes
                        .pop()
                        .context("program environment scope was not saved")?;
                    workflow_environment_sensitive = workflow_environment_sensitive_scopes
                        .pop()
                        .context("program environment sensitivity scope was not saved")?;
                    if let Some(name) = frame.status_binding {
                        bindings.insert(
                            name.to_owned(),
                            Value::Integer(frame.exit_status.unwrap_or(0)),
                        );
                    }
                    if let Some(name) = frame.binding {
                        let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                            options.max_output_bytes,
                            signals,
                            options.command_timeout,
                        )?;
                        if frame.sensitive_binding {
                            let redactor = crate::security::secret_redactor(registry);
                            redactor.register(&bytes).map_err(|_| {
                                anyhow!(
                                    "line {}: could not register sensitive program output",
                                    frame.line
                                )
                            })?;
                            if bytes.ends_with(b"\n") {
                                redactor.register(&bytes[..bytes.len() - 1]).map_err(|_| {
                                    anyhow!(
                                        "line {}: could not register sensitive program output",
                                        frame.line
                                    )
                                })?;
                            }
                        }
                        bindings.insert(
                            name.to_owned(),
                            Value::String(
                                String::from_utf8(bytes.clone())
                                    .context("named program results must be UTF-8 text")?,
                            ),
                        );
                        data = Input::Bytes(bytes);
                        pipeline_sensitive = frame.sensitive_binding;
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"program_return", "program":frame.name, "line":frame.line}),
                    )?;
                    if let Some((target_depth, _)) =
                        pending_status_unwind.filter(|(_, end_marker)| *end_marker == index)
                    {
                        if call_frames.len() >= target_depth {
                            // Return one enclosing call at a time so its EndCall restores
                            // cwd, workflow environment, sensitive state, and local bindings.
                            let target = control_state.current_call()?;
                            pending_status_unwind = Some((target_depth, target.end_marker));
                            let action = control_state.transfer(
                                control_flow::TransferKind::CallStatus,
                                target.end_marker,
                                control_flow::UnwindTo::new(target.call_depth, target.loop_depth),
                            )?;
                            match apply_control_action(action, control_context!())? {
                                AppliedControl::Continue => jump_to = Some(target.end_marker),
                                AppliedControl::Jump(target) => jump_to = Some(target),
                                AppliedControl::Rethrow(error) => return Err(error),
                            }
                        } else {
                            pending_status_unwind = None;
                        }
                    }
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
                    let scoped_call_exit =
                        matches!(step.kind, StepKind::Exit { .. }) && !call_frames.is_empty();
                    if scoped_call_exit {
                        let frame = call_frames.last_mut().expect("checked above");
                        // `exit with status` inside a reusable program ends that call.
                        // Its finally blocks and call-local jobs still unwind normally.
                        job_supervisor
                            .cancel_scope(frame.job_scope)
                            .with_context(|| {
                                format!(
                                    "line {}: cancelling jobs before reusable-program exit",
                                    step.line
                                )
                            })?;
                        frame.exit_status = Some(i64::from(exit.status));
                        let target = control_state.current_call()?;
                        let action = control_state.transfer(
                            control_flow::TransferKind::CallStatus,
                            target.end_marker,
                            control_flow::UnwindTo::new(target.call_depth, target.loop_depth),
                        )?;
                        match apply_control_action(action, control_context!())? {
                            AppliedControl::Continue => jump_to = Some(target.end_marker),
                            AppliedControl::Jump(target) => jump_to = Some(target),
                            AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                        }
                    } else {
                        lazy_event(options.trace, signals, || match &exit.message {
                            Some(message) => {
                                serde_json::json!({"event":"fail","line":step.line,"status":exit.status,"message_bytes":message.len()})
                            }
                            None => {
                                serde_json::json!({"event":"exit","line":step.line,"status":exit.status})
                            }
                        })?;
                        // `fail with message` and top-level exits remain run-scoped.
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
                            AppliedControl::Jump(target) => jump_to = Some(target),
                            AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                        }
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
                    let parent = current_job_scope(&control_state, &call_frames, &loop_frames);
                    let job_scope = job_supervisor.enter_scope(parent)?;
                    control_state.enter_try_with_job_scope(
                        index,
                        catch,
                        finally.map(|offset| index + offset),
                        index + end,
                        job_scope,
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
                    if let Some(scope) = control_state.active_job_scope(call_frames.len()) {
                        job_supervisor
                            .cancel_scope_preserving(scope)
                            .with_context(|| {
                                format!("line {}: cancelling jobs before finally", step.line)
                            })?;
                    }
                    let action = control_state.finally_marker(index)?;
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::EndTry => {
                    let scope = control_state.job_scope_at_end(index + 1)?;
                    job_supervisor.finish_scope(scope).with_context(|| {
                        format!("line {}: closing try-block job scope", step.line)
                    })?;
                    // `Try.end` is a relative offset to the instruction after
                    // EndTry, so the runtime marker uses the same half-open
                    // boundary convention.
                    let status_capture_depth = call_frames
                        .iter()
                        .rposition(|frame| frame.status_binding.is_some())
                        .map(|index| index + 1);
                    let action = if let Some(call_depth) = status_capture_depth {
                        control_state.end_try_for_call(index + 1, call_depth)?
                    } else {
                        control_state.end_try(index + 1)?
                    };
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => {}
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => control_rethrow = Some(error),
                    }
                }
                StepKind::UseFolder { path } => {
                    let folder = path_value(path, &bindings, &cwd)?;
                    cwd = (|| {
                        crate::capabilities::authorize_host_path(
                            registry,
                            &folder,
                            crate::capabilities::HostAccess::Read,
                            &cwd,
                        )?;
                        let cwd = enter_directory(registry, &folder, &cwd)?;
                        if !cwd.is_dir() {
                            bail!("use folder requires a directory");
                        }
                        Ok(cwd)
                    })()
                    .with_context(|| {
                        format!("line {}: use folder {:?}", step.line, folder.display())
                    })?;
                }
                StepKind::UseValue { source } => {
                    pipeline_sensitive = value_references_sensitive(source, &sensitive_bindings);
                    let bytes = text_value_bytes(source, &bindings, step.line)?;
                    if pipeline_sensitive {
                        crate::security::register_sensitive_bytes(registry, &bytes)?;
                    }
                    data = Input::Bytes(bytes);
                }
                StepKind::BindValue { source, binding } => {
                    let value = resolve(source, &bindings)?;
                    let sensitive_source = value_references_sensitive(source, &sensitive_bindings);
                    pipeline_sensitive = sensitive_bindings.contains(binding) || sensitive_source;
                    if sensitive_bindings.contains(binding) || sensitive_source {
                        crate::security::register_sensitive_value(registry, &value)?;
                    }
                    let bytes = bound_value_bytes(&value, options.max_output_bytes)?;
                    if sensitive_bindings.contains(binding) || sensitive_source {
                        crate::security::register_sensitive_bytes(registry, &bytes)?;
                    }
                    data = Input::Bytes(bytes);
                    bindings.insert(binding.clone(), value);
                }
                StepKind::CaptureBytes { binding } => {
                    let bytes = std::mem::replace(&mut data, Input::Empty).materialize(
                        options.max_output_bytes,
                        signals,
                        options.command_timeout,
                    )?;
                    if sensitive_bindings.contains(binding) {
                        crate::security::register_sensitive_bytes(registry, &bytes)?;
                    }
                    pipeline_sensitive |= sensitive_bindings.contains(binding);
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
                    pipeline_sensitive = sensitive_bindings.contains(binding);
                    data = Input::Bytes(bytes);
                }
                StepKind::Read { binding, .. }
                | StepKind::ReadStdin { binding }
                | StepKind::ReadCallInput { binding } => {
                    match &step.kind {
                        StepKind::ReadCallInput { .. } => {
                            if let Some(binding) = binding {
                                pipeline_sensitive |= sensitive_bindings.contains(binding);
                            }
                        }
                        _ => pipeline_sensitive = false,
                    }
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
                        if sensitive_bindings.contains(name) {
                            crate::security::register_sensitive_value(
                                registry,
                                bindings.get(name).expect("just inserted sensitive binding"),
                            )?;
                        }
                        pipeline_sensitive |= sensitive_bindings.contains(name);
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
                    if sensitive_bindings.contains(binding) {
                        crate::security::register_sensitive_bytes(registry, &bytes)?;
                        crate::security::register_sensitive_value(registry, &value)?;
                    }
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
                    if sensitive_bindings.contains(binding) {
                        crate::security::register_sensitive_value(registry, &value)?;
                    }
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
                    if sensitive_bindings.contains(binding) {
                        crate::security::register_sensitive_value(registry, &value)?;
                    }
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"structured", "operation":"take_json", "line":step.line, "type":kind.to_string()}),
                    )?;
                }
                StepKind::EncodeJson { source } => {
                    let sensitive_source = value_references_sensitive(source, &sensitive_bindings);
                    pipeline_sensitive = sensitive_source;
                    let source = structured_source(source, &bindings)?;
                    let bytes = crate::structured::encode(&source, options.max_output_bytes)
                        .with_context(|| format!("line {}", step.line))?;
                    if sensitive_source {
                        crate::security::register_sensitive_bytes(registry, &bytes)?;
                    }
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
                                        sensitive_items: value_references_sensitive(
                                            &each.values,
                                            &sensitive_bindings,
                                        ),
                                        sensitive_environment: workflow_environment_sensitive,
                                        options,
                                        cancelled: Arc::clone(&signals.cancelled),
                                        draining: Arc::clone(&signals.drain),
                                        context: AppletInvocationContext::from_signals(signals)?,
                                        environment: (!workflow_env.set.is_empty()
                                            || !workflow_env.unset.is_empty())
                                        .then(|| workflow_env.clone()),
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
                                        sensitive_items: value_references_sensitive(
                                            &each.values,
                                            &sensitive_bindings,
                                        ),
                                        sensitive_environment: workflow_environment_sensitive,
                                        options,
                                        cancelled: Arc::clone(&signals.cancelled),
                                        draining: Arc::clone(&signals.drain),
                                        context: AppletInvocationContext::from_signals(signals)?,
                                        environment: (!workflow_env.set.is_empty()
                                            || !workflow_env.unset.is_empty())
                                        .then(|| workflow_env.clone()),
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
                                sensitive_items: false,
                                sensitive_environment: workflow_environment_sensitive,
                                options,
                                cancelled: Arc::clone(&signals.cancelled),
                                draining: Arc::clone(&signals.drain),
                                context: AppletInvocationContext::from_signals(signals)?,
                                environment: (!workflow_env.set.is_empty()
                                    || !workflow_env.unset.is_empty())
                                .then(|| workflow_env.clone()),
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
                    let pipeline_is_replaced = each.is_some()
                        && !options.trace
                        && journal.is_none()
                        && program.steps.get(index + 1).is_some_and(|next| {
                            let replacement_fits = |value: &Value| match value {
                                Value::String(text) => text.len() <= options.max_output_bytes,
                                Value::Integer(number) => {
                                    number.to_string().len() <= options.max_output_bytes
                                }
                                Value::Bool(true) => options.max_output_bytes >= 4,
                                Value::Bool(false) => options.max_output_bytes >= 5,
                                Value::Null => options.max_output_bytes >= 4,
                                Value::Number(number) => {
                                    number.to_string().len() <= options.max_output_bytes
                                }
                                _ => false,
                            };
                            match &next.kind {
                                StepKind::UseValue { source }
                                | StepKind::BindValue { source, .. } => replacement_fits(source),
                                _ => false,
                            }
                        });
                    let output_limit = if each.is_some() {
                        let item_count = match &value {
                            crate::ast::Value::Record(fields) => match fields.get("items") {
                                Some(crate::ast::Value::List(items)) => items.len(),
                                _ => 0,
                            },
                            _ => 0,
                        };
                        tasks::each_result_encoded_limit(options.max_output_bytes, item_count)
                    } else {
                        options.max_output_bytes
                    };
                    let bytes = if pipeline_is_replaced {
                        Vec::new()
                    } else {
                        crate::structured::encode(&value, output_limit)
                            .with_context(|| format!("line {}: task group output", step.line))?
                    };
                    if sensitive_bindings.contains(binding) {
                        crate::security::register_sensitive_value(registry, &value)?;
                        crate::security::register_sensitive_bytes(registry, &bytes)?;
                    }
                    bindings.insert(binding.clone(), value);
                    pipeline_sensitive = sensitive_bindings.contains(binding);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"task_group","line":step.line,"mode":mode,"tasks":tasks.len(),"output_bytes":bytes.len()}),
                    )?;
                    data = if pipeline_is_replaced {
                        Input::Empty
                    } else {
                        Input::Bytes(bytes)
                    };
                }
                StepKind::SpawnJob { name, stages } => {
                    let scope = current_job_scope(&control_state, &call_frames, &loop_frames);
                    let _environment = (!workflow_env.set.is_empty()
                        || !workflow_env.unset.is_empty())
                    .then(|| workflow_env.clone())
                    .map(|overlay| crate::capabilities::CommandEnvScope::enter(Some(overlay)));
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
                            environment: (!workflow_env.set.is_empty()
                                || !workflow_env.unset.is_empty())
                            .then(|| workflow_env.clone()),
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
                    let scopes = visible_job_scopes(&control_state, &call_frames, &loop_frames);
                    let scope = job_supervisor.scope_for_job(name, &scopes);
                    let batch_scope = scopes
                        .iter()
                        .copied()
                        .find(|scope| job_supervisor.batch_scopes(*scope, name).is_some());
                    let (value, retire_scope) = if let Some(scope) = scope {
                        (
                            job_supervisor.inspect(scope, name).with_context(|| {
                                format!("line {}: inspecting job {name:?}", step.line)
                            })?,
                            Some(scope),
                        )
                    } else if let Some(parent) = batch_scope {
                        (
                            job_supervisor
                                .inspect_batch(parent, name)
                                .with_context(|| {
                                    format!("line {}: inspecting job batch {name:?}", step.line)
                                })?,
                            None,
                        )
                    } else {
                        (Value::List(Vec::new()), None)
                    };
                    let bytes = crate::structured::encode(&value, options.max_output_bytes)
                        .with_context(|| format!("line {}: job inspection output", step.line))?;
                    bindings.insert(binding.clone(), value);
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"job_inspect","line":step.line,"job":name,"output_bytes":bytes.len()}),
                    )?;
                    if last_job_uses.get(name.as_str()) == Some(&index) {
                        if let Some(scope) = retire_scope {
                            job_supervisor.retire_resolved(scope, name);
                        } else if let Some(parent) = batch_scope {
                            job_supervisor.retire_batch(parent, name);
                        }
                    }
                }
                StepKind::AwaitJob {
                    name,
                    status_binding,
                    result_binding,
                } => {
                    let scopes = visible_job_scopes(&control_state, &call_frames, &loop_frames);
                    let scope = job_supervisor.scope_for_job(name, &scopes);
                    let batch_scope = scopes
                        .iter()
                        .copied()
                        .find(|scope| job_supervisor.batch_scopes(*scope, name).is_some());
                    let mut batch_results = if let Some(scope) = batch_scope {
                        job_supervisor
                            .await_batch_until(scope, name, signals.deadline)
                            .with_context(|| {
                                format!("line {}: awaiting job batch {name:?}", step.line)
                            })?
                    } else if let Some(scope) = scope {
                        vec![(
                            scope,
                            job_supervisor
                                .await_job_until(scope, name, signals.deadline)
                                .with_context(|| {
                                    format!("line {}: awaiting job {name:?}", step.line)
                                })?,
                        )]
                    } else {
                        Vec::new()
                    };
                    let mut aggregate_stdout = Vec::new();
                    let mut aggregate_records = Vec::new();
                    let mut aggregate_status = 0i32;
                    let mut aggregate_failure = None;
                    for (scope, mut terminal) in batch_results.drain(..) {
                        let stage_failure = terminal
                            .stages
                            .iter()
                            .take(terminal.stages.len().saturating_sub(1))
                            .find(|stage| {
                                stage.kind.is_some_and(|kind| {
                                    kind != execution_result::ExecutionKind::Success
                                })
                            });
                        let stage_failed = stage_failure.is_some();
                        if let Some(stage) = stage_failure {
                            terminal.kind = execution_result::ExecutionKind::Failure;
                            terminal.message = Some(format!(
                                "background pipeline stage {:?} ended with {:?}",
                                stage.applet, stage.kind
                            ));
                        }
                        let kept = status_binding.is_some()
                            && terminal.status.is_some()
                            && matches!(
                                terminal.kind,
                                execution_result::ExecutionKind::Absence
                                    | execution_result::ExecutionKind::Failure
                                    | execution_result::ExecutionKind::Retryable
                            );
                        if (stage_failed
                            || (terminal.kind != execution_result::ExecutionKind::Success && !kept))
                            && aggregate_failure.is_none()
                        {
                            aggregate_failure = Some(terminal.clone());
                        }
                        let status = terminal.status.unwrap_or(1);
                        if aggregate_status == 0 && status != 0 {
                            aggregate_status = status;
                        }
                        if result_binding.is_some() {
                            aggregate_records.push(
                                job_supervisor.result_value(scope, name, step.line, &terminal)?,
                            );
                        }
                        aggregate_stdout.extend(terminal.stdout.take().unwrap_or_default());
                    }
                    if let Some(mut terminal) = aggregate_failure {
                        if terminal.message.is_none() && terminal.status.is_some() {
                            terminal.message = Some(format!(
                                "background job {name:?} completed with {:?} outcome ({})",
                                terminal.kind,
                                terminal.status.unwrap_or_default()
                            ));
                        }
                        return Err(execution_result::ExecutionFailure::new(terminal).into());
                    }
                    if let Some(binding) = status_binding {
                        bindings
                            .insert(binding.clone(), Value::Integer(i64::from(aggregate_status)));
                    }
                    if let Some(binding) = result_binding {
                        let value = if scope.is_some()
                            && batch_scope.is_none()
                            && aggregate_records.len() == 1
                        {
                            aggregate_records.remove(0)
                        } else {
                            Value::List(aggregate_records)
                        };
                        crate::structured::encode(&value, options.max_output_bytes)
                            .with_context(|| format!("line {}: job result record", step.line))?;
                        bindings.insert(binding.clone(), value);
                    }
                    data = Input::Bytes(aggregate_stdout);
                    if stream_top
                        && status_binding.is_none()
                        && result_binding.is_none()
                        && !crate::ast::has_stdout_consumer(&program.steps, index + 1)
                    {
                        if let Input::Bytes(bytes) = std::mem::replace(&mut data, Input::Empty) {
                            if !observe::suppresses_terminal_output(step.line) {
                                write_streamed_stdout_owned(bytes, signals)?;
                            }
                        }
                        data = Input::Empty;
                    }
                    let status = aggregate_status;
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"job_await","line":step.line,"job":name,"status":status}),
                    )?;
                    if last_job_uses.get(name.as_str()) == Some(&index) {
                        if let Some(scope) = scope {
                            job_supervisor.retire_resolved(scope, name);
                        }
                        if let Some(parent) = batch_scope {
                            job_supervisor.retire_batch(parent, name);
                        }
                    }
                }
                StepKind::CancelJob { name } => {
                    let scopes = visible_job_scopes(&control_state, &call_frames, &loop_frames);
                    let scope = job_supervisor.scope_for_job(name, &scopes);
                    let batch_scope = scopes
                        .iter()
                        .copied()
                        .find(|scope| job_supervisor.batch_scopes(*scope, name).is_some());
                    if let Some(scope) = scope {
                        job_supervisor.cancel(scope, name).with_context(|| {
                            format!("line {}: cancelling job {name:?}", step.line)
                        })?;
                    } else if let Some(parent) = batch_scope {
                        job_supervisor.cancel_batch(parent, name).with_context(|| {
                            format!("line {}: cancelling job batch {name:?}", step.line)
                        })?;
                    }
                    lazy_event(
                        options.trace,
                        signals,
                        || serde_json::json!({"event":"job_cancel","line":step.line,"job":name}),
                    )?;
                    if last_job_uses.get(name.as_str()) == Some(&index) {
                        if let Some(scope) = scope {
                            job_supervisor.retire_resolved(scope, name);
                        }
                        if let Some(parent) = batch_scope {
                            job_supervisor.retire_batch(parent, name);
                        }
                    }
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
                        sync_destination_parent(&destination, &cwd)?;
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
                StepKind::Environment {
                    set,
                    unset,
                    workflow_scope,
                } => {
                    let environment_sensitive = set
                        .values()
                        .any(|value| value_references_sensitive(value, &sensitive_bindings));
                    let mut overlay = crate::capabilities::CommandEnvOverlay {
                        set: BTreeMap::new(),
                        unset: unset
                            .iter()
                            .map(|name| normalize_environment_variable(name))
                            .collect(),
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
                        if text.contains('\0') {
                            bail!(
                                "line {}: environment value for {name:?} cannot contain a NUL byte",
                                step.line
                            );
                        }
                        overlay
                            .set
                            .insert(normalize_environment_variable(name), text);
                    }
                    if *workflow_scope {
                        merge_environment_overlay(&mut workflow_env, overlay);
                        workflow_environment_sensitive |= environment_sensitive;
                    } else {
                        pending_env = Some(overlay);
                        pending_environment_sensitive = environment_sensitive;
                    }
                }
                StepKind::Redirect { target, path } => {
                    // The file is opened now, before the command can start, and the
                    // command's stderr is written through this same handle later.
                    let (file, destination) = match path {
                        Some(path) => {
                            let destination = path_value(path, &bindings, &cwd)?;
                            let sync_destination = journal.is_some().then(|| destination.clone());
                            let append = *target == crate::ast::RedirectTarget::StderrAppend;
                            (
                                Some(open_redirect_target(
                                    &destination,
                                    append,
                                    registry,
                                    signals,
                                    &cwd,
                                )?),
                                sync_destination,
                            )
                        }
                        None => (None, None),
                    };
                    pending_redirect = Some(PendingRedirect::new(*target, destination, file));
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
                        sync_destination_parent(&destination, &cwd)?;
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
                    let env = {
                        let mut merged = workflow_env.clone();
                        if let Some(pending) = pending_env.take() {
                            merge_environment_overlay(&mut merged, pending);
                        }
                        (!merged.set.is_empty() || !merged.unset.is_empty()).then_some(merged)
                    };
                    let _environment_scope = env
                        .map(|overlay| crate::capabilities::CommandEnvScope::enter(Some(overlay)));
                    let handoff = std::mem::take(&mut pending_handoff);
                    let process = pending_process.take();
                    let manifest = registry.resolve_call(call)?;
                    if *connection == Connection::Start {
                        pipeline_sensitive = false;
                    }
                    let consumes_sensitive_pipeline = *connection != Connection::Start
                        && manifest.accepts_stdin
                        && pipeline_sensitive;
                    let declared_secret_environment =
                        crate::security::applet_has_declared_secret_environment(
                            registry,
                            manifest.name_for(call),
                        );
                    let mut group_sensitive = call.marks_sensitive_output()
                        || consumes_sensitive_pipeline
                        || pending_environment_sensitive
                        || workflow_environment_sensitive
                        || declared_secret_environment;
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
                        let mut command = crate::capabilities::command_for(
                            &binary,
                            argv.iter().map(|argument| OsStr::new(argument.as_str())),
                            registry,
                            &manifest.name,
                            &cwd,
                        )?;
                        // A custom model endpoint is authorized like a remote
                        // origin before the handover/detach. The parameters
                        // are already resolved, so no bindings remain.
                        if manifest.binary == "@execute-model" {
                            if let Some(granted) =
                                crate::capabilities::authorize_model_endpoint_params(
                                    registry,
                                    manifest,
                                    &parameters,
                                    &BTreeMap::new(),
                                    &signals.sensitivity_floor(),
                                )?
                            {
                                command.env(crate::capabilities::MODEL_GRANTED_ORIGIN_ENV, granted);
                            }
                        }
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
                                #[cfg(unix)]
                                let code = handoff::hand_over(command)
                                    .with_context(|| format!("line {}: hand over", step.line))?;
                                #[cfg(windows)]
                                let code = handoff::hand_over(command, signals)
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
                        pipeline_sensitive = false;
                        pending_environment_sensitive = false;
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
                        group_sensitive |= calls.iter().any(|(stage, stage_manifest, _)| {
                            stage.marks_sensitive_output()
                                || crate::security::applet_has_declared_secret_environment(
                                    registry,
                                    stage_manifest.name_for(stage),
                                )
                        });
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
                        let sensitive_terminal_output = group_sensitive
                            || output_call
                                .binding
                                .as_ref()
                                .is_some_and(|name| sensitive_bindings.contains(name))
                            || output_call
                                .result_binding
                                .as_ref()
                                .is_some_and(|name| sensitive_bindings.contains(name));
                        let mut sensitive_output_call = output_call.clone();
                        if sensitive_terminal_output {
                            sensitive_output_call.sensitive_binding = true;
                        }
                        let mut calls_for_execution = calls
                            .iter()
                            .map(|(call, manifest, line)| (*call, *manifest, *line))
                            .collect::<Vec<_>>();
                        if sensitive_terminal_output {
                            let terminal = calls_for_execution
                                .last_mut()
                                .expect("invocation group is nonempty");
                            terminal.0 = &sensitive_output_call;
                        }
                        let (mut bytes, status, stdout_captured, stages, typed_json) = if call.cache
                        {
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
                                    calls: &calls_for_execution,
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
                                    Some(file) => Some(read_cached_output(
                                        manifest,
                                        file,
                                        options.max_output_bytes,
                                    )?),
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
                            let _handoff = terminal::Scope::enter(handoff);
                            // One native command receives a file or stdout redirect as
                            // its own stream, so the bytes land as they are written.
                            let stream = match redirect.as_ref() {
                                Some(pending)
                                    if calls.len() == 1
                                        && manifest.execution_backend()
                                            == AppletBackend::Command =>
                                {
                                    pending.stream()?
                                }
                                _ => None,
                            };
                            let _stderr = ForceStderrCapture::enter(
                                redirect.as_ref().is_some_and(|pending| {
                                    pending.captures_stderr()
                                        || (stream.is_none()
                                            && pending.target
                                                != crate::ast::RedirectTarget::StdoutToStderr)
                                }) || sensitive_terminal_output,
                            );
                            let stream_scope = StreamScope::enter(stream);
                            let completed = execute_call_group(GroupRequest {
                                calls: &calls_for_execution,
                                input,
                                prepared_first_call: None,
                                bindings: &bindings,
                                cwd: &cwd,
                                options,
                                launched: &mut result.launched_processes,
                                signals,
                                registry,
                            });
                            if let Some(redirect) = redirect.as_mut() {
                                redirect.streamed = stream_scope.used();
                            }
                            drop(stream_scope);
                            let completed = match completed {
                                Ok(completed) => completed,
                                Err(error) => {
                                    // A failed command's stderr still goes to its
                                    // file target before the failure propagates; a
                                    // streamed file already holds it and is synced.
                                    if let Some(redirect) = redirect.as_mut() {
                                        if redirect.streamed {
                                            redirect.finish_stream()?;
                                        } else if let Some(failure) =
                                            error.downcast_ref::<CommandFailed>()
                                        {
                                            let stderr =
                                                failure.stderr.clone().unwrap_or_else(|| {
                                                    failure.diagnostic.clone().into_bytes()
                                                });
                                            redirect.write_stderr(&stderr)?;
                                        }
                                        if journal.is_some() {
                                            if let Some(destination) = redirect.destination.as_ref()
                                            {
                                                sync_destination_parent(destination, &cwd)?;
                                            }
                                        }
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
                        let merged_redirect = redirect.as_mut().filter(|pending| {
                            matches!(pending.target, crate::ast::RedirectTarget::StderrMerged)
                        });
                        if let Some(pending) = merged_redirect {
                            bytes = pending.route(bytes, &redirected_stderr, signals)?;
                        }
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
                        let sensitive_output = output_call
                            .binding
                            .as_ref()
                            .is_some_and(|name| sensitive_bindings.contains(name))
                            || output_call
                                .result_binding
                                .as_ref()
                                .is_some_and(|name| sensitive_bindings.contains(name));
                        if sensitive_output {
                            crate::security::register_sensitive_bytes(registry, &bytes)?;
                            if let Some(stderr) = stderr {
                                crate::security::register_sensitive_bytes(registry, stderr)?;
                            }
                            for stage_stderr in
                                stages.iter().filter_map(|stage| stage.stderr.as_deref())
                            {
                                crate::security::register_sensitive_bytes(registry, stage_stderr)?;
                            }
                            if let Some(value) = typed_json.as_ref() {
                                crate::security::register_sensitive_value(
                                    registry,
                                    &crate::structured::from_json(value.clone()),
                                )?;
                            }
                        }
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
                            Some(redirect)
                                if matches!(
                                    redirect.target,
                                    crate::ast::RedirectTarget::StderrMerged
                                ) =>
                            {
                                bytes
                            }
                            Some(redirect) => {
                                let bytes = redirect.route(bytes, &redirected_stderr, signals)?;
                                if journal.is_some() {
                                    if let Some(destination) = redirect.destination.as_ref() {
                                        sync_destination_parent(destination, &cwd)?;
                                    }
                                }
                                bytes
                            }
                            None => bytes,
                        };
                        if streamed {
                            // Nothing consumes this value, so it is written now
                            // and the pipeline value becomes empty.
                            if !observe::suppresses_terminal_output(step.line) {
                                write_streamed_stdout_owned(bytes, signals)?;
                            }
                            data = Input::Empty;
                        } else {
                            data = Input::Bytes(bytes);
                        }
                        pipeline_sensitive = sensitive_terminal_output;
                        pending_environment_sensitive = false;
                    }
                }
            }
            Ok(())
        })();
        if execution.is_ok() {
            if let Some(eligible) = program.steps[index].kind.program_stdout() {
                program_stdout = eligible;
                last_program_stdout_line = Some(program.steps[index].line);
            }
        }
        // A declared retry runs the command again from its first marker. The journal
        // node stays open, because the step has not completed.
        let mut execution = match execution {
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
        if let Some(error) = control_rethrow.take() {
            if error
                .downcast_ref::<CommandFailed>()
                .and_then(|failure| failure.status.code())
                .is_some()
                && call_frames
                    .iter()
                    .any(|frame| frame.status_binding.is_some())
            {
                // A failure returned from finally/cleanup may reach this path
                // without an in-flight statement error; feed it through the
                // same status boundary as a direct native failure.
                execution = Err(error);
            } else {
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
        }
        if let Err(error) = execution {
            let status_capture_depth = error
                .downcast_ref::<CommandFailed>()
                .and_then(|failure| failure.status.code())
                .and_then(|_| {
                    call_frames
                        .iter()
                        .rposition(|frame| frame.status_binding.is_some())
                })
                .map(|index| index + 1);
            let error = call_frames.iter().rev().fold(error, |error, frame| {
                error.context(format!(
                    "program {:?}, called at line {}",
                    frame.name, frame.line
                ))
            });
            // A repeat interrupt that landed mid-statement is reported as a
            // cleanup interruption, with the pending failure kept as context
            // below. Other cleanup errors (budgets, timeouts, step failures)
            // keep their own errors so they still unwind through the frames.
            let cleanup_cancelled = was_cleanup
                && cleanup_signals
                    .as_ref()
                    .is_some_and(|cleanup| cleanup.is_cancelled())
                && is_cancellation_error(&error);
            let (error, cancellation) = if cleanup_cancelled && !cleanup_interrupted_reported {
                cleanup_interrupted_reported = true;
                (cleanup_interrupted(), true)
            } else if cleanup_cancelled {
                (error, true)
            } else {
                let cancellation = !was_cleanup && parent_signals.cancelled.load(Ordering::Relaxed);
                (error, cancellation)
            };
            let action = if let Some(call_depth) = status_capture_depth {
                control_state.fail_for_call(error, cancellation, call_depth)?
            } else {
                control_state.fail(error, cancellation)?
            };
            match apply_control_action(action, control_context!())? {
                AppliedControl::Continue => {}
                AppliedControl::Jump(target) => {
                    // A handler outside the in-flight node (a finally or `on exit`
                    // body) runs with the failure pending: that node did not complete.
                    if journal_end.is_some_and(|end| target < journal_start || target >= end) {
                        journal_suspended = true;
                    }
                    jump_to = Some(target);
                }
                AppliedControl::Rethrow(error)
                    if error
                        .downcast_ref::<CommandFailed>()
                        .and_then(|failure| failure.status.code())
                        .is_some()
                        && status_capture_depth.is_some() =>
                {
                    let status = i64::from(
                        error
                            .downcast_ref::<CommandFailed>()
                            .and_then(|failure| failure.status.code())
                            .expect("guard checked status"),
                    );
                    let capture_index = call_frames
                        .iter()
                        .rposition(|frame| frame.status_binding.is_some())
                        .expect("guard checked a status-bound frame");
                    call_frames[capture_index].exit_status = Some(status);
                    let target_depth = capture_index + 1;
                    let target = control_state.current_call()?;
                    pending_status_unwind = Some((target_depth, target.end_marker));
                    let action = control_state.transfer(
                        control_flow::TransferKind::CallStatus,
                        target.end_marker,
                        control_flow::UnwindTo::new(target.call_depth, target.loop_depth),
                    )?;
                    match apply_control_action(action, control_context!())? {
                        AppliedControl::Continue => jump_to = Some(target.end_marker),
                        AppliedControl::Jump(target) => jump_to = Some(target),
                        AppliedControl::Rethrow(error) => return Err(error),
                    }
                }
                AppliedControl::Rethrow(error) if depth == 0 && journal.is_none() => {
                    // Cleanup that overran its budget after every statement completed
                    // fails the run but keeps its output: it ends like `fail`, with the
                    // budget error as the message. A checkpointed run keeps the error.
                    match cleanup_overrun_exit(&error) {
                        Some(exit) => {
                            control_state.set_exit(exit);
                            index = program.steps.len();
                            continue;
                        }
                        None => return Err(error),
                    }
                }
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
        while bound_call_depths
            .last()
            .is_some_and(|depth| *depth > call_frames.len())
        {
            bound_call_depths.pop();
            program_stdout = false;
        }
        let exited = control_state.exit_pending();
        index = jump_to.unwrap_or(index + 1);
        if let Some(journal) = journal.as_deref_mut().filter(|_| {
            !journal_suspended && (journal_end == Some(index) || (exited && journal_end.is_some()))
        }) {
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
                bindings
                    .iter()
                    .map(|(name, value)| (name.as_str(), value))
                    .chain(std::iter::once((
                        STDOUT_ELIGIBILITY,
                        &Value::Bool(program_stdout),
                    ))),
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
    } else if state_out.is_none() {
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
    result.binding_types = bindings
        .iter()
        .map(|(name, value)| {
            let kind = match value {
                Value::String(_) => "string",
                Value::Integer(_) => "integer",
                Value::Bool(_) => "boolean",
                Value::List(_) => "list",
                Value::Record(_) => "record",
                Value::Null => "null",
                Value::Number(_) => "number",
                Value::Reference(_) => "reference",
            };
            (name.clone(), kind)
        })
        .collect();
    #[cfg(feature = "live")]
    if let Some(state) = state_out {
        *state = live::NodeState {
            cwd: cwd.to_path_buf(),
            bindings,
            data: result.output.clone(),
        };
    }
    if !program_stdout {
        result.output.clear();
    } else {
        observe::note_program_output(&result.output, last_program_stdout_line);
    }
    result.executions = execution_history.executions;
    result.dropped_executions = execution_history.dropped_executions;
    result.dropped_stages = execution_history.dropped_stages;
    Ok(result)
}
