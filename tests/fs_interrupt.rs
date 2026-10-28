//! Bounded cancellation and deadline behavior for filesystem applets.
//!
//! These tests run the real in-process applets and prove three properties:
//! entry checkpoints report an already-exceeded budget before mutating
//! anything, a lock held by another participant surfaces a bounded
//! `lock_timeout` instead of waiting forever, and a budget that expires while
//! waiting on that lock preempts the wait instead of serving the full lock
//! timeout. Upper-bound durations are generous so loaded CI machines do not
//! flake; the point is boundedness (seconds), not speed (milliseconds).

use flow::applets::{
    execute_in_cwd, execute_with_context, AppletErrorKind, ExecutionBudgetExceeded,
    ExecutionContext,
};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Generous upper bound: failure means the applet waited far past its budget.
const BOUND: Duration = Duration::from_secs(20);

/// Interruption target: an expired budget should surface within about
/// 100 ms. The bound is deliberately looser so loaded CI machines do not
/// flake on scheduling delays; the poll-count assertions below are what
/// prove the stop happened mid-operation rather than at its end.
const INTERRUPT_BOUND: Duration = Duration::from_secs(2);

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn read_hash(dir: &Path, path: &Path) -> String {
    let state = execute_in_cwd(
        "fs.read",
        &args(json!({"path": path})),
        &[],
        dir,
    )
    .unwrap();
    state["content_hash"].as_str().unwrap().to_owned()
}

fn transaction_plan(journal: &Path, target: &Path, before_hash: &str) -> Map<String, Value> {
    args(json!({
        "journal": journal,
        "operations": [{
            "path": target,
            "expected_content_hash": before_hash,
            "action": "write",
            "content": "after",
        }],
    }))
}

#[test]
fn transaction_apply_reports_cancellation_before_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("cancelled.txt");
    let journal = dir.path().join("journal");
    fs::write(&target, "before").unwrap();
    let plan = transaction_plan(&journal, &target, &read_hash(dir.path(), &target));

    let on_budget = || Err(ExecutionBudgetExceeded::Cancelled);
    let context = ExecutionContext::new(&on_budget);
    let error = execute_with_context("fs.transaction", &plan, &[], dir.path(), &context)
        .expect_err("an already-cancelled budget must stop the transaction");
    assert_eq!(error.kind, AppletErrorKind::Cancelled);
    assert_eq!(error.code, "cancelled");

    assert_eq!(fs::read_to_string(&target).unwrap(), "before");
    assert!(!journal.exists());
}

#[test]
fn transaction_apply_reports_deadline_before_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("expired.txt");
    let journal = dir.path().join("journal");
    fs::write(&target, "before").unwrap();
    let plan = transaction_plan(&journal, &target, &read_hash(dir.path(), &target));

    let on_budget = || Err(ExecutionBudgetExceeded::Timeout);
    let context = ExecutionContext::new(&on_budget);
    let error = execute_with_context("fs.transaction", &plan, &[], dir.path(), &context)
        .expect_err("an already-expired budget must stop the transaction");
    assert_eq!(error.kind, AppletErrorKind::Timeout);
    assert_eq!(error.code, "timeout");

    assert_eq!(fs::read_to_string(&target).unwrap(), "before");
    assert!(!journal.exists());
}

fn committed_journal(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let first = dir.join("first.conf");
    let second = dir.join("second.conf");
    let journal = dir.join("journal");
    fs::write(&first, "before one").unwrap();
    fs::write(&second, "before two").unwrap();
    let plan = args(json!({
        "journal": &journal,
        "operations": [
            {
                "path": &first,
                "expected_content_hash": read_hash(dir, &first),
                "action": "write",
                "content": "after one",
            },
            {
                "path": &second,
                "expected_content_hash": read_hash(dir, &second),
                "action": "write",
                "content": "after two",
            },
        ],
    }));
    let applied = execute_in_cwd("fs.transaction", &plan, &[], dir).unwrap();
    assert_eq!(applied["status"], "committed");
    (first, second, journal)
}

