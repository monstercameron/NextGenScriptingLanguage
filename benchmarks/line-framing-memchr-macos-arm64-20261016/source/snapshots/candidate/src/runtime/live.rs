//! Opt-in execution adapter for live control. Ordinary runs retain their fast path.
use super::*;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub(crate) struct NodeState {
    pub cwd: PathBuf,
    pub bindings: BTreeMap<String, Value>,
    pub data: Vec<u8>,
}

pub(crate) fn node_end(program: &Program, index: usize) -> usize {
    resume::node_end(program, index)
}

pub(crate) fn execute_node(
    node: &Program,
    registry: &Registry,
    options: &RunOptions,
    state: NodeState,
    cancelled: Arc<AtomicBool>,
) -> Result<(RunResult, NodeState)> {
    let signals = Signals::with_cancellation_and_deadline(
        cancelled,
        true,
        deadline_after(options.total_timeout, "total timeout")?,
    )?;
    let options = RunOptions {
        inputs: Default::default(),
        working_directory: state.cwd,
        cache_directory: options.cache_directory.clone(),
        trace: options.trace,
        max_output_bytes: options.max_output_bytes,
        command_timeout: options.command_timeout,
        total_timeout: options.total_timeout,
    };
    crate::capabilities::preflight(node, registry, &options.working_directory)?;
    // run_finite_state replaces this slot on success; errors publish no state.
    // Copying the incoming bindings and bytes here only to discard them can
    // duplicate megabytes for every managed node.
    let mut final_state = NodeState {
        cwd: PathBuf::new(),
        bindings: BTreeMap::new(),
        data: Vec::new(),
    };
    let result = run_finite_state(
        node,
        registry,
        &options,
        &signals,
        Input::Bytes(state.data),
        state.bindings,
        None,
        FiniteStateSource::Fresh,
        Some(&mut final_state),
    )?;
    Ok((result, final_state))
}

pub(crate) fn producer(
    invocation: &Invocation,
    registry: &Registry,
    options: &RunOptions,
    bindings: &BTreeMap<String, Value>,
    cancelled: Arc<AtomicBool>,
    max_record: usize,
    mut emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<usize> {
    let signals = Signals::with_cancellation_and_deadline(
        cancelled,
        true,
        deadline_after(options.total_timeout, "total timeout")?,
    )?;
    let manifest = registry.resolve_call(invocation)?;
    let calls = [(invocation, manifest, 0)];
    let mut sink = streaming::LineSink::new(&mut emit, None, max_record);
    let mut launched = 0;
    execute_group_with_sink(
        registry,
        &calls,
        Input::Empty,
        None,
        bindings,
        &options.working_directory,
        options,
        &mut launched,
        &signals,
        Some(&mut sink),
    )?;
    Ok(launched)
}

pub(crate) fn stream(
    stream: &crate::stream::StreamProgram,
    registry: &Registry,
    options: &RunOptions,
    bindings: &BTreeMap<String, Value>,
    cancelled: Arc<AtomicBool>,
    max_record: usize,
    mut emit: impl FnMut(&[u8], &AtomicBool, Option<Instant>) -> Result<()>,
) -> Result<usize> {
    let signals = Signals::with_cancellation_and_deadline(
        cancelled,
        true,
        deadline_after(options.total_timeout, "total timeout")?,
    )?;
    let calls = stream
        .pipeline
        .steps
        .iter()
        .map(|step| {
            let StepKind::Invoke { call, .. } = &step.kind else {
                bail!("stream requires native invocations");
            };
            Ok((call, registry.resolve_call(call)?, step.line))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut sink = streaming::LineSink::new(&mut emit, stream.line_limit, max_record);
    let mut launched = 0;
    execute_group_with_sink(
        registry,
        &calls,
        Input::Empty,
        None,
        bindings,
        &options.working_directory,
        options,
        &mut launched,
        &signals,
        Some(&mut sink),
    )?;
    Ok(launched)
}
