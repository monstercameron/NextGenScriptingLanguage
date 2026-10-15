use super::*;
use serde_json::json;

#[test]
#[ignore = "explicit release semantic-node fingerprint measurements; no child processes"]
#[allow(clippy::assertions_on_constants)]
fn checkpoint_fingerprints() -> Result<()> {
    assert!(!cfg!(debug_assertions), "use --release");
    let directory = tempfile::tempdir()?;
    let registry = Registry::default();
    let mut results = BTreeMap::new();
    for (name, calls, count) in [
        ("json_1", false, 1),
        ("json_64", false, 64),
        ("json_1000", false, 1000),
        ("reusable_1", true, 1),
        ("reusable_64", true, 64),
        ("reusable_256", true, 256),
    ] {
        let mut source = if calls {
            format!(
                "define a program named worker\n  accept payload as text with default \"{}\"\n  then encode payload as json\n",
                "x".repeat(1024)
            )
        } else {
            String::new()
        };
        source.push_str("repeat 0 times\n");
        source.push_str(&if calls {
            "  run program worker\n".repeat(count)
        } else {
            "  then encode 42 as json\n".repeat(count)
        });
        let program = crate::parser::parse(&source)?;
        validate_checkpoint_controls(&program, &registry)?;
        let expected = node_fingerprint(
            &program,
            0,
            program.steps.len(),
            &registry,
            directory.path(),
        )?;
        assert_eq!(expected.len(), 64);
        let mut raw = Vec::new();
        for batch in 0..60 {
            let start = Instant::now();
            for _ in 0..10 {
                std::hint::black_box(node_fingerprint(
                    std::hint::black_box(&program),
                    0,
                    program.steps.len(),
                    &registry,
                    directory.path(),
                )?);
            }
            if batch >= 10 {
                raw.push(start.elapsed().as_secs_f64() * 1e9 / 10.0);
            }
        }
        let mut sorted = raw.clone();
        sorted.sort_by(f64::total_cmp);
        results.insert(
            name,
            json!({
                "source_sha256": format!("{:x}", Sha256::digest(source.as_bytes())),
                "expanded_steps": program.steps.len(), "fingerprint": expected,
                "p50_ns": (sorted[24]+sorted[25])/2.0, "p95_ns": sorted[47],
                "raw_batch_average_ns": raw,
            }),
        );
    }
    let report = json!({"method": "Private semantic-node fingerprint entry, parsed compound nodes and registry prepared outside timing. Exact length-framed serialization and SHA-256 plus temporary buffer destruction included. No native executable or provider calls. 10 warmup batches, 50 measured batches of 10 operations, serial release execution; not journal durability, full checkpoint/CLI latency or RSS measurement. Fingerprints and input source hashes retained for cross-binary equality.", "results": results});
    fs::write(
        std::env::var("OPT_BENCH_REPORT")?,
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

// Exact pre-optimization serializer retained as the identity oracle.
fn legacy_fingerprint_with_digests(
    program: &Program,
    start: usize,
    end: usize,
    registry: &Registry,
    cwd: &Path,
    binary_digests: &mut BinaryDigests,
) -> Result<String> {
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"execute-node-semantics-v1");
    for step in &program.steps[start..end] {
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
            hash_field(
                &mut hash,
                &serde_json::to_vec(&StepKind::CallProgram {
                    call: call.clone(),
                    inputs,
                    end: *end,
                    connection: *connection,
                })?,
            );
        } else {
            hash_field(&mut hash, &serde_json::to_vec(&step.kind)?);
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
                    StepKind::If { .. } | StepKind::CallProgram { .. } | StepKind::Loop { .. }
                ),
            )?;
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[test]
fn fingerprint_buffer_reuse_matches_legacy_serialization_and_line_normalization() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let registry = Registry::default();
    let sources = [
        String::new(),
        "repeat 0 times\n  then encode 42 as json\n".into(),
        "if true is true\n  read \"雪; $HOME\" as data\n  then parse json as doc\n  take integer at \"/n\" from doc as number\n  then encode number as json\notherwise\n  read from stdin as raw\n  then save to \"out\"\nfor each item in [\"a\", \"b\"]\n  then encode item as json\nwhile false is true\n  then encode \"done\" as json\n".into(),
        format!("define a program named worker\n  accept payload as text with default \"{}\"\n  then encode payload as json\nrepeat 0 times\n{}", "雪\\n\\\"\\\\".repeat(128), "  run program worker\n".repeat(64)),
    ];
    for source in sources {
        let mut program = crate::parser::parse(&source)?;
        let expected = legacy_fingerprint_with_digests(
            &program,
            0,
            program.steps.len(),
            &registry,
            directory.path(),
            &mut HashMap::new(),
        )?;
        assert_eq!(
            node_fingerprint(
                &program,
                0,
                program.steps.len(),
                &registry,
                directory.path()
            )?,
            expected
        );
        for step in &mut program.steps {
            step.line += 100;
            if let StepKind::CallProgram { inputs, .. } = &mut step.kind {
                for input in inputs {
                    input.line += 200;
                }
            }
        }
        assert_eq!(
            node_fingerprint(
                &program,
                0,
                program.steps.len(),
                &registry,
                directory.path()
            )?,
            expected
        );
        if let Some(step) = program
            .steps
            .iter_mut()
            .find(|step| matches!(step.kind, StepKind::EncodeJson { .. }))
        {
            if let StepKind::EncodeJson { source } = &mut step.kind {
                *source = Value::Integer(777);
            }
            assert_ne!(
                node_fingerprint(
                    &program,
                    0,
                    program.steps.len(),
                    &registry,
                    directory.path()
                )?,
                expected
            );
        }
    }
    Ok(())
}
