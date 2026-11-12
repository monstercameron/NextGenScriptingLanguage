//! Named locks for programs (#348): try, wait with a timeout, probe, and release on drop.
//! These run on Windows and Unix; the locks are std file locks, which use flock on Unix and
//! LockFileEx on Windows.

use flow::agent::instance::{
    acquire_named_lock, named_lock_held, try_named_lock, validate_lock_name, LockMode,
    NamedLockWait,
};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

fn lock_dir(root: &Path) -> std::path::PathBuf {
    root.join("locks")
}

#[test]
fn try_takes_the_lock_and_dropping_the_handle_releases_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());

    let held = try_named_lock(&dir, "theme-set", LockMode::Exclusive)
        .expect("try")
        .expect("a free lock is acquired");
    assert_eq!(held.name(), "theme-set");
    assert_eq!(held.mode(), LockMode::Exclusive);
    assert!(named_lock_held(&dir, "theme-set").expect("probe"));

    assert!(
        try_named_lock(&dir, "theme-set", LockMode::Exclusive)
            .expect("second try")
            .is_none(),
        "a second try must see the lock busy"
    );

    drop(held);
    assert!(!named_lock_held(&dir, "theme-set").expect("probe after drop"));
    assert!(try_named_lock(&dir, "theme-set", LockMode::Exclusive)
        .expect("try after drop")
        .is_some());
}

#[test]
fn a_stale_lock_file_with_no_holder_never_blocks() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());
    fs::create_dir_all(&dir).expect("create dir");
    fs::write(dir.join("stale.lock"), b"left by a crashed run").expect("write stale file");

    assert!(!named_lock_held(&dir, "stale").expect("probe stale"));
    assert!(try_named_lock(&dir, "stale", LockMode::Exclusive)
        .expect("try stale")
        .is_some());
}

#[test]
fn probe_never_creates_a_missing_lock_file() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());

    assert!(!named_lock_held(&dir, "absent").expect("probe absent"));
    assert!(!dir.join("absent.lock").exists());
}

#[test]
fn shared_holders_coexist_and_block_an_exclusive_try() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());

    let first = try_named_lock(&dir, "reader", LockMode::Shared)
        .expect("first shared")
        .expect("first shared acquired");
    let second = try_named_lock(&dir, "reader", LockMode::Shared)
        .expect("second shared")
        .expect("second shared acquired alongside the first");
    assert!(
        try_named_lock(&dir, "reader", LockMode::Exclusive)
            .expect("exclusive try")
            .is_none(),
        "an exclusive try must not succeed while shared holders exist"
    );
    assert!(named_lock_held(&dir, "reader").expect("probe shared"));

    drop(first);
    drop(second);
    assert!(try_named_lock(&dir, "reader", LockMode::Exclusive)
        .expect("exclusive after shared")
        .is_some());
}

#[test]
fn waiting_acquire_with_zero_wait_makes_one_attempt() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());
    let _held = try_named_lock(&dir, "busy", LockMode::Exclusive)
        .expect("try")
        .expect("acquired");

    match acquire_named_lock(&dir, "busy", LockMode::Exclusive, Duration::ZERO).expect("acquire") {
        NamedLockWait::TimedOut { .. } => {}
        NamedLockWait::Acquired { .. } => panic!("a busy lock must not be acquired"),
    }
}

#[test]
fn waiting_acquire_times_out_on_a_busy_lock() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());
    let _held = try_named_lock(&dir, "busy", LockMode::Exclusive)
        .expect("try")
        .expect("acquired");

    let wait = Duration::from_millis(120);
    match acquire_named_lock(&dir, "busy", LockMode::Exclusive, wait).expect("acquire") {
        NamedLockWait::TimedOut { waited } => assert!(waited >= wait),
        NamedLockWait::Acquired { .. } => panic!("a busy lock must time out"),
    }
}

#[test]
fn waiting_acquire_takes_the_lock_once_the_holder_releases_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());
    let held = try_named_lock(&dir, "handoff", LockMode::Exclusive)
        .expect("try")
        .expect("acquired");

    let releaser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(80));
        drop(held);
    });
    match acquire_named_lock(
        &dir,
        "handoff",
        LockMode::Exclusive,
        Duration::from_secs(30),
    )
    .expect("acquire")
    {
        NamedLockWait::Acquired { lock, waited } => {
            assert!(waited >= Duration::from_millis(60));
            assert_eq!(lock.name(), "handoff");
        }
        NamedLockWait::TimedOut { .. } => panic!("the lock was released well inside the wait"),
    }
    releaser.join().expect("releaser thread");
}

