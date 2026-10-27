use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;
thread_local! {
    static ALLOCATED: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

fn count(bytes: usize) {
    let _ = ALLOCATED.try_with(|counter| {
        if let Some((calls, total)) = counter.get() {
            counter.set(Some((calls + 1, total + bytes)));
        }
    });
}

// SAFETY: All allocations and deallocations use System with their original layouts.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: The caller supplies a valid allocation layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: The pointer and layout are forwarded unchanged to their allocator.
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(size);
        // SAFETY: The pointer, layout and requested size satisfy GlobalAlloc's contract.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

pub(super) fn measured<T>(operation: impl FnOnce() -> T) -> (T, usize, usize) {
    ALLOCATED.with(|counter| counter.set(Some((0, 0))));
    let result = operation();
    let (calls, bytes) = ALLOCATED.with(|counter| counter.replace(None).unwrap());
    (result, calls, bytes)
}

pub(super) fn journal() -> (tempfile::TempDir, CheckpointOptions, Journal) {
    let temp = tempfile::tempdir().unwrap();
    let options = CheckpointOptions {
        directory: temp.path().join("checkpoint"),
        program_path: temp.path().join("p.program"),
        resume: false,
        retry_uncertain: false,
    };
    fs::write(&options.program_path, "exit with status 0\n").unwrap();
    fs::create_dir(&options.directory).unwrap();
    let blobs_directory = options.directory.join("blobs");
    fs::create_dir(&blobs_directory).unwrap();
    let lock = File::create(options.directory.join("lock")).unwrap();
    #[cfg(windows)]
    crate::platform::windows::lock(&lock).unwrap();
    #[cfg(unix)]
    {
        // SAFETY: lock is a live descriptor; flock takes no pointers.
        let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(locked, 0);
    }
    let output = store_blob(&blobs_directory, b"", 1024).unwrap();
    // Exercise persistence independently of directory ACL changes, which the sandbox denies.
    let mut journal = Journal {
        directory: options.directory.clone(),
        blobs_directory,
        max_output_bytes: 1024 * 1024,
        retry_uncertain: false,
        resuming: false,
        persistence_failed: false,
        preloaded_state: None,
        data: JournalData {
            version: JOURNAL_VERSION,
            identity: Identity {
                input_signature: None,
                keyed_inputs: false,
                program_path: EncodedPath::from_path(&options.program_path),
                initial_cwd: EncodedPath::from_path(temp.path()),
                mode: "finite".to_owned(),
            },
            status: Status::Running,
            completed: Vec::new(),
            in_flight: None,
            snapshot: SnapshotRef {
                cwd: EncodedPath::from_path(temp.path()),
                output,
                bindings: BTreeMap::new(),
            },
            error: None,
            listener_cursor: None,
        },
        completed_chain: chain_start(),
        completed_log_bytes: 0,
        elements_chain: String::new(),
        elements_bytes: 0,
        elements_lines: 0,
        elements_file: None,
        elements_dirty: false,
        elements_synced_at: std::time::Instant::now(),
        element_state_cache: None,
        element_replay: None,
        element_buffer: Vec::new(),
        _lock: lock,
    };
    journal.persist().unwrap();
    (temp, options, journal)
}

fn legacy_persist(journal: &Journal) {
    let body = JournalDisk {
        version: JOURNAL_VERSION,
        identity: journal.data.identity.clone(),
        status: journal.data.status,
        completed_count: journal.data.completed.len(),
        completed_chain: journal.completed_chain.clone(),
        in_flight: journal.data.in_flight.clone(),
        snapshot: journal.data.snapshot.clone(),
        error: journal.data.error.clone(),
        listener_cursor: journal.data.listener_cursor.clone(),
    };
    let data = serde_json::to_vec(&body).unwrap();
    let envelope = encode_envelope(&hex_digest(&data), &data);
    atomic_write(
        &journal.directory,
        "journal",
        "json",
        &envelope,
        "checkpoint.lane_v3_test.write",
    )
    .unwrap();
}

#[test]
#[ignore = "explicit checkpoint journal allocation and elapsed-time microbenchmark"]
fn journal_memory_microbenchmark() {
    for bindings in [8, 1000, 8000] {
        let (_temp, _options, mut journal) = journal();
        for index in 0..bindings {
            journal.data.snapshot.bindings.insert(
                format!("binding{index:05}"),
                journal.data.snapshot.output.clone(),
            );
        }
        for optimized in [false, true] {
            let started = std::time::Instant::now();
            let (_, calls, bytes) = measured(|| {
                for _ in 0..10 {
                    if optimized {
                        journal.persist().unwrap();
                    } else {
                        legacy_persist(&journal);
                    }
                }
            });
            eprintln!("journal bindings={bindings} optimized={optimized}: {:?}, {calls} allocations, {bytes} bytes", started.elapsed());
        }
    }
}

#[test]
fn finite_finish_collects_superseded_blobs() {
    let (_temp, options, mut journal) = journal();
    let orphan = store_blob(&journal.blobs_directory, b"superseded", 1024).unwrap();
    journal.finish().unwrap();
    assert!(!journal.blobs_directory.join(orphan.sha256).exists());
    assert!(options.directory.join("journal.json").is_file());
}

#[test]
fn checkpoint_refuses_sensitive_inputs_before_persisting() {
    for source in ["environment \"API_KEY\"", "secret prompt \"Key?\""] {
        let text = format!("accept key as text from {source}\nexit with status 0\n");
        let program = crate::parser::parse(&text).unwrap();
        let result = crate::runtime::resume::validate_checkpoint_controls(
            &program,
            &crate::manifest::Registry::default(),
        );
        assert!(result.is_err(), "sensitive checkpoint accepted: {source}");
    }
}

#[test]
fn checkpoint_refuses_sensitive_inputs_in_reusable_definitions() {
    let mut program = crate::parser::parse("define a program named worker\n  accept key as text\n  use value key\nrun program worker with key \"value\"\n").unwrap();
    // Programmatic ASTs must obey the same persistence gate as parsed programs.
    std::sync::Arc::make_mut(&mut program.definitions)
        .get_mut("worker")
        .unwrap()
        .inputs[0]
        .source = Some(crate::ast::InputSource::Environment("API_KEY".to_owned()));
    assert!(crate::runtime::resume::validate_checkpoint_controls(
        &program,
        &crate::manifest::Registry::default()
    )
    .is_err());
}

#[test]
fn plain_run_still_accepts_environment_inputs() {
    let program = crate::parser::parse(
        "accept key as text from environment \"API_KEY\"\nexit with status 0\n",
    )
    .unwrap();
    crate::runtime::validate(&program, &crate::manifest::Registry::default()).unwrap();
}

#[test]
fn envelope_limit_failure_preserves_durable_journal() {
    let (_temp, options, mut journal) = journal();
    let before = fs::read(options.directory.join("journal.json")).unwrap();
    journal.data.error = Some("x".repeat(16 * 1024 * 1024));
    assert!(journal
        .persist()
        .unwrap_err()
        .to_string()
        .contains("16 MiB"));
    assert_eq!(
        fs::read(options.directory.join("journal.json")).unwrap(),
        before
    );
    assert!(!fs::read_dir(options.directory).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}

#[test]
fn binding_serialization_reuses_and_erases_scratch() {
    let (_temp, _options, mut journal) = journal();
    let value = Value::String("x".repeat(128 * 1024));
    let next_value = Value::String("y".repeat(128 * 1024));
    let mut buffer = Vec::new();
    // Changed bindings exercise the serialization used by successive commits.
    let first =
        store_value_blob(&journal.blobs_directory, &value, 1024 * 1024, &mut buffer).unwrap();
    let (result, _, bytes) = measured(|| {
        store_value_blob(
            &journal.blobs_directory,
            &next_value,
            1024 * 1024,
            &mut buffer,
        )
    });
    let next = result.unwrap();
    assert_eq!(next.length, first.length);
    assert_ne!(next.sha256, first.sha256);
    assert!(bytes < 64 * 1024, "allocated {bytes} bytes");
    assert!(buffer.is_empty());
    journal.finish().unwrap();
}

#[test]
fn persist_borrows_snapshot_and_streams_envelope() {
    let (_temp, options, mut journal) = journal();
    for index in 0..8000 {
        journal.data.snapshot.bindings.insert(
            format!("binding{index:05}"),
            journal.data.snapshot.output.clone(),
        );
    }
    let (result, calls, bytes) = measured(|| journal.persist());
    result.unwrap();
    eprintln!("journal persist: {calls} allocations, {bytes} allocated bytes");
    assert!(bytes < 128 * 1024, "persist allocated {bytes} bytes");
    let raw = fs::read(options.directory.join("journal.json")).unwrap();
    let envelope: JournalEnvelope = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        envelope.checksum,
        hex_digest(envelope.data.get().as_bytes())
    );
    let body: JournalDisk = serde_json::from_str(envelope.data.get()).unwrap();
    assert_eq!(
        raw,
        encode_envelope(&envelope.checksum, &serde_json::to_vec(&body).unwrap())
    );
}

#[test]
fn collection_refuses_unreadable_or_malformed_reference_logs() {
    let (_temp, _options, mut journal) = journal();
    journal.data.identity.mode = "service:test".to_owned();
    let orphan = store_blob(&journal.blobs_directory, b"keep-if-scan-fails", 1024).unwrap();
    fs::write(journal.directory.join("elements.jsonl"), b"malformed\n").unwrap();
    journal.finish().unwrap();
    assert!(journal.blobs_directory.join(orphan.sha256).exists());
}

#[test]
fn collection_keeps_blob_refs_in_completed_and_element_logs() {
    for name in ["completed.jsonl", "elements.jsonl"] {
        let (_temp, _options, mut journal) = journal();
        journal.data.identity.mode = "service:test".to_owned();
        let reference =
            store_blob(&journal.blobs_directory, b"historical-reference", 1024).unwrap();
        let line =
            serde_json::to_string(&serde_json::json!({"state": {"binding": reference}})).unwrap();
        fs::write(journal.directory.join(name), format!("{line}\n")).unwrap();
        journal.finish().unwrap();
        assert!(
            journal.blobs_directory.join(reference.sha256).exists(),
            "lost reference from {name}"
        );
    }
}

#[cfg(feature = "agent-advisory")]
#[test]
fn status_detects_held_lock_and_release() {
    let (_temp, options, journal) = journal();
    let status = crate::agent::checkpoint::summarize(&options.directory).unwrap();
    assert_eq!(status["resume_action"], "locked");
    drop(journal);
    let status = crate::agent::checkpoint::summarize(&options.directory).unwrap();
    assert_ne!(status["resume_action"], "locked");
    assert_eq!(status["lock_held"], false);
}

#[cfg(feature = "agent-advisory")]
#[test]
fn graph_status_detects_held_lock() {
    let (_temp, options, journal) = journal();
    fs::write(options.directory.join("journal.json"), br#"{"checksum":"unused","data":{"kind":"graph","version":5,"completed_count":0,"in_flight":[]}}"#).unwrap();
    let summary = crate::agent::checkpoint::summarize(&options.directory).unwrap();
    assert_eq!(summary["lock_held"], true);
    assert_eq!(summary["resume_action"], "locked");
    drop(journal);
    let summary = crate::agent::checkpoint::summarize(&options.directory).unwrap();
    assert_eq!(summary["lock_held"], false);
}

#[cfg(feature = "agent-advisory")]
#[test]
fn lock_probe_never_creates_files_and_refuses_nonregular_locks() {
    let directory = tempfile::tempdir().unwrap();
    assert!(!directory_lock_held(directory.path()).unwrap());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    fs::create_dir(directory.path().join("lock")).unwrap();
    assert!(directory_lock_held(directory.path()).is_err());
}
