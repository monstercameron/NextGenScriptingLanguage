//! Explicit in-process benchmark: no subprocess work; run optimized with --ignored.
use chrono::TimeZone;
use flow::{
    ast::{Connection, Invocation, StepKind, Value},
    manifest::Registry,
    parser, runtime,
    schedule::CronSchedule,
};
use serde_json::json;
use std::{collections::BTreeMap, hint::black_box, path::Path, time::Instant};

#[test]
#[ignore = "explicit release performance measurements"]
fn core_operations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let registry = Registry::load(&root.join("manifests")).unwrap();
    let short="run printf with message \"hello\"\npipe to tr with from \"[:lower:]\" and to \"[:upper:]\"\npipe to wc with flags [\"-l\"]\n";
    let long = "run printf with message \"one\"\n".repeat(1000);
    let parsed = parser::parse(short).unwrap();
    runtime::validate(&parsed, &registry).unwrap();
    let ls = registry.resolve("ls").unwrap();
    let mut params = BTreeMap::new();
    params.insert("path".into(), Value::String(".".into()));
    params.insert(
        "flags".into(),
        Value::List((0..256).map(|_| Value::String("-a".into())).collect()),
    );
    let invocation = Invocation {
        operation: "ls".into(),
        direct: false,
        parameters: params,
        binding: None,
        cache: false,
        status_binding: None,
    };
    ls.validate(&invocation).unwrap();
    let cron = CronSchedule::parse("*/5 0-23/2 * * 0,7").unwrap();
    let datetime = chrono::Local
        .with_ymd_and_hms(2026, 10, 11, 12, 10, 0)
        .unwrap();
    let mut measurements = BTreeMap::new();
    for (name, iterations) in [
        ("parse_three_commands", 1000),
        ("parse_thousand_commands", 5),
        ("validate_three_commands", 1000),
        ("manifest_validate_list256", 1000),
        ("manifest_argv_list256", 500),
        ("registry_clone", 1000),
        ("registry_load", 5),
        ("cron_parse", 1000),
        ("cron_matches", 10000),
    ] {
        let mut values = Vec::new();
        for batch in 0..110 {
            let started = Instant::now();
            for _ in 0..iterations {
                match name {
                    "parse_three_commands" => {
                        black_box(parser::parse(black_box(short)).unwrap());
                    }
                    "parse_thousand_commands" => {
                        black_box(parser::parse(black_box(&long)).unwrap());
                    }
                    "validate_three_commands" => {
                        runtime::validate(black_box(&parsed), black_box(&registry)).unwrap();
                    }
                    "manifest_validate_list256" => {
                        ls.validate(black_box(&invocation)).unwrap();
                    }
                    "manifest_argv_list256" => {
                        black_box(
                            ls.build_arguments(black_box(&invocation.parameters))
                                .unwrap(),
                        );
                    }
                    "registry_clone" => {
                        let copy: Registry = Registry::clone(black_box(&registry));
                        black_box(copy);
                    }
                    "registry_load" => {
                        black_box(Registry::load(black_box(&root.join("manifests"))).unwrap());
                    }
                    "cron_parse" => {
                        black_box(CronSchedule::parse(black_box("*/5 0-23/2 * * 0,7")).unwrap());
                    }
                    "cron_matches" => {
                        black_box(black_box(&cron).matches(black_box(&datetime)));
                    }
                    _ => unreachable!(),
                }
            }
            let ns = started.elapsed().as_secs_f64() * 1e9 / iterations as f64;
            if batch >= 10 {
                values.push(ns);
            }
        }
        let mut sorted = values.clone();
        sorted.sort_by(f64::total_cmp);
        measurements.insert(name,json!({"p50_ns":(sorted[49]+sorted[50])/2.,"p95_ns":sorted[94],"p99_ns":sorted[98],"raw_batch_average_ns":values,"operations_per_batch":iterations}));
    }
    // Workload data remain black-boxed and expected AST shape is checked.
    assert!(matches!(
        &parsed.steps[0].kind,
        StepKind::Invoke {
            connection: Connection::Start,
            ..
        }
    ));
    let report = json!({"version":env!("CARGO_PKG_VERSION"),"mode":"release optimized in-process functions, includes allocation/drop; excludes external commands and CLI startup","warmup_batches":10,"measured_batches":100,"operations":measurements});
    let path = std::env::var("OPT_BENCH_REPORT").expect("set OPT_BENCH_REPORT");
    std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
}
