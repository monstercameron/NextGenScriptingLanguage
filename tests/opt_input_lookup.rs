//! Explicit input-interface scaling probe; no native commands or provider calls.
use flow::{ast::Value, inputs, manifest::Registry, parser, runtime};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, hint::black_box, time::Instant};

#[test]
#[ignore = "explicit paired release performance measurements"]
#[allow(clippy::assertions_on_constants)] // Explicit release-only measurement guard.
fn input_schema_lookup() {
    assert!(
        !cfg!(debug_assertions),
        "run this ignored probe with --release"
    );
    let registry = Registry::default();
    let mut results = BTreeMap::new();
    for (count, iterations) in [(0, 1000), (1, 1000), (8, 100), (64, 20), (1000, 1)] {
        let mut source = String::new();
        let mut supplied = BTreeMap::new();
        let mut typed = BTreeMap::new();
        for n in 0..count {
            let name = format!("p_{n:04}");
            source.push_str(&format!("accept {name} as integer with default 42\n"));
            supplied.insert(name.clone(), "42".to_owned());
            typed.insert(name, Value::Integer(42));
        }
        let last = if count == 0 {
            "42".to_owned()
        } else {
            format!("p_{:04}", count - 1)
        };
        source.push_str(&format!("then encode {last} as json\n"));
        let program = parser::parse(&source).unwrap();
        let partial = supplied
            .last_key_value()
            .map(|(k, v)| BTreeMap::from([(k.clone(), v.clone())]))
            .unwrap_or_default();
        assert_eq!(
            inputs::parse_cli(&program.inputs, &supplied).unwrap(),
            typed
        );
        assert_eq!(
            inputs::resolve(&program.inputs, &typed, false).unwrap(),
            typed
        );
        assert_eq!(
            inputs::resolve(&program.inputs, &typed, true).unwrap(),
            typed
        );
        let described = inputs::describe(&program.inputs, &typed).unwrap();
        assert_eq!(described["inputs"].as_array().unwrap().len(), count);
        assert!(described["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|input| input["value"] == 42));
        runtime::validate_with_arguments(&program, &registry, &typed).unwrap();
        let input_sha = format!("{:x}", Sha256::digest(serde_json::to_vec(&typed).unwrap()));
        for operation in [
            "validate",
            "parse_cli_all",
            "parse_cli_last",
            "resolve_all",
            "resolve_locals",
            "describe_all",
            "validate_program_arguments",
        ] {
            let mut raw = Vec::new();
            for batch in 0..60 {
                let start = Instant::now();
                for _ in 0..iterations {
                    match operation {
                        "validate" => {
                            inputs::validate(black_box(&program.inputs)).unwrap();
                        }
                        "parse_cli_all" => {
                            black_box(
                                inputs::parse_cli(black_box(&program.inputs), black_box(&supplied))
                                    .unwrap(),
                            );
                        }
                        "parse_cli_last" => {
                            black_box(
                                inputs::parse_cli(black_box(&program.inputs), black_box(&partial))
                                    .unwrap(),
                            );
                        }
                        "resolve_all" => {
                            black_box(
                                inputs::resolve(
                                    black_box(&program.inputs),
                                    black_box(&typed),
                                    false,
                                )
                                .unwrap(),
                            );
                        }
                        "resolve_locals" => {
                            black_box(
                                inputs::resolve(
                                    black_box(&program.inputs),
                                    black_box(&typed),
                                    true,
                                )
                                .unwrap(),
                            );
                        }
                        "describe_all" => {
                            black_box(
                                inputs::describe(black_box(&program.inputs), black_box(&typed))
                                    .unwrap(),
                            );
                        }
                        "validate_program_arguments" => {
                            runtime::validate_with_arguments(
                                black_box(&program),
                                black_box(&registry),
                                black_box(&typed),
                            )
                            .unwrap();
                        }
                        _ => unreachable!(),
                    }
                }
                if batch >= 10 {
                    raw.push(start.elapsed().as_secs_f64() * 1e9 / iterations as f64);
                }
            }
            let mut sorted = raw.clone();
            sorted.sort_by(f64::total_cmp);
            results.insert(format!("{operation}_{count:04}"),json!({"p50_ns":(sorted[24]+sorted[25])/2.0,"p95_ns":sorted[47],"raw_batch_average_ns":raw,"operations_per_batch":iterations,"declarations":count,"source_sha256":format!("{:x}",Sha256::digest(source.as_bytes())),"supplied_inputs_sha256":input_sha}));
        }
    }
    let report = json!({"method":"Native in-process input interface operations, includes allocations/destruction and schema validation; fixture creation, source parse, hashing excluded. 10 warmup plus 50 measured batches, batch size scales by declaration count. Exact map/type/inspection oracles checked before timing. No child processes. Large interfaces are scaling probes, not typical script speedups.","results":results});
    std::fs::write(
        std::env::var("OPT_BENCH_REPORT").expect("set OPT_BENCH_REPORT"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
