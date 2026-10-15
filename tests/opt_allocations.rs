//! Explicit release-only allocation probe, separate from latency benchmarks.
//!
//! Run with `cargo test --release --test opt_allocations -- --ignored` and set
//! `OPT_ALLOC_REPORT` to the JSON output path. The allocator wrapper delegates
//! to `System`; counting is enabled only around one selected operation at a
//! time, and report construction/writing happens after counting is disabled.

use flow::{
    ast::{Invocation, Value},
    manifest::Registry,
    parser, runtime,
    schedule::CronSchedule,
    ui::syntax,
};
use serde::Serialize;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::BTreeMap,
    hint::black_box,
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATION_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_REQUESTED_BYTES: AtomicU64 = AtomicU64::new(0);
static REALLOCATION_CALLS: AtomicU64 = AtomicU64::new(0);
static REALLOCATION_REQUESTED_BYTES: AtomicU64 = AtomicU64::new(0);

struct CountingSystem;

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingSystem = CountingSystem;

unsafe impl GlobalAlloc for CountingSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOCATION_REQUESTED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: This wrapper forwards the original layout unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOCATION_REQUESTED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: This wrapper forwards the original layout unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: This wrapper forwards the original pointer and layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            REALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            REALLOCATION_REQUESTED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        // SAFETY: This wrapper forwards the original pointer/layout and requested size.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[derive(Debug, Serialize)]
struct Counts {
    allocation_calls: u64,
    allocation_requested_bytes: u64,
    reallocation_calls: u64,
    reallocation_requested_bytes: u64,
}

struct CountingScope {
    active: bool,
}

impl CountingScope {
    fn begin() -> Self {
        assert!(
            !COUNTING.load(Ordering::Relaxed),
            "allocation scopes cannot nest"
        );
        ALLOCATION_CALLS.store(0, Ordering::Relaxed);
        ALLOCATION_REQUESTED_BYTES.store(0, Ordering::Relaxed);
        REALLOCATION_CALLS.store(0, Ordering::Relaxed);
        REALLOCATION_REQUESTED_BYTES.store(0, Ordering::Relaxed);
        COUNTING.store(true, Ordering::SeqCst);
        Self { active: true }
    }

    fn finish(mut self) -> Counts {
        COUNTING.store(false, Ordering::SeqCst);
        self.active = false;
        Counts {
            allocation_calls: ALLOCATION_CALLS.load(Ordering::Relaxed),
            allocation_requested_bytes: ALLOCATION_REQUESTED_BYTES.load(Ordering::Relaxed),
            reallocation_calls: REALLOCATION_CALLS.load(Ordering::Relaxed),
            reallocation_requested_bytes: REALLOCATION_REQUESTED_BYTES.load(Ordering::Relaxed),
        }
    }
}

impl Drop for CountingScope {
    fn drop(&mut self) {
        if self.active {
            COUNTING.store(false, Ordering::SeqCst);
        }
    }
}

fn measure<T>(operation: impl FnOnce() -> T) -> Counts {
    let scope = CountingScope::begin();
    // Black-box the result and destroy it before stopping the counter. This is
    // an allocation count, not a live-byte or peak-RSS measurement.
    drop(black_box(operation()));
    scope.finish()
}

