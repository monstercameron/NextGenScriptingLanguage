//! Cancellation latency for in-process file lock waits and hashing (#552).
//!
//! The acceptance bar is that a cancelled applet returns within 100 ms of the
//! cancellation itself, not within the lock timeout (up to 300 s) or after
//! hashing a whole large input. These tests record the instant the budget
//! reports cancellation and measure how long the applet takes to return after
//! that instant. The bound is the 100 ms target from the issue.

use flow::applets::{
    execute_with_context, AppletErrorKind, ExecutionBudgetExceeded, ExecutionContext,
};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Latency target from #552: a cancelled operation returns within 100 ms.
const LATENCY_TARGET: Duration = Duration::from_millis(100);

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// Hold the applet's advisory lock file through a separate descriptor so the
/// applet's own acquisition attempt keeps waiting.
fn hold_target_lock(dir: &Path, file_name: &str) -> fs::File {
    let lock_path: PathBuf = dir.join(format!(".{file_name}.execute-applet.lock"));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    lock.try_lock().unwrap();
    lock
}

#[test]
fn lock_wait_returns_within_100ms_of_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("latency-cancel.txt");
    fs::write(&target, "before").unwrap();
    let _held = hold_target_lock(dir.path(), "latency-cancel.txt");

    let hash =
        flow::applets::execute_in_cwd("fs.read", &args(json!({"path": &target})), &[], dir.path())
            .unwrap()["content_hash"]
            .as_str()
            .unwrap()
            .to_owned();
    let write = args(json!({
        "path": &target,
        "expected_content_hash": hash,
        "symlink_policy": "reject",
        "permissions_policy": "preserve",
        "content": "after",
        "lock_timeout_ms": 300_000,
    }));

    // Cancel only once the applet has been waiting on the held lock for a
    // while, so the cancellation lands inside the lock-wait loop and not at
    // an earlier checkpoint.
    let started = Instant::now();
    let cancelled_at: Mutex<Option<Instant>> = Mutex::new(None);
    let on_budget = || {
        if started.elapsed() >= Duration::from_millis(30) {
            let mut slot = cancelled_at.lock().unwrap();
            slot.get_or_insert_with(Instant::now);
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = ExecutionContext::new(&on_budget);
    let error = execute_with_context("fs.write_atomic", &write, &[], dir.path(), &context)
        .expect_err("cancellation must preempt the lock wait");
    let returned_at = Instant::now();

    let cancel_instant = cancelled_at
        .lock()
        .unwrap()
        .expect("the budget must have reported cancellation");
    let latency = returned_at.duration_since(cancel_instant);

    assert_eq!(error.kind, AppletErrorKind::Cancelled);
    assert_eq!(error.code, "cancelled");
    assert!(
        latency < LATENCY_TARGET,
        "lock wait took {latency:?} to return after cancellation (target {LATENCY_TARGET:?})"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "before");
}

#[test]
fn hash_returns_within_100ms_of_cancellation_on_large_input() {
    let dir = tempfile::tempdir().unwrap();
    // 64 MiB stands in for the 2 GB case: a run that ignored cancellation
    // would hash all of it before returning.
    let input = vec![b'c'; 64 * 1024 * 1024];

    let polls = AtomicUsize::new(0);
    let cancelled_at: Mutex<Option<Instant>> = Mutex::new(None);
    let on_budget = || {
        if polls.fetch_add(1, Ordering::Relaxed) + 1 >= 3 {
            let mut slot = cancelled_at.lock().unwrap();
            slot.get_or_insert_with(Instant::now);
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = ExecutionContext::new(&on_budget);
    let error = execute_with_context(
        "hash.sha256",
        &args(json!({})),
        &input,
        dir.path(),
        &context,
    )
    .expect_err("cancellation must stop a large hash");
    let returned_at = Instant::now();

    let cancel_instant = cancelled_at
        .lock()
        .unwrap()
        .expect("the budget must have reported cancellation");
    let latency = returned_at.duration_since(cancel_instant);

    assert_eq!(error.kind, AppletErrorKind::Cancelled);
    assert!(
        latency < LATENCY_TARGET,
        "hash took {latency:?} to return after cancellation (target {LATENCY_TARGET:?})"
    );
    assert!(
        polls.load(Ordering::Relaxed) < 40,
        "hash ran past its midpoint instead of stopping: {} polls",
        polls.load(Ordering::Relaxed),
    );
}
