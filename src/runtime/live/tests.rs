use super::*;
use crate::parser;
use serde_json::json;

fn options(directory: &Path) -> RunOptions {
    RunOptions {
        max_output_bytes: 8 * 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        ..RunOptions::new(directory.into())
    }
}

#[test]
fn live_node_publishes_final_cwd_bindings_and_pipeline_bytes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::create_dir(directory.path().join("child"))?;
    let program = parser::parse("use folder \"child\"\nthen encode 42 as json\n")?;
    let context = NodeState {
        cwd: directory.path().into(),
        bindings: BTreeMap::from([("untouched".into(), Value::Integer(17))]),
        data: b"old input".to_vec(),
    };
    let (result, state) = execute_node(
        &program,
        &Registry::default(),
        &options(directory.path()),
        context,
        Arc::new(AtomicBool::new(false)),
    )?;
    assert_eq!(state.cwd, fs::canonicalize(directory.path().join("child"))?);
    assert_eq!(
        state.bindings,
        BTreeMap::from([("untouched".into(), Value::Integer(17))])
    );
    assert_eq!(state.data, b"42");
    assert_eq!(result.output, state.data);
    assert_eq!(result.launched_processes, 0);
    Ok(())
}

#[test]
fn live_empty_node_preserves_input_and_cancelled_node_returns_no_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let context = NodeState {
        cwd: directory.path().into(),
        bindings: BTreeMap::from([("kept".into(), Value::String("value".into()))]),
        data: vec![0, 255, 0, 1],
    };
    let (result, state) = execute_node(
        &parser::parse("")?,
        &Registry::default(),
        &options(directory.path()),
        context.clone(),
        Arc::new(AtomicBool::new(false)),
    )?;
    assert_eq!(result.output, context.data);
    assert_eq!(state.data, context.data);
    assert_eq!(state.bindings, context.bindings);
    let error = execute_node(
        &parser::parse("then encode 42 as json\n")?,
        &Registry::default(),
        &options(directory.path()),
        context,
        Arc::new(AtomicBool::new(true)),
    )
    .err()
    .expect("cancelled node must not publish a success state");
    assert!(format!("{error:#}").contains("cancelled"));
    Ok(())
}

#[test]
fn live_job_batch_survives_a_node_boundary_until_awaited() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let program = parser::parse(
        "repeat 2 times\n  spawn as item\n    run \"printf\" with arguments [\"x\"]\nawait item\n",
    )?;
    let StepKind::Loop { end, .. } = &program.steps[0].kind else {
        panic!("fixture starts with a loop node");
    };
    let end = *end;
    let first = Program {
        definitions: program.definitions.clone(),
        inputs: program.inputs.clone(),
        steps: program.steps[..end].to_vec(),
        trigger: None,
    };
    let second = Program {
        definitions: program.definitions.clone(),
        inputs: program.inputs.clone(),
        steps: program.steps[end..].to_vec(),
        trigger: None,
    };
    let jobs = Arc::new(JobSession::new());
    let context = NodeState {
        cwd: directory.path().into(),
        bindings: BTreeMap::new(),
        data: Vec::new(),
    };
    let (_, state) = execute_node_with_session(
        &first,
        &Registry::default(),
        &options(directory.path()),
        context,
        Arc::new(AtomicBool::new(false)),
        JobNode {
            session: Arc::clone(&jobs),
            last_uses: BTreeMap::new(),
            final_node: false,
        },
    )?;
    let (result, _) = execute_node_with_session(
        &second,
        &Registry::default(),
        &options(directory.path()),
        state,
        Arc::new(AtomicBool::new(false)),
        JobNode {
            session: jobs,
            last_uses: BTreeMap::from([("item".into(), 0)]),
            final_node: true,
        },
    )?;
    assert_eq!(result.output, b"xx");
    Ok(())
}