#[test]
fn twenty_parallel_tries_let_only_one_holder_run_at_a_time() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = Arc::new(lock_dir(root.path()));
    let barrier = Arc::new(Barrier::new(20));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let winners = Arc::new(AtomicUsize::new(0));

    let workers: Vec<_> = (0..20)
        .map(|_| {
            let (dir, barrier, active, peak, winners) = (
                Arc::clone(&dir),
                Arc::clone(&barrier),
                Arc::clone(&active),
                Arc::clone(&peak),
                Arc::clone(&winners),
            );
            thread::spawn(move || {
                barrier.wait();
                if let Some(lock) =
                    try_named_lock(&dir, "debounce", LockMode::Exclusive).expect("try")
                {
                    winners.fetch_add(1, Ordering::SeqCst);
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(30));
                    active.fetch_sub(1, Ordering::SeqCst);
                    drop(lock);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker thread");
    }

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "never more than one holder at once"
    );
    assert!(winners.load(Ordering::SeqCst) >= 1, "at least one try wins");
}

#[test]
fn twenty_parallel_waiting_acquires_serialize_every_critical_section() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = Arc::new(lock_dir(root.path()));
    let barrier = Arc::new(Barrier::new(20));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let workers: Vec<_> = (0..20)
        .map(|_| {
            let (dir, barrier, active, peak, completed) = (
                Arc::clone(&dir),
                Arc::clone(&barrier),
                Arc::clone(&active),
                Arc::clone(&peak),
                Arc::clone(&completed),
            );
            thread::spawn(move || {
                barrier.wait();
                match acquire_named_lock(
                    &dir,
                    "reload",
                    LockMode::Exclusive,
                    Duration::from_secs(60),
                )
                .expect("acquire")
                {
                    NamedLockWait::Acquired { lock, .. } => {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(5));
                        active.fetch_sub(1, Ordering::SeqCst);
                        completed.fetch_add(1, Ordering::SeqCst);
                        drop(lock);
                    }
                    NamedLockWait::TimedOut { .. } => panic!("a 60 second wait must not time out"),
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker thread");
    }

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "critical sections never overlap"
    );
    assert_eq!(
        completed.load(Ordering::SeqCst),
        20,
        "every waiter runs its section"
    );
}

#[test]
fn invalid_names_are_refused_and_create_no_file() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());

    // 251 bytes: one more than the longest name whose lock file name fits in 255 bytes.
    let too_long = "x".repeat(251);
    let refused = [
        "",
        "../escape",
        "a/b",
        "a\\b",
        "..",
        ".hidden",
        "trailing.",
        "has space",
        "semi;colon",
        "non\u{e9}ascii",
        "CON",
        "nul.txt",
        "lpt1",
        too_long.as_str(),
    ];
    for name in refused {
        assert!(
            validate_lock_name(name).is_err(),
            "name {name:?} must be refused"
        );
        assert!(
            try_named_lock(&dir, name, LockMode::Exclusive).is_err(),
            "try must refuse {name:?}"
        );
    }
    assert!(!root.path().join("escape.lock").exists());
    assert!(named_lock_held(&dir, "../escape").is_err());
}

#[test]
fn accepted_names_are_single_components_inside_the_directory() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());

    let longest = "n".repeat(250);
    for name in ["theme-set", "x.lock", "a_b-9", "0", longest.as_str()] {
        validate_lock_name(name).expect("valid name");
        let lock = try_named_lock(&dir, name, LockMode::Exclusive)
            .expect("try")
            .expect("free lock");
        drop(lock);
        let created = fs::read_dir(&dir)
            .expect("read lock dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .any(|file| file.to_string_lossy() == format!("{name}.lock"));
        assert!(
            created,
            "{name} must create {name}.lock inside the directory"
        );
    }
}

#[test]
fn a_spawned_child_does_not_keep_the_lock_after_the_parent_releases_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = lock_dir(root.path());
    let held = try_named_lock(&dir, "spawn", LockMode::Exclusive)
        .expect("try")
        .expect("acquired");

    #[cfg(unix)]
    let mut child = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");
    #[cfg(windows)]
    let mut child = Command::new("cmd")
        .args(["/C", "ping", "-n", "31", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");

    drop(held);
    let probe = named_lock_held(&dir, "spawn");
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        !probe.expect("probe after child spawned"),
        "the child must not hold the lock"
    );
}
