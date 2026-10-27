#![cfg(windows)]
//! Native PE fixture coverage for the persistent worker backend.

#[path = "../src/platform/windows.rs"]
#[allow(dead_code)]
pub(crate) mod windows_platform;

#[path = "../src/runtime/worker_backend.rs"]
#[allow(dead_code)]
mod worker_backend;

mod platform {
    pub(crate) use crate::windows_platform as windows;
}

/// The included platform module records the cancelling signal through its
/// parent; this harness does not report interrupt exit codes.
pub(crate) fn record_cancel_signal(_signal: i32) {}

/// Drain-state helpers mirror `src/platform.rs`, which the included module reaches
/// through its parent.
pub(crate) const DRAIN_CANCELLED: u8 = 3;

pub(crate) fn request_drain(state: &std::sync::atomic::AtomicU8) -> bool {
    state
        .compare_exchange(
            0,
            1,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

mod capabilities {
    pub use flow::capabilities::*;
}

mod manifest {
    pub use flow::manifest::*;
}

mod security {
    pub use flow::security::*;
}

mod working_dir {
    pub use flow::working_dir::*;
}

use flow::contracts::Sensitivity;
use flow::{parser, runtime};
use serde_json::json;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
use worker_backend::{WorkerLaunch, WorkerPool, WorkerRequest};

const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_STDERR: usize = 1024;

fn fixture() -> PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_native-fixture"))
}

fn registry() -> manifest::Registry {
    let source = json!({
        "name": "worker.fixture",
        "binary": fixture(),
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "json_lines_worker",
            "lifecycle": {"kind": "json_lines"}
        }
    });
    manifest::Registry::from_sources(&[source.to_string()]).expect("valid worker manifest")
}

fn launch(directory: &TempDir, mode: &str) -> WorkerLaunch {
    let marker = directory.path().join("launches");
    let effects = directory.path().join("effects");
    WorkerLaunch::inherited(
        registry(),
        "worker.fixture".to_owned(),
        fixture(),
        directory.path().to_path_buf(),
        vec![
            OsString::from("json-lines"),
            OsString::from(mode),
            marker.into_os_string(),
            effects.into_os_string(),
        ],
        worker_backend::WorkerLimits {
            protocol_version: 1,
            timeout: Duration::from_secs(5),
            max_frame_bytes: MAX_FRAME,
            max_stderr_bytes: MAX_STDERR,
        },
    )
    .expect("valid worker launch")
}

fn request(input: &[u8]) -> WorkerRequest {
    WorkerRequest {
        arguments: Vec::new(),
        input: input.to_vec(),
        value: None,
        json_output: false,
        max_output_bytes: MAX_FRAME,
        sensitivity: security::SensitivitySet::from_label(Sensitivity::Unknown),
        deadline: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        spawned: Arc::new(AtomicBool::new(false)),
    }
}

fn marker_pid(directory: &TempDir) -> u32 {
    std::fs::read_to_string(directory.path().join("launches"))
        .expect("worker launch marker")
        .lines()
        .next()
        .expect("worker pid line")
        .parse()
        .expect("numeric process id")
}

fn assert_reaped(pid: u32) {
    // SAFETY: OpenProcess reads only scalar arguments here (access mask, inherit flag 0,
    // and `pid` parsed from the launch marker); it takes no pointers. A NULL return means
    // failure and is handled by the is_null check at line 152 before the handle is used.
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if process.is_null() {
        // SAFETY: GetLastError returns the calling thread's last-error value, which is
        // still the one set by the failed OpenProcess at line 145: the only code run in
        // between is the pure-Rust `is_null` check, with no Win32 calls.
        let error = unsafe { GetLastError() };
        assert_eq!(
            error, ERROR_INVALID_PARAMETER,
            "opening worker process {pid} failed unexpectedly with Windows error {error}"
        );
        return;
    }
    // SAFETY: `process` was checked non-null at line 152, so it is a valid handle that
    // this function owns. WaitForSingleObject with timeout 0 only polls it, and the
    // handle stays open until CloseHandle at line 170.
    let state = unsafe { WaitForSingleObject(process, 0) };
    // SAFETY: `process` is the non-null handle from OpenProcess (line 145) that has not
    // been closed yet; this is its only CloseHandle, so the handle is closed exactly once.
    unsafe {
        CloseHandle(process);
    }
    assert_ne!(
        state, WAIT_TIMEOUT,
        "worker process {pid} is still running after pool cleanup"
    );
    assert_eq!(state, WAIT_OBJECT_0, "waiting for worker process {pid}");
}