#[test]
#[ignore = "explicit release-only allocation measurements"]
#[allow(clippy::assertions_on_constants)] // Keep the ignored probe buildable in debug; reject running it there.
fn report_release_allocation_counts() {
    assert!(
        !cfg!(debug_assertions),
        "run this ignored probe with --release for representative counts"
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifests_path = root.join("manifests");
    let registry = Registry::load(&manifests_path).unwrap();
    let mut paths: Vec<_> = std::fs::read_dir(&manifests_path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    let sources: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect();

    // All operation inputs, including the 1,000-line source and list values,
    // are ready before any counter scope begins.
    let three_commands = "run printf with message \"hello\"\npipe to tr with from \"[:lower:]\" and to \"[:upper:]\"\npipe to wc with flags [\"-l\"]\n";
    let thousand_commands = "run printf with message \"one\"\n".repeat(1000);
    let screen_source = include_str!("../examples/top-cpu.program");
    let cron_source = "*/5 0-23/2 * * 0,7";
    let ls = registry.resolve("ls").unwrap();
    let mut parameters = BTreeMap::new();
    parameters.insert("path".to_owned(), Value::String(".".to_owned()));
    parameters.insert(
        "flags".to_owned(),
        Value::List((0..256).map(|_| Value::String("-a".to_owned())).collect()),
    );
    let invocation = Invocation {
        operation: "ls".to_owned(),
        direct: false,
        parameters,
        binding: None,
        cache: false,
        status_binding: None,
        result_binding: None,
    };

    let directory = tempfile::tempdir().unwrap();
    let input_program = parser::parse("accept payload as json\ntake integer at \"/n\" from payload as number\nthen encode number as json\n").unwrap();
    let input_options = runtime::RunOptions {
        inputs: BTreeMap::from([(
            "payload".into(),
            Value::Record(BTreeMap::from([
                ("n".into(), Value::Integer(42)),
                ("padding".into(), Value::String("x".repeat(4 * 1024 * 1024))),
            ])),
        )]),
        working_directory: directory.path().into(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 32,
        command_timeout: Some(std::time::Duration::from_secs(5)),
        total_timeout: None,
    };
    let checked = runtime::run(&input_program, &registry, &input_options).unwrap();
    assert_eq!(checked.output, b"42");
    assert_eq!(checked.launched_processes, 0);

    let mut operations = BTreeMap::new();
    operations.insert(
        "runtime_json_input_4mib",
        measure(|| {
            runtime::run(
                black_box(&input_program),
                black_box(&registry),
                black_box(&input_options),
            )
            .unwrap()
        }),
    );
    operations.insert(
        "registry_load",
        measure(|| Registry::load(black_box(&manifests_path)).unwrap()),
    );
    operations.insert(
        "registry_from_sources",
        measure(|| Registry::from_sources(black_box(&sources)).unwrap()),
    );
    operations.insert(
        "registry_clone",
        measure(|| Registry::clone(black_box(&registry))),
    );
    operations.insert(
        "parser_three_commands",
        measure(|| parser::parse(black_box(three_commands)).unwrap()),
    );
    operations.insert(
        "parser_thousand_commands",
        measure(|| parser::parse(black_box(thousand_commands.as_str())).unwrap()),
    );
    operations.insert(
        "ui_syntax_parse",
        measure(|| syntax::parse(black_box(screen_source)).unwrap()),
    );
    operations.insert(
        "manifest_argv_list256",
        measure(|| {
            ls.build_arguments(black_box(&invocation.parameters))
                .unwrap()
        }),
    );
    operations.insert(
        "cron_parse",
        measure(|| CronSchedule::parse(black_box(cron_source)).unwrap()),
    );

    // Interface fixtures are built outside all counter scopes. Measure index
    // allocation traffic separately from the returned owned value maps.
    for (count, validate_name, parse_name, resolve_name) in [
        (
            0,
            "input_validate_0000",
            "input_parse_cli_0000",
            "input_resolve_0000",
        ),
        (
            1,
            "input_validate_0001",
            "input_parse_cli_0001",
            "input_resolve_0001",
        ),
        (
            8,
            "input_validate_0008",
            "input_parse_cli_0008",
            "input_resolve_0008",
        ),
        (
            64,
            "input_validate_0064",
            "input_parse_cli_0064",
            "input_resolve_0064",
        ),
        (
            1000,
            "input_validate_1000",
            "input_parse_cli_1000",
            "input_resolve_1000",
        ),
    ] {
        let declarations: Vec<_> = (0..count)
            .map(|n| flow::ast::InputDeclaration {
                name: format!("p_{n:04}"),
                kind: flow::ast::InputType::Integer,
                default: Some(Value::Integer(42)),
                source: None,
                line: n + 1,
            })
            .collect();
        let supplied: BTreeMap<_, _> = declarations
            .iter()
            .map(|d| (d.name.clone(), "42".to_owned()))
            .collect();
        let typed: BTreeMap<_, _> = declarations
            .iter()
            .map(|d| (d.name.clone(), Value::Integer(42)))
            .collect();
        assert_eq!(
            flow::inputs::parse_cli(&declarations, &supplied).unwrap(),
            typed
        );
        assert_eq!(
            flow::inputs::resolve(&declarations, &typed, false).unwrap(),
            typed
        );
        operations.insert(
            validate_name,
            measure(|| flow::inputs::validate(black_box(&declarations)).unwrap()),
        );
        operations.insert(
            parse_name,
            measure(|| {
                flow::inputs::parse_cli(black_box(&declarations), black_box(&supplied)).unwrap()
            }),
        );
        operations.insert(
            resolve_name,
            measure(|| {
                flow::inputs::resolve(black_box(&declarations), black_box(&typed), false).unwrap()
            }),
        );
    }

    // Completed compound checkpoints exercise fingerprint allocation traffic
    // through the public API. Parsing, durable setup and directory construction
    // stay outside the counter; resume still validates and reads real journals.
    let checkpoint_root = tempfile::tempdir().unwrap();
    for (name, calls, count) in [
        ("checkpoint_resume_1", false, 1),
        ("checkpoint_resume_64", false, 64),
        ("checkpoint_resume_1000", false, 1000),
        ("checkpoint_resume_reusable256", true, 256),
    ] {
        let mut source = if calls {
            format!("define a program named worker\n  accept payload as text with default \"{}\"\n  then encode payload as json\n", "x".repeat(1024))
        } else {
            String::new()
        };
        source.push_str("repeat 0 times\n");
        source.push_str(&if calls {
            "  run program worker\n".repeat(count)
        } else {
            "  then encode 42 as json\n".repeat(count)
        });
        let program = parser::parse(&source).unwrap();
        let source_path = checkpoint_root.path().join(format!("{name}.program"));
        std::fs::write(&source_path, source).unwrap();
        let options = runtime::RunOptions {
            inputs: BTreeMap::new(),
            working_directory: checkpoint_root.path().into(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 32,
            command_timeout: None,
            total_timeout: None,
        };
        let mut checkpoint = runtime::CheckpointOptions {
            directory: checkpoint_root.path().join(name),
            program_path: source_path,
            resume: false,
            retry_uncertain: false,
        };
        let result = runtime::run_checkpointed(&program, &registry, &options, &checkpoint).unwrap();
        assert!(result.output.is_empty());
        assert_eq!(result.launched_processes, 0);
        checkpoint.resume = true;
        operations.insert(
            name,
            measure(|| {
                runtime::run_checkpointed(&program, &registry, &options, &checkpoint).unwrap()
            }),
        );
    }

    // JSON construction, serialization, environment access, and file output
    // all occur with allocation counting disabled.
    let report = serde_json::json!({
        "method": {
            "build": "release; debug assertions are rejected",
            "allocator": "process-local global allocator wrapper delegates all memory operations to std::alloc::System",
            "scope": "counters reset before each synchronous operation and are enabled only for that black_box operation and destruction of its result",
            "allocation_bytes": "allocation_requested_bytes sums Layout::size() for alloc and alloc_zeroed calls",
            "reallocation_bytes": "reallocation_requested_bytes sums each realloc new_size request",
            "excluded": "deallocation calls, net live bytes, peak live bytes, and peak RSS are not counted",
            "inputs": "paths, source strings, registry used by clone/argv, invocation, 256 list values, parsed JSON-selection program/options with 4 MiB padding, and completed compound checkpoints are prepared before measurement",
            "reporting": "JSON construction, serialization, environment lookup, and report file writing happen after counting is disabled"
        },
        "operations": operations
    });
    let report_path =
        std::env::var("OPT_ALLOC_REPORT").expect("set OPT_ALLOC_REPORT to the JSON output path");
    std::fs::write(report_path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
}
