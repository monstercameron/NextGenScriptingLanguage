//! Replays the fuzz seed corpus and every saved crash through the fuzz entry points.
//!
//! A crash the fuzzer finds is committed under `fuzz/regressions/<target>/`; this test
//! replays it through `flow::fuzz_entry::run` and fails, naming the file, if it panics
//! again. The seeds under `fuzz/corpus/<target>/` are replayed too. Requires the
//! `fuzzing` feature: `cargo test --locked --features fuzzing --test fuzz_regressions`.
#![cfg(feature = "fuzzing")]

use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

/// Run every input file in `directory` through `target`, and count them.
fn replay(directory: &Path, target: &str) -> usize {
    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };
    let mut replayed = 0;
    for entry in entries {
        let path = entry.expect("read a fuzz input directory entry").path();
        let hidden = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'));
        if !path.is_file() || hidden {
            continue;
        }
        let bytes = fs::read(&path).expect("read a fuzz input");
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            flow::fuzz_entry::run(target, &bytes);
        }));
        assert!(
            outcome.is_ok(),
            "fuzz target {target} panicked on {}",
            path.display()
        );
        replayed += 1;
    }
    replayed
}

#[test]
fn saved_crashes_and_seeds_do_not_panic() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz");
    let mut replayed = 0;
    for target in flow::fuzz_entry::TARGETS {
        replayed += replay(&root.join("regressions").join(target), target);
        replayed += replay(&root.join("corpus").join(target), target);
    }
    assert!(
        replayed > 0,
        "no fuzz inputs found under {}",
        root.display()
    );
}
