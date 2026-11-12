#![cfg(windows)]
//! Issue #924: a quiet native command must not wait out a fixed supervision
//! sleep. Measured end to end through the public runtime API, after warm-up.
use std::{
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};

fn quiet_command_source() -> String {
    let cmd =
        std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_owned());
    format!(
        "run {} with arguments [\"/C\", \"exit\", \"0\"]\n",
        serde_json::to_string(&cmd).unwrap()
    )
}

#[test]
fn quiet_native_command_finishes_without_a_fixed_sleep() {
    let folder = tempfile::tempdir().unwrap();
    let program = flow::parser::parse(&quiet_command_source()).unwrap();
    let options = flow::runtime::RunOptions::new(folder.path().to_path_buf());
    let run_once = || {
        let started = Instant::now();
        flow::runtime::run_with_context(
            &program,
            &flow::manifest::Registry::default(),
            &options,
            Default::default(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("quiet native command runs");
        started.elapsed()
    };
    // Warm-up pays one-time loader and first-spawn costs outside the samples.
    run_once();
    let mut samples: Vec<Duration> = (0..7).map(|_| run_once()).collect();
    samples.sort();
    let median = samples[samples.len() / 2];
    assert!(
        median < Duration::from_millis(60),
        "quiet native command median {median:?} exceeds the 60 ms budget; samples {samples:?}"
    );
}