#[test]
fn transaction_recover_reports_cancellation_before_restoration() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second, journal) = committed_journal(dir.path());
    let rollback = args(json!({"journal": &journal, "decision": "rollback"}));

    let on_budget = || Err(ExecutionBudgetExceeded::Cancelled);
    let context = ExecutionContext::new(&on_budget);
    let error = execute_with_context(
        "fs.transaction.recover",
        &rollback,
        &[],
        dir.path(),
        &context,
    )
    .expect_err("an already-cancelled budget must stop recovery");
    assert_eq!(error.kind, AppletErrorKind::Cancelled);
    assert_eq!(error.code, "cancelled");

    assert_eq!(fs::read_to_string(&first).unwrap(), "after one");
    assert_eq!(fs::read_to_string(&second).unwrap(), "after two");
    assert!(journal.join("journal.json").is_file());
}

#[test]
fn transaction_recover_reports_deadline_before_restoration() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second, journal) = committed_journal(dir.path());
    let rollback = args(json!({"journal": &journal, "decision": "rollback"}));

    let on_budget = || Err(ExecutionBudgetExceeded::Timeout);
    let context = ExecutionContext::new(&on_budget);
    let error = execute_with_context(
        "fs.transaction.recover",
        &rollback,
        &[],
        dir.path(),
        &context,
    )
    .expect_err("an already-expired budget must stop recovery");
    assert_eq!(error.kind, AppletErrorKind::Timeout);
    assert_eq!(error.code, "timeout");

    assert_eq!(fs::read_to_string(&first).unwrap(), "after one");
    assert_eq!(fs::read_to_string(&second).unwrap(), "after two");
    assert!(journal.join("journal.json").is_file());
}

/// Hold the applet's advisory lock file with a separate descriptor so the
/// applet's own acquisition attempt blocks. Lock files live next to their
/// target as `.<name>.execute-applet.lock` for readable file names.
fn hold_target_lock(dir: &Path, file_name: &str) -> fs::File {
    let lock_path = dir.join(format!(".{file_name}.execute-applet.lock"));
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
fn contended_lock_fails_within_its_bounded_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("guarded.txt");
    fs::write(&target, "before").unwrap();
    let _held = hold_target_lock(dir.path(), "guarded.txt");

    let write = args(json!({
        "path": &target,
        "expected_content_hash": read_hash(dir.path(), &target),
        "symlink_policy": "reject",
        "permissions_policy": "preserve",
        "content": "after",
        "lock_timeout_ms": 500,
    }));
    let started = Instant::now();
    let error = execute_in_cwd("fs.write_atomic", &write, &[], dir.path())
        .expect_err("a held lock must surface lock_timeout");
    let elapsed = started.elapsed();

    assert_eq!(error.code, "lock_timeout");
    assert_eq!(error.kind, AppletErrorKind::Retryable);
    assert!(
        elapsed < BOUND,
        "lock wait outlived its bound: {elapsed:?}"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "before");
}

fn budget_that_expires(
    polls: &AtomicUsize,
    budget: ExecutionBudgetExceeded,
) -> impl Fn() -> Result<(), ExecutionBudgetExceeded> + '_ {
    move || {
        if polls.fetch_add(1, Ordering::Relaxed) + 1 >= 5 {
            Err(budget)
        } else {
            Ok(())
        }
    }
}

/// Fail when any abandoned temporary remains under `dir`, including inside
/// journal directories. Stable advisory lock files
/// (`.<name>.execute-applet.lock`) are intentionally retained and are not
/// temporaries.
fn assert_no_stray_temps(dir: &Path) {
    let mut stray = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                if name.ends_with(".tmp") || name.contains(".prepare.") {
                    stray.push(path);
                }
            }
        }
    }
    assert!(
        stray.is_empty(),
        "abandoned temporary files remain: {stray:?}"
    );
}

