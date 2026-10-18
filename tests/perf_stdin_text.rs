//! Subprocess benchmark for the stdin capture and text binding path.
//! Run with `cargo test --release --test perf_stdin_text -- --ignored --nocapture`.

use std::fs::File;
use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn benchmark(label: &str, script: &std::path::Path, input: &File) {
    let execute = std::env::var_os("EXECUTE_PERF_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_execute").into());
    let mut samples = Vec::with_capacity(200);
    for _ in 0..200 {
        let started = Instant::now();
        let status = Command::new(&execute)
            .arg(script)
            .stdin(Stdio::from(
                input.try_clone().expect("clone input file handle"),
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("start execute");
        assert!(status.success(), "{label} run failed: {status}");
        samples.push(started.elapsed());
    }
    eprintln!(
        "{label}: median {:?} over 200 process runs",
        median(samples)
    );
}

#[test]
#[ignore = "subprocess stdin performance harness"]
fn stdin_text_capture() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut text = String::with_capacity(1_000_000);
    while text.len() < 1_000_000 {
        text.push_str("the quick brown fox jumps over the lazy dog 0123456789\n");
    }
    text.truncate(1_000_000);
    let input_path = directory.path().join("input.txt");
    std::fs::write(&input_path, text)?;
    let input = File::open(input_path)?;
    let no_work = directory.path().join("no-work.program");
    let stdin_only = directory.path().join("stdin-only.program");
    std::fs::write(&no_work, "use value \"\"\n")?;
    std::fs::write(&stdin_only, "read from stdin as input\n")?;
    benchmark("no work", &no_work, &input);
    benchmark("read stdin as text", &stdin_only, &input);
    Ok(())
}
