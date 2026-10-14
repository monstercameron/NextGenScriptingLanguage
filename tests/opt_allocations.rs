//! Explicit release-only allocation probe, separate from latency benchmarks.
//!
//! Run with `cargo test --release --test opt_allocations -- --ignored` and set
//! `OPT_ALLOC_REPORT` to the JSON output path. The allocator wrapper delegates
//! to `System`; counting is enabled only around one selected operation at a
//! time, and report construction/writing happens after counting is disabled.

use flow::{
    ast::{Invocation, Value},
    manifest::Registry,
    parser,
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
    };

    let mut operations = BTreeMap::new();
    operations.insert(
        "registry_load",
        measure(|| Registry::load(black_box(&manifests_path)).unwrap()),
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
            "inputs": "paths, source strings, registry used by clone/argv, invocation, and 256 list values are prepared before measurement",
            "reporting": "JSON construction, serialization, environment lookup, and report file writing happen after counting is disabled"
        },
        "operations": operations
    });
    let report_path =
        std::env::var("OPT_ALLOC_REPORT").expect("set OPT_ALLOC_REPORT to the JSON output path");
    std::fs::write(report_path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
}