#[test]
fn live_session_refreshes_job_deadlines_for_each_node() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let started = directory.path().join("job-started");
    let gate = directory.path().join("job-gate");
    let finished = directory.path().join("job-finished");
    let script = r#"printf started > "$1"; while [ ! -e "$2" ]; do sleep 0.1; done; printf first; printf done > "$3""#;
    let program = parser::parse(&format!(
        "spawn as first\n  run \"sh\" with arguments [\"-c\", {}, \"job\", {}, {}, {}]\nspawn as second\n  run \"printf\" with arguments [\"second\"]\nawait first\nawait second\n",
        serde_json::to_string(script)?,
        serde_json::to_string(&started.to_string_lossy())?,
        serde_json::to_string(&gate.to_string_lossy())?,
        serde_json::to_string(&finished.to_string_lossy())?,
    ))?;
    let spawn = program
        .steps
        .iter()
        .position(|step| matches!(&step.kind, StepKind::SpawnJob { name, .. } if name == "first"))
        .expect("program has a first job spawn");
    let split = spawn + 1;
    let first = Program {
        definitions: program.definitions.clone(),
        inputs: program.inputs.clone(),
        steps: program.steps[..split].to_vec(),
        trigger: None,
    };
    let second = Program {
        definitions: program.definitions.clone(),
        inputs: program.inputs.clone(),
        steps: program.steps[split..].to_vec(),
        trigger: None,
    };
    let source_last_uses = job_last_uses(&program.steps);
    let first_last_uses = source_last_uses
        .iter()
        .filter_map(|(name, index)| (*index < split).then_some((name.clone(), *index)))
        .collect();
    let second_last_uses = source_last_uses
        .into_iter()
        .filter_map(|(name, index)| (index >= split).then_some((name, index - split)))
        .collect();
    let jobs = Arc::new(JobSession::new());
    let context = NodeState {
        cwd: directory.path().into(),
        bindings: BTreeMap::new(),
        data: Vec::new(),
    };
    let mut node_options = options(directory.path());
    let node_timeout = Duration::from_secs(2);
    node_options.total_timeout = Some(node_timeout);
    let (_, state) = execute_node_with_session(
        &first,
        &Registry::default(),
        &node_options,
        context,
        Arc::new(AtomicBool::new(false)),
        JobNode {
            session: Arc::clone(&jobs),
            last_uses: first_last_uses,
            final_node: false,
        },
    )
    .context("first node should finish within its own deadline")?;
    let started_deadline = Instant::now() + Duration::from_secs(5);
    while !started.exists() && Instant::now() < started_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        started.exists(),
        "the carried job should start before node two"
    );
    // Wait beyond node one's deadline while the worker is held by the gate.
    // Releasing it before node two lets that node finish within its fresh
    // timeout, while a stale live TaskScope deadline would reject its spawn.
    std::thread::sleep(node_timeout + Duration::from_millis(100));
    assert!(!finished.exists(), "the carried job should still be gated");
    std::fs::write(&gate, b"release")?;
    let (result, _) = execute_node_with_session(
        &second,
        &Registry::default(),
        &node_options,
        state,
        Arc::new(AtomicBool::new(false)),
        JobNode {
            session: jobs,
            last_uses: second_last_uses,
            final_node: true,
        },
    )
    .context("second node should use a fresh job deadline")?;
    assert_eq!(result.output, b"second");
    assert!(
        finished.exists(),
        "the carried job should finish after release"
    );
    Ok(())
}

#[test]
#[ignore = "explicit paired release measurements; no child processes"]
#[allow(clippy::assertions_on_constants)] // Build in debug; run only in release.
fn live_state_handoff() -> Result<()> {
    assert!(!cfg!(debug_assertions), "use --release");
    let directory = tempfile::tempdir()?;
    let registry = Registry::default();
    let options = options(directory.path());
    let mut results = BTreeMap::new();
    for (name, binding_bytes, data_bytes) in [
        ("small_control", 0, 0),
        ("bindings_64k", 64 * 1024, 0),
        ("bindings_1m", 1024 * 1024, 0),
        ("bindings_4m", 4 * 1024 * 1024, 0),
        ("pipeline_64k", 0, 64 * 1024),
        ("pipeline_1m", 0, 1024 * 1024),
        ("pipeline_4m", 0, 4 * 1024 * 1024),
    ] {
        let source = if data_bytes == 0 {
            "then encode 42 as json\n"
        } else {
            ""
        };
        let program = parser::parse(source)?;
        let context = NodeState {
            cwd: directory.path().into(),
            bindings: if binding_bytes == 0 {
                BTreeMap::new()
            } else {
                BTreeMap::from([("padding".into(), Value::String("x".repeat(binding_bytes)))])
            },
            data: vec![b'x'; data_bytes],
        };
        let token = Arc::new(AtomicBool::new(false));
        let (result, state) = execute_node(
            &program,
            &registry,
            &options,
            context.clone(),
            token.clone(),
        )?;
        let expected = if data_bytes == 0 {
            b"42".as_slice()
        } else {
            context.data.as_slice()
        };
        assert_eq!(result.output, expected);
        assert_eq!(state.data, expected);
        assert_eq!(state.bindings, context.bindings);
        assert_eq!(state.cwd, fs::canonicalize(directory.path())?);
        assert_eq!(result.launched_processes, 0);
        assert_eq!(result.cache_hits, 0);
        let mut raw = Vec::new();
        for sample in 0..140 {
            // The caller supplies owned state. Its construction and Arc clone
            // happen before the timer, identically for both source versions.
            let owned = context.clone();
            let token = token.clone();
            let start = Instant::now();
            drop(std::hint::black_box(execute_node(
                std::hint::black_box(&program),
                &registry,
                &options,
                owned,
                token,
            )?));
            if sample >= 20 {
                raw.push(start.elapsed().as_secs_f64() * 1e9);
            }
        }
        let mut sorted = raw.clone();
        sorted.sort_by(f64::total_cmp);
        results.insert(name, json!({
            "source": source, "binding_bytes": binding_bytes, "input_bytes": data_bytes,
            "input_sha256": format!("{:x}", Sha256::digest(&context.data)),
            "bindings_sha256": format!("{:x}", Sha256::digest(serde_json::to_vec(&context.bindings)?)),
            "p50_ns": (sorted[59] + sorted[60])/2.0, "p95_ns": sorted[113], "raw_ns": raw,
        }));
    }
    let report = json!({"method": "Private managed-live runtime entry with owned NodeState. Fixture clone, parsing, hashes, registry and token construction excluded. Signal registration, cwd canonicalization, contextual preflight, execution, publication and result/state destruction included. Exact output bytes, full bindings/cwd, zero child processes/cache hits checked outside timings. 20 warmups; 120 measured operations per case. Warm filesystem; not RPC, CLI or isolated copy timing. No allocation, peak memory or RSS measurement.", "results": results});
    fs::write(
        std::env::var("OPT_BENCH_REPORT")?,
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
