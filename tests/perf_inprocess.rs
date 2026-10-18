use flow::manifest::Registry;
use flow::parser;
use flow::runtime::{self, RunOptions};
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    for _ in 0..iterations {
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
    }
    eprintln!(
        "{label}: median {:?} over {iterations} runs",
        median(samples)
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
