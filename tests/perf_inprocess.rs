use flow::manifest::Registry;
use flow::parser;
use flow::runtime::{self, RunOptions};
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct CountingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record_allocation(size: usize) {
    let live = LIVE_BYTES.fetch_add(size, Ordering::Relaxed) + size;
    let mut peak = PEAK_BYTES.load(Ordering::Relaxed);
    while live > peak {
        match PEAK_BYTES.compare_exchange_weak(peak, live, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(current) => peak = current,
        }
    }
}

// SAFETY: Every allocation operation is forwarded to `System` with the original pointer/layout.
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: This allocator forwards the unchanged layout to the system allocator.
        let pointer = unsafe { std::alloc::System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: This pointer and layout came from the system allocator through this wrapper.
        unsafe { std::alloc::System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(
        &self,
        pointer: *mut u8,
        layout: std::alloc::Layout,
        new_size: usize,
    ) -> *mut u8 {
        // SAFETY: This pointer and layout came from the system allocator through this wrapper.
        let new_pointer = unsafe { std::alloc::System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            if new_size >= layout.size() {
                record_allocation(new_size - layout.size());
            } else {
                LIVE_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new_pointer
    }
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn json_input() -> Vec<u8> {
    let mut text = String::from("{\"a\":{\"b\":[");
    for id in 0..5000 {
        if id != 0 {
            text.push(',');
        }
        text.push_str(&format!(
            "{{\"id\":{id},\"name\":\"item-{id}\",\"tags\":[\"a\",\"b\"]}}"
        ));
    }
    text.push_str(" ]},\"meta\":{\"count\":5000}}");
    text.into_bytes()
}

fn benchmark(
    label: &str,
    source: &str,
    input: &[u8],
    bindings: impl Fn() -> std::collections::BTreeMap<String, flow::ast::Value>,
    registry: &Registry,
    iterations: usize,
) {
    let program = parser::parse(source).expect("benchmark source parses");
    let options = RunOptions::new(std::env::current_dir().expect("current directory"));
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut samples = Vec::with_capacity(iterations);
    let mut peak_extra = 0usize;
    for _ in 0..iterations {
        let baseline = LIVE_BYTES.load(Ordering::Relaxed);
        PEAK_BYTES.store(baseline, Ordering::Relaxed);
        let started = Instant::now();
        let result = runtime::run_with_context(
            &program,
            registry,
            &options,
            bindings(),
            input.to_vec(),
            Arc::clone(&cancelled),
        )
        .expect("benchmark run succeeds");
        black_box(result.output);
        samples.push(started.elapsed());
        peak_extra = peak_extra.max(PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(baseline));
    }
    eprintln!(
        "{label}: median {:?} over {iterations} runs; peak additional heap {peak_extra} bytes",
        median(samples),
    );
}

#[test]
#[ignore = "in-process performance harness"]
fn end_to_end_json_and_text() {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let registry = Registry::from_bundled(&cwd.join("manifests")).expect("bundled registry");
    let line = "the quick brown fox jumps over the lazy dog 0123456789\n";
    let mut text = line.repeat(1_000_000 / line.len() + 1);
    text.truncate(1_000_000);
    let json = json_input();
    eprintln!(
        "text input bytes: {}; JSON input bytes: {}",
        text.len(),
        json.len()
    );
    benchmark(
        "text upper",
        "text.upper with text text\n",
        &[],
        || {
            std::collections::BTreeMap::from([(
                "text".into(),
                flow::ast::Value::String(text.clone()),
            )])
        },
        &registry,
        31,
    );
    benchmark(
        "JSON parse only",
        "then parse json as document\n",
        &json,
        Default::default,
        &registry,
        31,
    );
    benchmark(
        "JSON pointer",
        "then parse json as document\njson.get with document document and pointer \"/meta/count\"\n",
        &json,
        Default::default,
        &registry,
        31,
    );
}
