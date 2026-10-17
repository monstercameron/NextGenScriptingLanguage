//! Resume compatibility is based on completed node semantics, not source lines.
use super::*;
use checkpoint::Journal;
use std::collections::HashMap;

#[cfg(test)]
mod bench;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[derive(PartialEq, Eq)]
struct BinaryIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl From<std::fs::Metadata> for BinaryIdentity {
    fn from(value: std::fs::Metadata) -> Self {
        #[cfg(unix)]
        return Self {
            device: value.dev(),
            inode: value.ino(),
            length: value.size(),
            modified: (value.mtime(), value.mtime_nsec()),
            changed: (value.ctime(), value.ctime_nsec()),
        };
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            Self {
                device: 0,
                inode: 0,
                length: value.file_size(),
                modified: (value.last_write_time() as i64, 0),
                changed: (
                    value.creation_time() as i64,
                    i64::from(value.file_attributes()),
                ),
            }
        }
    }
}
type BinaryDigests = HashMap<PathBuf, (BinaryIdentity, [u8; 32])>;

pub(super) fn node_end(program: &Program, start: usize) -> usize {
    // A redirect is one node with the command it routes, which validation keeps
    // as the single following invocation. Splitting them would let resume run the
    // command without its routing state.
    if matches!(
        program.steps[start].kind,
        StepKind::Redirect { .. } | StepKind::Interactive
    ) {
        return start + 2;
    }
    // An environment step is one node with the redirect and command it applies to.
    if matches!(program.steps[start].kind, StepKind::Environment { .. }) {
        let redirect = matches!(
            program.steps.get(start + 1).map(|step| &step.kind),
            Some(StepKind::Redirect { .. })
        );
        return start + if redirect { 3 } else { 2 };
    }
    if let StepKind::If { end, .. }
    | StepKind::IfCommand { end, .. }
    | StepKind::CallProgram { end, .. }
    | StepKind::Loop { end, .. } = program.steps[start].kind
    {
        return start + end;
    }
    // A job handle is never serialized, so a spawn's node runs through the statement
    // that awaits or cancels the job. Validation refuses a spawn with no such statement.
    if let StepKind::SpawnJob { name, .. } = &program.steps[start].kind {
        if let Some(end) = job_end(program, start, name) {
            return end;
        }
    }
    let mut end = start + 1;
    if matches!(program.steps[start].kind, StepKind::Invoke { .. }) {
        while pipe_at(program, end) {
            end += 1;
        }
    }
    end
}

/// The index just past the top-level statement that awaits or cancels the job
/// spawned at `start`, if one exists.
fn job_end(program: &Program, start: usize, name: &str) -> Option<usize> {
    let mut index = start + 1;
    while index < program.steps.len() {
        let end = node_end(program, index).min(program.steps.len());
        if program.steps[index..end]
            .iter()
            .any(|step| ends_job(&step.kind, name))
        {
            return Some(end);
        }
        index = end;
    }
    None
}

fn ends_job(kind: &StepKind, name: &str) -> bool {
    matches!(
        kind,
        StepKind::AwaitJob { name: job, .. } | StepKind::CancelJob { name: job } if job == name
    )
}

fn is_job_step(kind: &StepKind) -> bool {
    matches!(
        kind,
        StepKind::SpawnJob { .. }
            | StepKind::InspectJob { .. }
            | StepKind::AwaitJob { .. }
            | StepKind::CancelJob { .. }
    )
}

/// Whether a checkpointed program's job operations can be recorded. Each spawn must
/// be awaited or cancelled in the same checkpoint node, so its handle never crosses a
/// checkpoint. Jobs inside reusable program definitions are not supported yet.
pub(super) fn validate_checkpoint_jobs(program: &Program) -> Result<()> {
    if program
        .definitions
        .values()
        .flat_map(|definition| &definition.steps)
        .any(|step| is_job_step(&step.kind))
    {
        bail!("checkpointed and resumed programs do not support background jobs inside reusable program definitions");
    }
    let mut index = 0;
    while index < program.steps.len() {
        let end = node_end(program, index).min(program.steps.len());
        let node = &program.steps[index..end];
        for step in node {
            if let StepKind::SpawnJob { name, .. } = &step.kind {
                if !node.iter().any(|other| ends_job(&other.kind, name)) {
                    bail!(
                        "line {}: checkpointed job {name:?} must be awaited or cancelled in the same statement",
                        step.line
                    );
                }
            }
        }
        index = end;
    }
    Ok(())
}