#[test]
fn contended_lock_wait_is_preempted_by_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("wait-cancel.txt");
    fs::write(&target, "before").unwrap();
    let _held = hold_target_lock(dir.path(), "wait-cancel.txt");

    let write = args(json!({
        "path": &target,
        "expected_content_hash": read_hash(dir.path(), &target),
        "symlink_policy": "reject",
        "permissions_policy": "preserve",
        "content": "after",
        "lock_timeout_ms": 120_000,
    }));
    let polls = AtomicUsize::new(0);
    let on_budget = budget_that_expires(&polls, ExecutionBudgetExceeded::Cancelled);
    let context = ExecutionContext::new(&on_budget);
    let started = Instant::now();
    let error = execute_with_context("fs.write_atomic", &write, &[], dir.path(), &context)
        .expect_err("cancellation must preempt the lock wait");
    let elapsed = started.elapsed();

    assert_eq!(error.kind, AppletErrorKind::Cancelled);
    assert_eq!(error.code, "cancelled");
    assert!(
        elapsed < BOUND,
        "cancelled lock wait outlived its bound: {elapsed:?}"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "before");
}

#[test]
fn contended_lock_wait_is_preempted_by_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("wait-deadline.txt");
    fs::write(&target, "before").unwrap();
    let _held = hold_target_lock(dir.path(), "wait-deadline.txt");

    let write = args(json!({
        "path": &target,
        "expected_content_hash": read_hash(dir.path(), &target),
        "symlink_policy": "reject",
        "permissions_policy": "preserve",
        "content": "after",
        "lock_timeout_ms": 120_000,
    }));
    let polls = AtomicUsize::new(0);
    let on_budget = budget_that_expires(&polls, ExecutionBudgetExceeded::Timeout);
    let context = ExecutionContext::new(&on_budget);
    let started = Instant::now();
    let error = execute_with_context("fs.write_atomic", &write, &[], dir.path(), &context)
        .expect_err("an expired deadline must preempt the lock wait");
    let elapsed = started.elapsed();

    assert_eq!(error.kind, AppletErrorKind::Timeout);
    assert_eq!(error.code, "timeout");
    assert!(
        elapsed < BOUND,
        "expired lock wait outlived its bound: {elapsed:?}"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "before");
}