fn wait_for_exit(pid: u32) {
    // SAFETY: OpenProcess takes no pointer arguments; PROCESS_SYNCHRONIZE, inherit flag 0
    // and `pid` are scalar values. A NULL return means failure and is caught by the assert
    // at line 184 before the handle is used.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    assert!(!process.is_null(), "opening worker process {pid}");
    // SAFETY: `process` is non-null (asserted at line 184) and was opened with SYNCHRONIZE
    // access, which WaitForSingleObject requires. The handle stays open until CloseHandle
    // at line 193.
    let state = unsafe { WaitForSingleObject(process, 3_000) };
    // SAFETY: `process` is the handle from OpenProcess at line 183 that passed the non-null
    // assert at line 184. It has not been closed yet, so this CloseHandle call is its only
    // close and the handle is not used after it.
    unsafe {
        CloseHandle(process);
    }
    assert_eq!(
        state, WAIT_OBJECT_0,
        "waiting for worker process {pid} to exit"
    );
}

fn error_kind(error: &anyhow::Error) -> worker_backend::WorkerErrorKind {
    error
        .downcast_ref::<worker_backend::WorkerError>()
        .expect("worker error carries structured effect certainty")
        .kind
}

#[test]
fn worker_reuse_preserves_bytes_status_and_stderr_then_reaps_on_drop() {
    let directory = TempDir::new().expect("temporary worker directory");
    let pool = WorkerPool::default();
    let worker_launch = launch(&directory, "sequence");

    let first = pool
        .invoke(worker_launch.clone(), request(&[0, 10, 128, 255]))
        .expect("first sequence response");
    let second = pool
        .invoke(worker_launch, request(&[255, 0, 13]))
        .expect("second sequence response");
    assert_eq!(first.output, b"1");
    assert_eq!(second.output, b"2");
    assert!(first.spawned_process);
    assert!(!second.spawned_process);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("launches"))
            .unwrap()
            .lines()
            .count(),
        1
    );

    let typed = pool
        .invoke(launch(&directory, "typed"), request(&[]))
        .expect("typed response");
    assert_eq!(typed.output, br#"{"n":42}"#);
    assert_eq!(typed.stderr, "é: diagnostic".as_bytes());

    let status = pool
        .invoke(launch(&directory, "status"), request(&[]))
        .expect("status response");
    assert_eq!(status.status, -17);
    assert_eq!(status.output, [0, 255, 1]);
    assert_eq!(status.stderr, "é: diagnostic".as_bytes());

    let pids = std::fs::read_to_string(directory.path().join("launches"))
        .unwrap()
        .lines()
        .map(|line| line.parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    drop(pool);
    for pid in pids {
        assert_reaped(pid);
    }
}

#[test]
fn exited_idle_worker_is_replaced_before_the_next_request() {
    let directory = TempDir::new().expect("temporary idle-exit directory");
    let pool = WorkerPool::default();
    let worker_launch = launch(&directory, "exit-after-one");

    let first = pool
        .invoke(worker_launch.clone(), request(b"first"))
        .expect("first response before worker exits");
    assert_eq!(first.output, b"first");
    assert!(first.spawned_process);
    let first_pid = marker_pid(&directory);
    wait_for_exit(first_pid);

    let second = pool
        .invoke(worker_launch, request(b"second"))
        .expect("a dead idle worker is replaced for the next request");
    assert_eq!(second.output, b"second");
    assert!(second.spawned_process);
    let pids = std::fs::read_to_string(directory.path().join("launches"))
        .unwrap()
        .lines()
        .map(|line| line.parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 2);

    drop(pool);
    for pid in pids {
        assert_reaped(pid);
    }
}

#[test]
fn exited_worker_unsolicited_output_is_a_protocol_error() {
    let directory = TempDir::new().expect("temporary unsolicited-output directory");
    let pool = WorkerPool::default();
    let worker_launch = launch(&directory, "trailing-and-exit");
    let response = pool
        .invoke(worker_launch.clone(), request(b"request"))
        .expect("first response before trailing bytes");
    assert_eq!(response.output, b"request");
    let pid = marker_pid(&directory);
    std::fs::write(directory.path().join("effects"), b"release trailing output")
        .expect("release fixture trailing output");
    wait_for_exit(pid);

    let error = pool
        .invoke(worker_launch, request(b"must-not-be-sent"))
        .expect_err("queued unsolicited bytes violate the worker protocol");
    assert_eq!(
        error_kind(&error),
        worker_backend::WorkerErrorKind::ProtocolError
    );
    assert!(
        !error
            .downcast_ref::<worker_backend::WorkerError>()
            .unwrap()
            .uncertain_effect,
        "the second request must be rejected before transmission"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("launches"))
            .unwrap()
            .lines()
            .count(),
        1,
        "protocol failures must not be replayed"
    );
    assert_reaped(pid);
}