/// Whether a checkpointed run can record its job operations.
pub(super) fn checkpoint_jobs_supported(program: &Program) -> bool {
    validate_checkpoint_jobs(program).is_ok()
}
fn manifest_identity(
    hash: &mut Sha256,
    call: &Invocation,
    registry: &Registry,
    cwd: &Path,
    binary_digests: &mut BinaryDigests,
    allow_absent: bool,
) -> Result<()> {
    let manifest = registry.resolve_call(call)?;
    hash_field(hash, &manifest.checkpoint_identity(call)?);
    if manifest.execution_backend() == AppletBackend::ProgramModule {
        // Module source and transitive package identities are preloaded as an
        // immutable session and appended by node_fingerprint_with_modules.
        // The manifest's @program-module sentinel is metadata, not an
        // executable that should be resolved from PATH.
        hash_field(hash, b"execute-program-module-preloaded-v1");
        hash_field(hash, manifest.name.as_bytes());
        return Ok(());
    }
    if manifest.builtin.is_none() {
        let binary = match binary_path(manifest.binary_for(call), cwd) {
            Ok(binary) => binary,
            Err(_) if allow_absent => {
                // A different branch may legitimately use a platform-specific
                // tool that is absent here. Pin that absence; installing it later
                // changes the completed conditional's identity on resume.
                hash_field(hash, b"\0execute-unresolved-conditional-binary-v1");
                hash_field(hash, manifest.binary_for(call).as_bytes());
                hash_field(hash, cwd.as_os_str().as_encoded_bytes());
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        hash_field(hash, binary.as_os_str().as_encoded_bytes());
        let identity = BinaryIdentity::from(fs::metadata(&binary)?);
        #[cfg(unix)]
        let cached = binary_digests
            .get(&binary)
            .filter(|(observed, _)| observed == &identity);
        #[cfg(windows)]
        let cached: Option<&(BinaryIdentity, [u8; 32])> = None;
        let contents = if let Some((_, contents)) = cached {
            *contents
        } else {
            let mut file = File::open(&binary)?;
            if BinaryIdentity::from(file.metadata()?) != identity {
                bail!(
                    "executable changed while fingerprinting {}",
                    binary.display()
                );
            }
            let mut chunk = [0u8; 32768];
            let mut contents = Sha256::new();
            loop {
                let count = file.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                contents.update(&chunk[..count]);
            }
            let contents: [u8; 32] = contents.finalize().into();
            if BinaryIdentity::from(file.metadata()?) != identity {
                bail!(
                    "executable changed while fingerprinting {}",
                    binary.display()
                );
            }
            binary_digests.insert(binary, (identity, contents));
            contents
        };
        hash_field(hash, &contents);
    }
    Ok(())
}

/// Identity for an ordinary call made from a preloaded program-module body.
/// Program-module imports are represented by the package graph itself. A
/// remote call without a pinned descriptor cannot provide a stable checkpoint
/// identity and therefore makes the containing module identity incomplete.
pub(super) fn module_body_call_identity(
    call: &Invocation,
    registry: &Registry,
    cwd: &Path,
) -> Result<Option<String>> {
    let manifest = registry.resolve_call(call)?;
    if manifest.execution_backend() == AppletBackend::ProgramModule {
        return Ok(None);
    }

    let mut hash = Sha256::new();
    hash_field(&mut hash, b"execute-module-body-call-v1");
    if manifest.execution_backend() == AppletBackend::Remote {
        let Some(remote) = manifest
            .execution_contract()
            .and_then(|contract| contract.remote.as_ref())
        else {
            bail!(
                "remote applet {:?} has no endpoint configuration",
                manifest.name
            );
        };
        if remote.descriptor_sha256.is_none() {
            return Ok(None);
        }
        hash_field(&mut hash, &manifest.checkpoint_identity(call)?);
    } else {
        manifest_identity(&mut hash, call, registry, cwd, &mut HashMap::new(), false)?;
    }
    Ok(Some(format!("{:x}", hash.finalize())))
}
pub(super) fn node_fingerprint(
    program: &Program,
    start: usize,
    end: usize,
    registry: &Registry,
    cwd: &Path,
) -> Result<String> {
    node_fingerprint_with_digests(program, start, end, registry, cwd, &mut HashMap::new())
}

pub(super) fn node_fingerprint_with_modules(
    program: &Program,
    start: usize,
    end: usize,
    registry: &Registry,
    cwd: &Path,
    module_fingerprints: &BTreeMap<String, String>,
) -> Result<String> {
    let base = node_fingerprint(program, start, end, registry, cwd)?;
    Ok(with_module_fingerprints(base, module_fingerprints))
}

fn with_module_fingerprints(
    base: String,
    module_fingerprints: &BTreeMap<String, String>,
) -> String {
    if module_fingerprints.is_empty() {
        return base;
    }
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"execute-node-module-snapshot-v1");
    hash_field(&mut hash, base.as_bytes());
    // BTreeMap iteration is canonical and the module session was completely
    // loaded before any checkpoint or workflow effect.
    if let Ok(bytes) = serde_json::to_vec(module_fingerprints) {
        hash_field(&mut hash, &bytes);
    }
    format!("{:x}", hash.finalize())
}
fn node_fingerprint_with_digests(
    program: &Program,
    start: usize,
    end: usize,
    registry: &Registry,
    cwd: &Path,
    binary_digests: &mut BinaryDigests,
) -> Result<String> {
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"execute-node-semantics-v1");
    let mut serialized = Vec::with_capacity(128);
    for step in &program.steps[start..end] {
        serialized.clear();
        if let StepKind::CallProgram {
            call,
            inputs,
            end,
            connection,
        } = &step.kind
        {
            let inputs = inputs
                .iter()
                .cloned()
                .map(|mut input| {
                    input.line = 0;
                    input
                })
                .collect();
            serde_json::to_writer(
                &mut serialized,
                &StepKind::CallProgram {
                    call: call.clone(),
                    inputs,
                    end: *end,
                    connection: *connection,
                },
            )?;
        } else {
            serde_json::to_writer(&mut serialized, &step.kind)?;
        }
        hash_field(&mut hash, &serialized);
        // A command condition's binary identity is part of its node, like an
        // invocation's. Its node is always compound, so an absent binary is
        // allowed here exactly as for other conditional bodies.
        if let StepKind::IfCommand { call, .. }
        | StepKind::Loop {
            kind: crate::ast::LoopKind::WhileCommand { call, .. },
            ..
        } = &step.kind
        {
            manifest_identity(&mut hash, call, registry, cwd, binary_digests, true)?;
        }
        if let StepKind::Invoke { call, .. } = &step.kind {
            manifest_identity(
                &mut hash,
                call,
                registry,
                cwd,
                binary_digests,
                matches!(
                    program.steps[start].kind,
                    StepKind::If { .. }
                        | StepKind::IfCommand { .. }
                        | StepKind::CallProgram { .. }
                        | StepKind::Loop { .. }
                ),
            )?;
        }
        if let StepKind::TaskGroup { tasks, .. } = &step.kind {
            for call in tasks {
                manifest_identity(&mut hash, call, registry, cwd, binary_digests, false)?;
            }
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
/// A graph node's checkpoint fingerprint, built as a checkpointed program node's is:
/// the node's own definition (`base`), its invocation's manifest, executable path and
/// content digest, and every preloaded program module's fingerprint.
pub(crate) fn graph_node_fingerprint(
    base: &[u8],
    call: Option<&Invocation>,
    registry: &Registry,
    cwd: &Path,
    module_fingerprints: &BTreeMap<String, String>,
) -> Result<String> {
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"execute-graph-node-v1");
    hash_field(&mut hash, base);
    if let Some(call) = call {
        manifest_identity(&mut hash, call, registry, cwd, &mut HashMap::new(), false)?;
    }
    Ok(with_module_fingerprints(
        format!("{:x}", hash.finalize()),
        module_fingerprints,
    ))
}

pub(super) fn retry_safe(
    program: &Program,
    start: usize,
    end: usize,
    registry: &Registry,
) -> Result<bool> {
    for step in &program.steps[start..end] {
        if matches!(
            step.kind,
            StepKind::Save { .. }
                | StepKind::Append { .. }
                | StepKind::Redirect { .. }
                | StepKind::ReadStdin { .. }
                | StepKind::CallProgram { .. }
        ) {
            return Ok(false);
        }
        if let StepKind::Invoke { call, .. }
        | StepKind::IfCommand { call, .. }
        | StepKind::Loop {
            kind: crate::ast::LoopKind::WhileCommand { call, .. },
            ..
        } = &step.kind
        {
            let manifest = registry.resolve_call(call)?;
            if manifest.builtin.is_none() && !manifest.cacheable {
                return Ok(false);
            }
        }
        if let StepKind::TaskGroup { tasks, .. } = &step.kind {
            for call in tasks {
                let manifest = registry.resolve_call(call)?;
                if manifest.builtin.is_none() && !manifest.cacheable {
                    return Ok(false);
                }
            }
        }
        if let StepKind::SpawnJob { stages, .. } = &step.kind {
            for stage in stages {
                let manifest = registry.resolve_call(&stage.call)?;
                if manifest.builtin.is_none() && !manifest.cacheable {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

pub(super) fn uncertain_on_failure(
    program: &Program,
    start: usize,
    end: usize,
    registry: &Registry,
) -> Result<bool> {
    for step in &program.steps[start..end] {
        if let StepKind::Invoke { call, .. }
        | StepKind::IfCommand { call, .. }
        | StepKind::Loop {
            kind: crate::ast::LoopKind::WhileCommand { call, .. },
            ..
        } = &step.kind
        {
            if registry.resolve_call(call)?.uncertain_on_failure {
                return Ok(true);
            }
        }
        if let StepKind::TaskGroup { tasks, .. } = &step.kind {
            for call in tasks {
                if registry.resolve_call(call)?.uncertain_on_failure {
                    return Ok(true);
                }
            }
        }
        if let StepKind::SpawnJob { stages, .. } = &step.kind {
            for stage in stages {
                if registry.resolve_call(&stage.call)?.uncertain_on_failure {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
pub(super) fn validate_checkpoint_controls(program: &Program, registry: &Registry) -> Result<()> {
    if let Some(step) = program
        .steps
        .iter()
        .find(|step| matches!(step.kind, StepKind::OnInterrupt { .. }))
    {
        bail!(
            "line {}: on interrupt is not supported with checkpointed runs yet",
            step.line
        );
    }
    // Byte captures live outside the journal and would be lost on resume.
    if let Some(step) = program.steps.iter().find(|step| {
        matches!(
            step.kind,
            StepKind::CaptureBytes { .. } | StepKind::UseBytes { .. }
        )
    }) {
        bail!(
            "line {}: byte captures are not supported with checkpointed runs yet",
            step.line
        );
    }
    if let Some(step) = program
        .steps
        .iter()
        .find(|step| matches!(step.kind, StepKind::Interactive))
    {
        bail!(
            "line {}: checkpointed and resumed programs do not support interactively",
            step.line
        );
    }
    validate_checkpoint_jobs(program)?;
    // Checkpoints record one node per top-level statement. An exit nested in a
    // compound node, a try block or a reusable call would end partway through
    // a node that resume cannot re-enter, so only top-level exits are supported.
    let mut nested = 0usize;
    for step in &program.steps {
        match &step.kind {
            StepKind::If { .. }
            | StepKind::IfCommand { .. }
            | StepKind::CallProgram { .. }
            | StepKind::Loop { .. }
            | StepKind::Try { .. } => nested += 1,
            StepKind::EndIf | StepKind::EndCall | StepKind::EndLoop { .. } | StepKind::EndTry => {
                nested = nested.saturating_sub(1)
            }
            StepKind::Exit { .. } | StepKind::Fail { .. } if nested > 0 => {
                bail!(
                    "line {}: checkpointed programs can exit or fail only at top level; move it out of if, loop, try and reusable-program bodies",
                    step.line
                );
            }
            _ => {}
        }
    }
    let mut depth = 0;
    for step in &program.steps {
        match &step.kind {
            StepKind::If { .. }
            | StepKind::IfCommand { .. }
            | StepKind::CallProgram { .. }
            | StepKind::Loop { .. } => depth += 1,
            StepKind::EndIf | StepKind::EndCall | StepKind::EndLoop { .. } => depth -= 1,
            StepKind::UseFolder { .. } if depth > 0 => {
                bail!(
                    "line {}: checkpointed conditionals cannot change directory; loops and reusable calls also require directory setup outside their bodies",
                    step.line
                );
            }
            StepKind::Invoke { call, .. }
                if depth > 0 && registry.resolve_call(call)?.builtin == Some(Builtin::Cd) =>
            {
                bail!(
                    "line {}: checkpointed conditionals cannot change directory; loops and reusable calls also require cd outside their bodies",
                    step.line
                );
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn prepare(program: &Program, registry: &Registry, journal: &Journal) -> Result<()> {
    prepare_with_modules(program, registry, journal, &BTreeMap::new())
}

pub(super) fn prepare_with_modules(
    program: &Program,
    registry: &Registry,
    journal: &Journal,
    module_fingerprints: &BTreeMap<String, String>,
) -> Result<()> {
    let mut start = 0;
    // Reuse content digests only while selected path metadata still matches.
    // Never retain the memo across resume attempts or execution boundaries.
    let mut binary_digests = HashMap::new();
    // Journals that name their nodes' first steps match each node by that id. A node
    // must then sit exactly where the completed prefix puts it, so a statement inserted
    // or removed above it is refused with its old and new line.
    let step_ids = journal
        .completed_nodes()
        .iter()
        .any(|node| node.step_id.is_some())
        .then(|| crate::ast::step_ids(&program.steps));
    for node in journal.completed_nodes() {
        if let (Some(id), Some(ids)) = (&node.step_id, &step_ids) {
            let old_line = node
                .line
                .map_or_else(|| "unknown".to_owned(), |line| line.to_string());
            match ids.iter().position(|candidate| candidate == id) {
                Some(index) if index == start => {}
                Some(index) => bail!(
                    "completed checkpoint node {id:?} moved from line {old_line} to line {new_line}: a statement was inserted or removed above completed node {id:?}; use a fresh checkpoint",
                    new_line = program.steps[index].line,
                ),
                None => bail!(
                    "completed checkpoint node {id:?} (line {old_line}) is no longer in the script: a statement was inserted or removed above completed node {id:?}, or its content changed; use a fresh checkpoint",
                ),
            }
        }
        if start >= program.steps.len() {
            bail!("completed checkpoint node changed: script lost a completed step");
        }
        let end = node_end(program, start);
        if node.start != start
            || node.end != end
            || node.fingerprint
                != with_module_fingerprints(
                    node_fingerprint_with_digests(
                        program,
                        start,
                        end,
                        registry,
                        &node.cwd_before,
                        &mut binary_digests,
                    )?,
                    module_fingerprints,
                )
        {
            bail!(
                "completed checkpoint node changed at line {}; use a fresh checkpoint",
                program.steps[start].line
            );
        }
        start = end;
    }
    // A completed top-level exit or fail is terminal: the steps after it never
    // run, so the chain legitimately stops before the end of the program.
    let terminal = journal.completed_nodes().last().is_some_and(|node| {
        node.end == node.start + 1
            && program.steps.get(node.start).is_some_and(|step| {
                matches!(step.kind, StepKind::Exit { .. } | StepKind::Fail { .. })
            })
    });
    if journal.is_finished() && !terminal && start != program.steps.len() {
        bail!("completed checkpoint program changed: use a fresh checkpoint for additional steps");
    }
    journal.ensure_retry_allowed()
}
pub(super) fn trigger_fingerprint(
    trigger: &Trigger,
    registry: &Registry,
    cwd: &Path,
) -> Result<String> {
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"execute-trigger-semantics-v1");
    let mut value = serde_json::to_value(trigger)?;
    // The enum uses external tagging; line numbers are parser diagnostics only.
    if let Some(variants) = value.as_object_mut() {
        for variant in variants.values_mut() {
            if let Some(fields) = variant.as_object_mut() {
                fields.remove("line");
            }
        }
    }
    hash_field(&mut hash, &serde_json::to_vec(&value)?);
    let mut binary_digests = HashMap::new();
    if let Trigger::Listen {
        source: ListenSource::Applet(call),
        ..
    } = trigger
    {
        manifest_identity(&mut hash, call, registry, cwd, &mut binary_digests, false)?;
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Run with durable state at each finite node boundary. Completed effects are skipped.
pub fn run_checkpointed(
    program: &Program,
    registry: &Registry,
    options: &RunOptions,
    checkpoint: &CheckpointOptions,
) -> Result<RunResult> {
    let signals =
        Signals::register_with_deadline(deadline_after(options.total_timeout, "total timeout")?)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    validate(program, registry)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    validate_checkpoint_controls(program, registry)?;
    super::handoff::validate(program, true)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    let configured = configured_inputs(program, registry, options)?;
    let options = configured.as_ref();
    if program.trigger.is_some() {
        bail!("triggered programs require run_service_checkpointed");
    }
    let cwd = fs::canonicalize(&options.working_directory)?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    crate::capabilities::preflight_with_cache_directory(
        program,
        registry,
        &cwd,
        options.cache_directory.as_deref(),
    )?;
    check_execution_budget(&signals.cancelled, signals.deadline)?;
    let module_session = preload_program_modules(program, registry, &signals, &cwd)?;
    if module_session.has_background_job_operations() {
        bail!("checkpointed and resumed programs do not support background job operations inside imported program modules");
    }
    module_session.validate_checkpoint_identity()?;
    let sensitivity_floor =
        workflow_sensitivity_floor(program, registry, &module_session, &cwd, None)?;
    signals.set_sensitivity_floor(sensitivity_floor.clone());
    authorize_checkpoint_persistence(registry, &sensitivity_floor)?;
    let mut checkpoint = checkpoint.clone();
    checkpoint.directory = cwd.join(&checkpoint.directory);
    crate::capabilities::authorize_host_path(registry, &checkpoint.directory, true, &cwd)?;
    let mut journal = Journal::open_with_inputs(
        &checkpoint,
        &cwd,
        "finite",
        options.max_output_bytes,
        input_signature(program, &options.inputs)?,
    )?;
    prepare_with_modules(program, registry, &journal, module_session.fingerprints())?;
    let already_finished = journal.is_finished();
    let result = run_finite_inner(
        program,
        registry,
        options,
        &signals,
        Input::Empty,
        None,
        Some(&mut journal),
    )?;
    if !already_finished {
        authorize_checkpoint_persistence(registry, &sensitivity_floor)?;
        journal.finish()?;
    }
    Ok(result)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ast::{Connection, Invocation, Step};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn prefix_digest_reuse_preserves_identities_paths_and_fresh_validation() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        for (directory, bytes) in [(&left, b"first".as_slice()), (&right, b"other".as_slice())] {
            std::fs::create_dir(directory).unwrap();
            std::fs::write(directory.join("tool"), bytes).unwrap();
            std::fs::set_permissions(
                directory.join("tool"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let registry = Registry::default();
        let program = crate::parser::parse("run \"./tool\"\nthen \"./tool\"\n").unwrap();
        let mut memo = HashMap::new();
        let mut original = Vec::new();
        for index in 0..2 {
            let expected = node_fingerprint(&program, index, index + 1, &registry, &left).unwrap();
            let reused = node_fingerprint_with_digests(
                &program,
                index,
                index + 1,
                &registry,
                &left,
                &mut memo,
            )
            .unwrap();
            assert_eq!(reused, expected);
            original.push(reused);
        }
        assert_eq!(memo.len(), 1);
        let right_identity =
            node_fingerprint_with_digests(&program, 0, 1, &registry, &right, &mut memo).unwrap();
        assert_ne!(right_identity, original[0]);
        assert_eq!(
            memo.len(),
            2,
            "relative binaries in distinct cwd values must not collide"
        );
        let before = std::fs::metadata(left.join("tool")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        std::fs::write(left.join("tool"), b"FIRST").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(left.join("tool"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        let after = std::fs::metadata(left.join("tool")).unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        let same_size_replacement =
            node_fingerprint_with_digests(&program, 0, 1, &registry, &left, &mut memo).unwrap();
        assert_ne!(
            same_size_replacement, original[0],
            "ctime must catch same-size writes with restored mtime"
        );
        std::fs::write(left.join("tool"), b"changed executable").unwrap();
        let changed_within_pass =
            node_fingerprint_with_digests(&program, 0, 1, &registry, &left, &mut memo).unwrap();
        assert_ne!(
            changed_within_pass, original[0],
            "metadata changes must invalidate even the current pass"
        );
        let mut next_validation = HashMap::new();
        for (index, old) in original.iter().enumerate() {
            let fresh = node_fingerprint_with_digests(
                &program,
                index,
                index + 1,
                &registry,
                &left,
                &mut next_validation,
            )
            .unwrap();
            assert_ne!(
                &fresh, old,
                "later validation must re-read executable content"
            );
            assert_eq!(
                fresh,
                node_fingerprint(&program, index, index + 1, &registry, &left).unwrap()
            );
        }
    }

    #[test]
    fn environment_hash_is_order_independent_and_uses_last_duplicate_value() {
        use std::ffi::OsString;
        fn digest(entries: &[(&str, &str)]) -> Vec<u8> {
            let mut hash = Sha256::new();
            hash_environment(
                &mut hash,
                entries
                    .iter()
                    .map(|(key, value)| (OsString::from(key), OsString::from(value))),
            );
            hash.finalize().to_vec()
        }
        assert_eq!(
            digest(&[("B", "2"), ("A", "1")]),
            digest(&[("A", "1"), ("B", "2")])
        );
        assert_eq!(
            digest(&[("A", "old"), ("B", "2"), ("A", "new")]),
            digest(&[("B", "2"), ("A", "new")])
        );
        assert_ne!(digest(&[("A", "1")]), digest(&[("A", "2")]));
        assert_ne!(digest(&[("A", "bc")]), digest(&[("Ab", "c")]));
    }

    #[test]
    fn binary_digest_reuse_preserves_fingerprint_and_is_scoped_to_one_pass() {
        let temp = tempfile::tempdir().unwrap();
        let manifest_dir = temp.path().join("manifests");
        std::fs::create_dir(&manifest_dir).unwrap();
        let binary = temp.path().join("probe");
        std::fs::write(&binary, b"first binary contents").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            manifest_dir.join("probe.json"),
            serde_json::json!({
                "name": "probe",
                "binary": binary.to_str().unwrap(),
                "arguments": []
            })
            .to_string(),
        )
        .unwrap();
        let registry = Registry::load(&manifest_dir).unwrap();
        let call = Invocation {
            operation: "probe".to_owned(),
            direct: false,
            discard_stderr: false,
            parameters: BTreeMap::new(),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        };
        let program = Program {
            definitions: Default::default(),
            inputs: Vec::new(),
            steps: vec![
                Step {
                    line: 1,
                    label: None,
                    kind: StepKind::Invoke {
                        call: call.clone(),
                        connection: Connection::Start,
                    },
                },
                Step {
                    line: 2,
                    label: None,
                    kind: StepKind::Invoke {
                        call,
                        connection: Connection::Pipe,
                    },
                },
            ],
            trigger: None,
        };

        let first = node_fingerprint(&program, 0, 2, &registry, temp.path()).unwrap();
        let mut expected = Sha256::new();
        hash_field(&mut expected, b"execute-node-semantics-v1");
        for step in &program.steps {
            hash_field(&mut expected, &serde_json::to_vec(&step.kind).unwrap());
            if let StepKind::Invoke { call, .. } = &step.kind {
                let manifest = registry.resolve_call(call).unwrap();
                hash_field(&mut expected, &serde_json::to_vec(manifest).unwrap());
                hash_field(&mut expected, binary.as_os_str().as_encoded_bytes());
                let contents = Sha256::digest(std::fs::read(&binary).unwrap());
                hash_field(&mut expected, &contents);
            }
        }
        assert_eq!(first, format!("{:x}", expected.finalize()));

        std::fs::write(&binary, b"second binary contents").unwrap();
        let after_change = node_fingerprint(&program, 0, 2, &registry, temp.path()).unwrap();
        assert_ne!(first, after_change);
    }
}
