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
    benchmark(
        "JSON map small selection",
        "then parse json as document\nrun json.map with document document and pointer \"/a/b\" and select \"/name\" as selected\n",
        &json,
        Default::default,
        &registry,
        11,
    );
    benchmark(
        "JSON map large selection",
        "then parse json as document\nrun json.map with document document and pointer \"/a/b\" and select \"\" as selected\n",
        &json,
        Default::default,
        &registry,
        11,
    );
    benchmark(
        "JSON filter small selection",
        "then parse json as document\nrun json.filter with document document and pointer \"/a/b\" and where [{\"pointer\":\"/id\",\"equals\":17}] as selected\n",
        &json,
        Default::default,
        &registry,
        11,
    );
    benchmark(
        "JSON filter large selection",
        "then parse json as document\nrun json.filter with document document and pointer \"/a/b\" and where [{\"exists\":true}] as selected\n",
        &json,
        Default::default,
        &registry,
        11,
    );
    benchmark(
        "JSON runtime encode",
        "then parse json as document\nthen encode document as json\n",
        &json,
        Default::default,
        &registry,
        11,
    );
    benchmark(
        "JSON stringify applet",
        "then parse json as document\nrun json.stringify with value document as encoded\n",
        &json,
        Default::default,
        &registry,
        11,
    );
}

/// These components overlap; they are diagnostics, not an additive cost model.
/// In particular, the runtime also clones bindings and validates applet contracts.
#[test]
#[ignore = "in-process text cost breakdown"]
fn text_upper_cost_breakdown() {
    fn measure<T>(label: &str, mut operation: impl FnMut() -> T) {
        let mut samples = Vec::with_capacity(101);
        for _ in 0..101 {
            let started = Instant::now();
            black_box(operation());
            samples.push(started.elapsed());
        }
        eprintln!("text component {label}: median {:?}", median(samples));
    }

    let line = "the quick brown fox jumps over the lazy dog 0123456789\n";
    let mut text = line.repeat(1_000_000 / line.len() + 1);
    text.truncate(1_000_000);
    let args =
        serde_json::Map::from_iter([("text".into(), serde_json::Value::String(text.clone()))]);
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    measure("allocate + copy input", || black_box(&text).clone());
    measure("ASCII scan", || black_box(text.as_bytes()).is_ascii());
    measure("UTF-8 validation", || {
        std::str::from_utf8(black_box(text.as_bytes())).unwrap()
    });
    let mut output = Vec::with_capacity(text.len());
    measure("scalar conversion into reused allocation", || {
        output.clear();
        output.extend(
            black_box(text.as_bytes())
                .iter()
                .map(u8::to_ascii_uppercase),
        );
        black_box(&output);
    });
    measure("standard Unicode uppercase + allocation", || {
        black_box(&text).to_uppercase()
    });
    measure("applet with borrowed arguments", || {
        flow::applets::execute_in_cwd("text.upper", black_box(&args), &[], &cwd).unwrap()
    });
    measure("allocate + copy output bytes", || {
        black_box(text.as_bytes()).to_vec()
    });
}

#[test]
#[ignore = "relative performance gate; run on an otherwise idle machine"]
fn text_upper_does_not_regress_standard_mapping() {
    let text = "the quick brown fox 0123456789\n".repeat(1_000_000 / 30 + 1);
    let text = &text[..1_000_000];
    let args =
        serde_json::Map::from_iter([("text".into(), serde_json::Value::String(text.to_owned()))]);
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut standard = Vec::with_capacity(101);
    let mut applet = Vec::with_capacity(101);
    for _ in 0..101 {
        let started = Instant::now();
        black_box(black_box(text).to_uppercase());
        standard.push(started.elapsed());
        let started = Instant::now();
        black_box(
            flow::applets::execute_in_cwd("text.upper", black_box(&args), &[], &cwd).unwrap(),
        );
        applet.push(started.elapsed());
    }
    let standard = median(standard);
    let applet = median(applet);
    eprintln!("text upper gate: standard {standard:?}, applet {applet:?}");
    assert!(applet <= standard.mul_f64(1.10) + Duration::from_micros(1));
}

#[test]
#[ignore = "1 MiB explicit execution-result allocation benchmark; run in isolation"]
fn lane_v2_result_binding_one_mib() {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let registry = Registry::from_bundled(&cwd.join("manifests")).unwrap();
    let program =
        parser::parse("run text.upper with text text and keep result as execution\n").unwrap();
    let input = "a".repeat(1024 * 1024);
    let expected = vec![b'A'; input.len()];
    let expected_serialized = serde_json::to_vec(&expected).unwrap();
    let options = RunOptions::new(cwd);
    let mut samples = Vec::new();
    let mut peak_extra = 0;
    for _ in 0..7 {
        let bindings = std::collections::BTreeMap::from([(
            "text".into(),
            flow::ast::Value::String(input.clone()),
        )]);
        let baseline = LIVE_BYTES.load(Ordering::Relaxed);
        PEAK_BYTES.store(baseline, Ordering::Relaxed);
        let start = Instant::now();
        let result = runtime::run_with_context(
            &program,
            &registry,
            &options,
            bindings,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        samples.push(start.elapsed());
        peak_extra = peak_extra.max(PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(baseline));
        assert_eq!(result.output, expected);
        black_box(result);
    }
    // Validate the source-visible record through the language's public JSON
    // boundary, outside the allocation/timing sample above.
    let encoded_program = parser::parse("run text.upper with text text and keep result as execution\nthen encode execution as json\n").unwrap();
    let mut encoded_options = options.clone();
    encoded_options.max_output_bytes = 8 * 1024 * 1024;
    let encoded = runtime::run_with_context(
        &encoded_program,
        &registry,
        &encoded_options,
        std::collections::BTreeMap::from([("text".into(), flow::ast::Value::String(input))]),
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&encoded.output).unwrap();
    assert_eq!(record["stdout_captured"], true);
    assert_eq!(
        serde_json::to_vec(&record["stdout"]).unwrap(),
        expected_serialized
    );
    eprintln!(
        "1 MiB result binding: median {:?}; peak additional heap {peak_extra} bytes",
        median(samples)
    );
}