#[test]
fn typed_worker_output_flows_through_the_runtime_contract() {
    let directory = TempDir::new().expect("temporary worker directory");
    let marker = directory.path().join("runtime-launches");
    let source = json!({
        "name": "worker.fixture",
        "binary": fixture(),
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "json_lines_worker",
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {"n": {"type": "integer"}},
                "required": ["n"]
            },
            "lifecycle": {
                "kind": "json_lines",
                "startup_arguments": [
                    "json-lines", "typed", marker.to_string_lossy()
                ]
            }
        }
    });
    let registry = manifest::Registry::from_sources(&[source.to_string()]).unwrap();
    let options = runtime::RunOptions {
        max_output_bytes: MAX_FRAME,
        command_timeout: Some(Duration::from_secs(5)),
        ..runtime::RunOptions::new(directory.path().to_path_buf())
    };
    let program = parser::parse(
        "run worker.fixture as result\ntake integer at \"/n\" from result as number\nthen encode number as json\n",
    )
    .unwrap();
    let result = runtime::run(&program, &registry, &options).expect("typed worker program");
    assert_eq!(result.output, b"42");
    let pid = std::fs::read_to_string(marker)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    drop(registry);
    assert_reaped(pid);
}

#[test]
fn malformed_and_oversized_responses_kill_workers_without_replay() {
    for (mode, expected_kind, maximum) in [
        (
            "wrong-id",
            worker_backend::WorkerErrorKind::ProtocolError,
            128,
        ),
        (
            "invalid-json",
            worker_backend::WorkerErrorKind::ProtocolError,
            128,
        ),
        (
            "oversized",
            worker_backend::WorkerErrorKind::ProtocolError,
            128,
        ),
        (
            "partial",
            worker_backend::WorkerErrorKind::TransportError,
            128,
        ),
        ("eof", worker_backend::WorkerErrorKind::TransportError, 128),
    ] {
        let directory = TempDir::new().expect("temporary malformed-response directory");
        let mut worker_launch = launch(&directory, mode);
        worker_launch.max_frame_bytes = maximum;
        let error = WorkerPool::default()
            .invoke(worker_launch, request(&[]))
            .expect_err("malformed response must fail");
        assert_eq!(error_kind(&error), expected_kind, "worker mode {mode}");
        let worker_error = error.downcast_ref::<worker_backend::WorkerError>().unwrap();
        assert!(worker_error.uncertain_effect, "worker mode {mode}");
        assert_eq!(worker_error.captured_stdout, None);
        assert_reaped(marker_pid(&directory));
        assert_eq!(
            std::fs::read_to_string(directory.path().join("launches"))
                .unwrap()
                .lines()
                .count(),
            1,
            "failed requests are never replayed"
        );
    }
}

#[test]
fn pool_capacity_and_sensitivity_floor_survive_windows_reuse() {
    let directory = TempDir::new().expect("temporary pool directory");
    let pool = WorkerPool::with_capacity(1).expect("one worker slot");
    let first_launch = launch(&directory, "sequence");
    let mut first_request = request(&[]);
    first_request.sensitivity = security::SensitivitySet::from_label(Sensitivity::Secret);
    assert_eq!(
        pool.invoke(first_launch.clone(), first_request)
            .unwrap()
            .output,
        b"1"
    );

    let second_request = request(&[]);
    let second_spawned = second_request.spawned.clone();
    let error = pool
        .invoke(launch(&directory, "echo"), second_request)
        .expect_err("full pool must reject another worker identity");
    assert_eq!(
        error_kind(&error),
        worker_backend::WorkerErrorKind::CapacityError
    );
    assert!(!second_spawned.load(Ordering::Relaxed));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("launches"))
            .unwrap()
            .lines()
            .count(),
        1
    );

    let mut public = request(&[]);
    public.sensitivity = security::SensitivitySet::from_label(Sensitivity::Public);
    let error = pool
        .invoke(first_launch.clone(), public)
        .expect_err("public call cannot reuse a worker retaining secret input");
    assert!(error.to_string().contains("request was not started"));
    assert_eq!(
        pool.invoke(first_launch, {
            let mut request = request(&[]);
            request.sensitivity = security::SensitivitySet::from_label(Sensitivity::Secret);
            request
        })
        .unwrap()
        .output,
        b"2"
    );

    let pid = marker_pid(&directory);
    drop(pool);
    assert_reaped(pid);
}