/// A 4 MiB input crosses 64 hash chunks at 64 KiB each. Cancelling after a
/// few checkpoints must stop the hash mid-way instead of running it to the
/// end: with a 16 MiB gate (a check every 256 chunks) this input would hash
/// to completion and `expect_err` below would fail.
#[test]
fn hash_reports_cancellation_mid_hash_within_bound() {
    let dir = tempfile::tempdir().unwrap();
    let input = vec![b'a'; 4 * 1024 * 1024];

    let polls = AtomicUsize::new(0);
    let on_budget = || {
        if polls.fetch_add(1, Ordering::Relaxed) + 1 >= 8 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = ExecutionContext::new(&on_budget);
    let started = Instant::now();
    let error = execute_with_context("hash.sha256", &args(json!({})), &input, dir.path(), &context)
        .expect_err("cancellation must preempt a mid-hash run");
    let elapsed = started.elapsed();

    assert_eq!(error.kind, AppletErrorKind::Cancelled);
    assert_eq!(error.code, "cancelled");
    assert!(
        elapsed < INTERRUPT_BOUND,
        "mid-hash cancellation missed its 100 ms target: {elapsed:?}"
    );
    assert!(
        polls.load(Ordering::Relaxed) < 40,
        "hash ran past its midpoint instead of stopping: {} polls",
        polls.load(Ordering::Relaxed),
    );
}

#[test]
fn hash_reports_deadline_mid_hash_within_bound() {
    let dir = tempfile::tempdir().unwrap();
    let input = vec![b'b'; 4 * 1024 * 1024];

    let polls = AtomicUsize::new(0);
    let on_budget = || {
        if polls.fetch_add(1, Ordering::Relaxed) + 1 >= 8 {
            Err(ExecutionBudgetExceeded::Timeout)
        } else {
            Ok(())
        }
    };
    let context = ExecutionContext::new(&on_budget);
    let started = Instant::now();
    let error = execute_with_context("hash.sha256", &args(json!({})), &input, dir.path(), &context)
        .expect_err("an expired deadline must preempt a mid-hash run");
    let elapsed = started.elapsed();

    assert_eq!(error.kind, AppletErrorKind::Timeout);
    assert_eq!(error.code, "timeout");
    assert!(
        elapsed < INTERRUPT_BOUND,
        "mid-hash deadline missed its 100 ms target: {elapsed:?}"
    );
    assert!(
        polls.load(Ordering::Relaxed) < 40,
        "hash ran past its midpoint instead of stopping: {} polls",
        polls.load(Ordering::Relaxed),
    );
}

/// A 2 MiB replacement crosses 32 write chunks at 64 KiB each. Cancelling
/// mid-write must stop before the rename, leave the target untouched, and
/// remove the half-written temporary next to it.
#[test]
fn cancelled_write_leaves_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("big.txt");
    fs::write(&target, "before").unwrap();

    let write = args(json!({
        "path": &target,
        "expected_content_hash": read_hash(dir.path(), &target),
        "symlink_policy": "reject",
        "permissions_policy": "preserve",
        "content": "x".repeat(2 * 1024 * 1024),
        "lock_timeout_ms": 30_000,
    }));
    let polls = AtomicUsize::new(0);
    let on_budget = || {
        if polls.fetch_add(1, Ordering::Relaxed) + 1 >= 12 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = ExecutionContext::new(&on_budget);
    let started = Instant::now();
    let error = execute_with_context("fs.write_atomic", &write, &[], dir.path(), &context)
        .expect_err("cancellation must preempt a mid-write run");
    let elapsed = started.elapsed();

    assert_eq!(error.kind, AppletErrorKind::Cancelled);
    assert_eq!(error.code, "cancelled");
    assert!(
        elapsed < INTERRUPT_BOUND,
        "mid-write cancellation missed its 100 ms target: {elapsed:?}"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "before");
    assert_no_stray_temps(dir.path());
}

/// Cancelling mid-transaction must keep the retained journal recoverable and
/// leave no staging or journal temporaries behind. The budget expires late
/// enough that the run is past its entry checks, so reaching `expect_err`
/// (instead of a commit) proves the per-entry and per-chunk checkpoints
/// fired. When the journal was already published, an explicit rollback with
/// a fresh budget must restore every before image; when the budget expired
/// before publication, the staging directory is cleaned and no target was
/// touched.
#[test]
fn cancelled_transaction_stays_recoverable_and_leaves_no_temps() {
    let dir = tempfile::tempdir().unwrap();
    let mut before = Vec::new();
    let mut operations = Vec::new();
    for index in 0..4 {
        let file_name = format!("target-{index}.conf");
        let path = dir.path().join(&file_name);
        let old = format!("before-{index}-{}", "x".repeat(128 * 1024));
        let new = format!("after-{index}-{}", "y".repeat(128 * 1024));
        fs::write(&path, &old).unwrap();
        before.push((path.clone(), old));
        operations.push(json!({
            "path": path,
            "expected_content_hash": read_hash(dir.path(), &path),
            "action": "write",
            "content": new,
        }));
    }
    let plan = args(json!({
        "journal": dir.path().join("journal"),
        "operations": operations,
    }));

    let polls = AtomicUsize::new(0);
    let on_budget = || {
        if polls.fetch_add(1, Ordering::Relaxed) + 1 >= 25 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = ExecutionContext::new(&on_budget);
    let started = Instant::now();
    let error = execute_with_context("fs.transaction", &plan, &[], dir.path(), &context)
        .expect_err("cancellation must preempt a mid-transaction run");
    let elapsed = started.elapsed();

    assert_eq!(error.kind, AppletErrorKind::Cancelled);
    assert_eq!(error.code, "cancelled");
    assert!(
        elapsed < INTERRUPT_BOUND,
        "mid-transaction cancellation missed its 100 ms target: {elapsed:?}"
    );
    assert_no_stray_temps(dir.path());

    let journal = dir.path().join("journal");
    if journal.join("journal.json").is_file() {
        let rollback = args(json!({"journal": &journal, "decision": "rollback"}));
        let recovered =
            execute_in_cwd("fs.transaction.recover", &rollback, &[], dir.path())
                .expect("a retained journal must stay recoverable after cancellation");
        assert_eq!(recovered["status"], "rolled_back");
        for (path, old) in &before {
            assert_eq!(fs::read_to_string(path).unwrap(), *old);
        }
        assert_no_stray_temps(dir.path());
    } else {
        for (path, old) in &before {
            assert_eq!(fs::read_to_string(path).unwrap(), *old);
        }
    }
}