#[test]
fn timeout_and_cancellation_reap_windows_workers_even_during_blocked_writes() {
    let timeout_directory = TempDir::new().expect("temporary timeout directory");
    let mut timeout_launch = launch(&timeout_directory, "slow");
    timeout_launch.timeout = Duration::from_secs(1);
    let started = Instant::now();
    let error = WorkerPool::default()
        .invoke(timeout_launch, request(&[]))
        .expect_err("worker deadline must time out");
    assert_eq!(error_kind(&error), worker_backend::WorkerErrorKind::Timeout);
    assert!(
        error
            .downcast_ref::<worker_backend::WorkerError>()
            .unwrap()
            .uncertain_effect
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        timeout_directory.path().join("effects").exists(),
        "worker must receive the request before timing out"
    );
    assert_reaped(marker_pid(&timeout_directory));

    let cancel_directory = TempDir::new().expect("temporary cancellation directory");
    let pool = Arc::new(WorkerPool::default());
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancel_request = request(&[]);
    cancel_request.cancelled = cancelled.clone();
    let invocation_pool = pool.clone();
    let invocation_launch = launch(&cancel_directory, "slow");
    let invocation =
        thread::spawn(move || invocation_pool.invoke(invocation_launch, cancel_request));
    let effects = cancel_directory.path().join("effects");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !effects.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        effects.exists(),
        "worker did not receive cancellation request"
    );
    cancelled.store(true, Ordering::Relaxed);
    let error = invocation
        .join()
        .expect("cancellation invocation did not panic")
        .expect_err("cancelled worker call must fail");
    assert_eq!(
        error_kind(&error),
        worker_backend::WorkerErrorKind::Cancelled
    );
    assert!(
        error
            .downcast_ref::<worker_backend::WorkerError>()
            .unwrap()
            .uncertain_effect
    );
    assert_reaped(marker_pid(&cancel_directory));
    drop(pool);

    let blocked_directory = TempDir::new().expect("temporary blocked-writer directory");
    let blocked_pool = Arc::new(WorkerPool::default());
    let blocked_cancelled = Arc::new(AtomicBool::new(false));
    let mut blocked_request = request(&vec![b'x'; 3 * 1024 * 1024]);
    blocked_request.cancelled = blocked_cancelled.clone();
    let blocked_pool_for_thread = blocked_pool.clone();
    let blocked_launch = launch(&blocked_directory, "ignore");
    let invocation =
        thread::spawn(move || blocked_pool_for_thread.invoke(blocked_launch, blocked_request));
    let launch_marker = blocked_directory.path().join("launches");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !launch_marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(launch_marker.exists(), "ignore worker did not start");
    thread::sleep(Duration::from_millis(100));
    blocked_cancelled.store(true, Ordering::Relaxed);
    let error = invocation
        .join()
        .expect("blocked-writer invocation did not panic")
        .expect_err("cancellation must interrupt a blocked worker pipe write");
    assert_eq!(
        error_kind(&error),
        worker_backend::WorkerErrorKind::Cancelled
    );
    assert!(
        error
            .downcast_ref::<worker_backend::WorkerError>()
            .unwrap()
            .uncertain_effect
    );
    assert_reaped(marker_pid(&blocked_directory));
    drop(blocked_pool);
}

#[test]
fn same_identity_calls_are_serialized_on_one_worker() {
    let directory = TempDir::new().expect("temporary concurrent worker directory");
    let pool = Arc::new(WorkerPool::with_capacity(2).unwrap());
    let worker_launch = launch(&directory, "sequence");
    let calls = 8;
    let workers = (0..calls)
        .map(|_| {
            let pool = pool.clone();
            let launch = worker_launch.clone();
            thread::spawn(move || {
                pool.invoke(launch, request(&[]))
                    .expect("serialized worker response")
                    .output
            })
        })
        .collect::<Vec<_>>();
    let mut values = workers
        .into_iter()
        .map(|worker| {
            String::from_utf8(worker.join().expect("worker invocation thread"))
                .unwrap()
                .parse::<usize>()
                .unwrap()
        })
        .collect::<Vec<_>>();
    values.sort_unstable();
    assert_eq!(values, (1..=calls).collect::<Vec<_>>());
    let pids = std::fs::read_to_string(directory.path().join("launches"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(pids.len(), 1);
    let pid = pids.iter().next().unwrap().parse().unwrap();
    drop(pool);
    assert_reaped(pid);
}
