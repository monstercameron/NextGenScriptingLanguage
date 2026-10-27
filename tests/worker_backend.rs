#![cfg(unix)]

#[path = "../src/runtime/worker_backend.rs"]
mod worker_backend;

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

// A `#[path]` inside an inline module resolves under `tests/platform/`, which does not
// exist, so Linux cannot resolve `..` through it. Declare the file at the crate root.
#[path = "../src/platform/unix_ffi.rs"]
pub(crate) mod unix_ffi_source;

mod platform {
    pub(crate) use super::unix_ffi_source as unix_ffi;
}

// The harness includes the whole FFI module but uses only some of its functions.
const _: fn() = || {
    let _ = (
        platform::unix_ffi::set_close_on_exec,
        platform::unix_ffi::duplicate_cloexec,
        platform::unix_ffi::effective_uid,
    );
};

use flow::ast::Value;
use flow::{parser, runtime};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use worker_backend::{WorkerLaunch, WorkerPool, WorkerRequest};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_STDERR: usize = 1024;
static WORKER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_worker_tests() -> std::sync::MutexGuard<'static, ()> {
    WORKER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn python3() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("Python 3 is available for fixture tests")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/json_lines_worker.py")
}

fn fixture_registry() -> manifest::Registry {
    let source = serde_json::json!({
        "name": "worker.fixture",
        "binary": "python3",
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "json_lines_worker",
            "lifecycle": {"kind": "json_lines"}
        }
    });
    manifest::Registry::from_sources(&[source.to_string()]).expect("valid fixture manifest")
}

fn secret_fixture_registry() -> manifest::Registry {
    let source = serde_json::json!({
        "name": "worker.fixture",
        "binary": "python3",
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "json_lines_worker",
            "lifecycle": {"kind": "json_lines"},
            "environment": {"set": {"FLOW_SECRET": "worker-secret-value"}}
        }
    });
    let registry = manifest::Registry::from_sources(&[source.to_string()])
        .expect("valid secret fixture manifest");
    registry.set_host_state(
        security::DataFlowPolicy::from_json(
            br#"{"version":1,"secrets":{"environment":{"worker.fixture":["FLOW_SECRET"]}}}"#,
        )
        .expect("valid data flow policy"),
    );
    registry
}

fn launch(directory: &TempDir, mode: &str) -> WorkerLaunch {
    launch_for_registry(directory, mode, fixture_registry())
}

fn launch_for_registry(
    directory: &TempDir,
    mode: &str,
    registry: manifest::Registry,
) -> WorkerLaunch {
    WorkerLaunch::inherited(
        registry,
        "worker.fixture".to_owned(),
        python3(),
        directory.path().to_path_buf(),
        vec![
            fixture().into_os_string(),
            OsString::from("--worker"),
            OsString::from(mode),
            directory.path().join("launches").into_os_string(),
            directory.path().join("effects").into_os_string(),
        ],
        worker_backend::WorkerLimits {
            timeout: Duration::from_secs(5),
            max_frame_bytes: MAX_FRAME,
            max_stderr_bytes: MAX_STDERR,
        },
    )
    .expect("valid worker launch")
}

fn request(arguments: &[&str], input: &[u8]) -> WorkerRequest {
    WorkerRequest {
        arguments: arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        input: input.to_vec(),
        max_output_bytes: MAX_FRAME,
        sensitivity: security::SensitivitySet::from_label(flow::contracts::Sensitivity::Unknown),
        deadline: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        spawned: Arc::new(AtomicBool::new(false)),
    }
}

fn recorded_pid(directory: &TempDir) -> i32 {
    let contents = std::fs::read_to_string(directory.path().join("launches"))
        .expect("worker wrote launch marker");
    contents
        .lines()
        .next()
        .expect("worker launch marker has one PID")
        .parse()
        .expect("worker PID is numeric")
}

fn assert_reaped(pid: i32) {
    // SAFETY: signal 0 sends nothing and kill takes no pointers. `pid` is an integer parsed
    // from the worker's launch marker by recorded_pid. errno is read by the assert below,
    // with no other libc call in between, so it still holds this call's failure code.
    let result = unsafe { libc::kill(pid, 0) };
    assert_eq!(result, -1, "worker process {pid} is still present");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "worker process {pid} was not reaped"
    );
}

fn worker_error(error: &anyhow::Error) -> &worker_backend::WorkerError {
    error
        .downcast_ref::<worker_backend::WorkerError>()
        .expect("worker invocation error carries effect certainty")
}

#[test]
fn one_worker_serves_multiple_requests_and_preserves_input_bytes() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let pool = WorkerPool::default();
    let first_launch = launch(&directory, "echo_both");

    let first_input = [0, 10, 13, 34, 92, 127, 128, 255];
    let first_request = request(&["first", "literal arg"], &first_input);
    let first_spawned = first_request.spawned.clone();
    let first = pool
        .invoke(first_launch.clone(), first_request)
        .expect("first worker request");
    assert_eq!(
        first.output,
        [first_input.as_slice(), b"|first|literal arg"].concat()
    );
    assert_eq!(first.status, 0);
    assert!(first.spawned_process);
    assert!(first_spawned.load(Ordering::Relaxed));

    let second_input = [255, 0, 1, 2, 240];
    let second_request = request(&["second"], &second_input);
    let second_spawned = second_request.spawned.clone();
    let second = pool
        .invoke(first_launch, second_request)
        .expect("second worker request");
    assert_eq!(
        second.output,
        [second_input.as_slice(), b"|second"].concat()
    );
    assert!(!second.spawned_process);
    assert!(!second_spawned.load(Ordering::Relaxed));

    assert_eq!(
        std::fs::read_to_string(directory.path().join("launches")).expect("launch count marker"),
        format!("{}\n", recorded_pid(&directory)),
        "one process should serve both records"
    );
    let retained_pid = recorded_pid(&directory);
    drop(pool);
    assert_reaped(retained_pid);

    // The echo-input worker uses the same protocol's raw byte array transport.
    let echo_pool = WorkerPool::default();
    let echo = echo_pool
        .invoke(
            launch(&directory, "echo_input"),
            request(&[], &[0, 10, 13, 34, 92, 127, 128, 255]),
        )
        .expect("binary input response");
    assert_eq!(echo.output, [0, 10, 13, 34, 92, 127, 128, 255]);
}

#[test]
fn registry_lifetime_owns_persistent_worker_cleanup() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let registry = fixture_registry();
    let final_registry = registry.clone();
    registry
        .host_state::<WorkerPool>()
        .invoke(
            launch_for_registry(&directory, "echo_input", registry.clone()),
            request(&[], b"alive"),
        )
        .expect("worker invocation through registry-owned pool");
    let pid = recorded_pid(&directory);

    drop(registry);
    // SAFETY: signal 0 sends nothing and takes only an integer. `pid` came from the launch
    // marker of the worker this test just invoked. `final_registry` is a clone that still
    // owns the pool, so the worker has not been reaped and the pid still names it.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    drop(final_registry);
    assert_reaped(pid);
}

#[test]
fn pool_rejects_new_identities_at_capacity_without_evicting_state() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let pool = WorkerPool::default();
    assert!(WorkerPool::with_capacity(0).is_err());
    assert!(WorkerPool::with_capacity(worker_backend::MAX_POOL_CAPACITY + 1).is_err());

    let mut first_launch = None;
    for index in 0..16 {
        let mode = if index == 0 { "sequence" } else { "echo_input" };
        let mut worker_launch = launch(&directory, mode);
        worker_launch
            .environment
            .retain(|(name, _)| name != OsStr::new("FLOW_WORKER_POOL_KEY"));
        worker_launch.environment.push((
            OsString::from("FLOW_WORKER_POOL_KEY"),
            OsString::from(index.to_string()),
        ));
        if index == 0 {
            first_launch = Some(worker_launch.clone());
        }
        let response = pool
            .invoke(worker_launch, request(&[], b"pool"))
            .expect("pool worker response");
        if index == 0 {
            assert_eq!(response.output, b"1");
        } else {
            assert_eq!(response.output, b"pool");
        }
        assert!(response.spawned_process);
    }

    let rejected_request = request(&[], b"overflow");
    let rejected_spawn_marker = rejected_request.spawned.clone();
    let error = pool
        .invoke(launch(&directory, "echo_input"), rejected_request)
        .expect_err("a full pool must reject a new worker identity without eviction");
    assert_eq!(
        worker_error(&error).kind,
        worker_backend::WorkerErrorKind::CapacityError
    );
    assert!(!worker_error(&error).uncertain_effect);
    assert_eq!(worker_error(&error).completed_status, None);
    assert!(!rejected_spawn_marker.load(Ordering::Relaxed));
    assert!(error.to_string().contains("request was not started"));

    let pids = std::fs::read_to_string(directory.path().join("launches"))
        .expect("worker launch markers")
        .lines()
        .map(|line| line.parse::<i32>().expect("numeric worker pid"))
        .collect::<HashSet<_>>();
    let live_workers = pids
        .iter()
        // SAFETY: signal 0 only probes existence and sends nothing. Each `pid` is an integer
        // parsed from a launch marker that this test wrote through its own workers.
        .filter(|pid| unsafe { libc::kill(**pid, 0) } == 0)
        .count();
    assert_eq!(live_workers, 16, "pool must retain at most 16 workers");

    let continued = pool
        .invoke(
            first_launch.expect("first worker launch"),
            request(&[], b"pool"),
        )
        .expect("the first worker remains available after capacity rejection");
    assert_eq!(
        continued.output, b"2",
        "worker-local state must be preserved"
    );
    assert!(!continued.spawned_process);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("launches"))
            .expect("worker launch markers")
            .lines()
            .count(),
        16,
        "capacity rejection and reuse must not launch or evict processes"
    );

    drop(pool);
    for pid in pids {
        assert_reaped(pid);
    }
}

// isolated_like serves graph program services only.
#[cfg(feature = "graph")]
#[test]
fn isolated_pool_forks_capacity_but_not_retained_workers() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let pool = WorkerPool::with_capacity(1).expect("valid worker capacity");
    let worker_launch = launch(&directory, "sequence");
    let first = pool
        .invoke(worker_launch.clone(), request(&[], b"parent"))
        .expect("parent pool worker starts");
    assert!(first.spawned_process);

    let isolated = pool.isolated_like();
    let isolated_first = isolated
        .invoke(worker_launch, request(&[], b"isolated"))
        .expect("isolated pool starts a private copy of the worker");
    assert!(
        isolated_first.spawned_process,
        "the source pool's retained worker must not be reused"
    );
    assert_eq!(isolated_first.output, b"1");

    let rejected = isolated
        .invoke(launch(&directory, "echo_input"), request(&[], b"capacity"))
        .expect_err("the isolated pool retains the configured capacity of one");
    assert_eq!(
        worker_error(&rejected).kind,
        worker_backend::WorkerErrorKind::CapacityError
    );
    assert!(rejected.to_string().contains("capacity 1 is full"));

    let pids = std::fs::read_to_string(directory.path().join("launches"))
        .expect("both pools should write worker launch markers")
        .lines()
        .map(|line| line.parse::<i32>().expect("numeric worker pid"))
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 2);
    drop(isolated);
    drop(pool);
    for pid in pids {
        assert_reaped(pid);
    }
}

#[test]
fn concurrent_calls_for_one_identity_are_serialized_on_one_worker() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let pool = Arc::new(WorkerPool::with_capacity(4).expect("valid worker capacity"));
    let worker_launch = launch(&directory, "sequence");
    let calls = 12;
    let mut invocations = Vec::new();

    for _ in 0..calls {
        let pool = pool.clone();
        let worker_launch = worker_launch.clone();
        invocations.push(thread::spawn(move || {
            pool.invoke(worker_launch, request(&[], &[]))
                .expect("serialized worker response")
                .output
        }));
    }

    let mut sequence = invocations
        .into_iter()
        .map(|invocation| {
            String::from_utf8(invocation.join().expect("invocation thread did not panic"))
                .expect("ASCII sequence")
                .parse::<usize>()
                .expect("numeric worker sequence")
        })
        .collect::<Vec<_>>();
    sequence.sort_unstable();
    assert_eq!(sequence, (1..=calls).collect::<Vec<_>>());

    let pids = std::fs::read_to_string(directory.path().join("launches"))
        .expect("worker launch markers")
        .lines()
        .map(|line| line.parse::<i32>().expect("numeric worker pid"))
        .collect::<HashSet<_>>();
    assert_eq!(
        pids.len(),
        1,
        "one identity must retain one stateful worker"
    );
    let pid = *pids.iter().next().expect("one worker PID");
    drop(pool);
    assert_reaped(pid);
}

#[test]
fn serialized_reuse_rechecks_worker_history_before_sending_a_request() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let pool = Arc::new(WorkerPool::default());
    let worker_launch = launch(&directory, "remember_first_pause");
    let mut secret = request(&[], b"TOP-SECRET-MARKER");
    secret.sensitivity = security::SensitivitySet::from_label(flow::contracts::Sensitivity::Secret);
    let secret_pool = Arc::clone(&pool);
    let secret_launch = worker_launch.clone();
    let secret_request = thread::spawn(move || secret_pool.invoke(secret_launch, secret));

    let effects = directory.path().join("effects");
    for _ in 0..200 {
        if effects.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(effects.exists(), "secret request reached the worker");
    assert!(pool
        .sensitivity_history_for_launch(&worker_launch)
        .unwrap()
        .contains(&flow::contracts::Sensitivity::Secret));

    let mut public = request(&[], b"public");
    public.sensitivity = security::SensitivitySet::from_label(flow::contracts::Sensitivity::Public);
    let public_error = pool
        .invoke(worker_launch.clone(), public)
        .expect_err("public floor must not reuse a worker with retained secret state");
    assert!(public_error.to_string().contains("request was not started"));
    assert!(!worker_error(&public_error).uncertain_effect);
    assert_eq!(worker_error(&public_error).completed_status, None);
    assert_eq!(
        std::fs::read_to_string(&effects).expect("worker request marker"),
        "received\n",
        "the rejected request must not reach the worker"
    );

    let secret_response = secret_request
        .join()
        .expect("secret request thread did not panic")
        .expect("secret worker response");
    assert_eq!(secret_response.output, b"TOP-SECRET-MARKER");

    let mut authorized = request(&[], &[]);
    authorized.sensitivity = security::SensitivitySet::from_labels([
        flow::contracts::Sensitivity::Public,
        flow::contracts::Sensitivity::Secret,
    ]);
    let resumed = pool
        .invoke(worker_launch, authorized)
        .expect("a floor containing the retained labels can reuse the worker");
    assert_eq!(resumed.output, b"TOP-SECRET-MARKER");
    assert_eq!(
        std::fs::read_to_string(&effects).unwrap().lines().count(),
        2
    );
}

#[test]
fn output_and_stderr_limits_are_enforced() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let pool = WorkerPool::default();
    let mut output_request = request(&[], &[]);
    output_request.max_output_bytes = 8;
    let error = pool
        .invoke(launch(&directory, "large_output"), output_request)
        .expect_err("oversized output must be rejected");
    assert!(error
        .to_string()
        .contains("exceeding the maximum of 8 bytes"));
    assert!(!worker_error(&error).uncertain_effect);
    assert_eq!(worker_error(&error).completed_status, Some(0));
    assert_eq!(
        worker_error(&error).kind,
        worker_backend::WorkerErrorKind::OutputLimit
    );
    assert_eq!(worker_error(&error).captured_stdout, Some(vec![b'x'; 256]));
    assert_eq!(worker_error(&error).captured_stderr, Some(Vec::new()));
    let oversized_output_pid = recorded_pid(&directory);
    let error = pool
        .invoke(launch(&directory, "large_output"), {
            let mut next = request(&[], &[]);
            next.max_output_bytes = 8;
            next
        })
        .expect_err("later capped response remains synchronized");
    assert_eq!(
        worker_error(&error).kind,
        worker_backend::WorkerErrorKind::OutputLimit
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("launches"))
            .expect("launch count marker")
            .lines()
            .count(),
        1,
        "a complete oversized response should leave the worker reusable"
    );

    let stderr_directory = TempDir::new().expect("temporary stderr worker directory");
    let mut stderr_launch = launch(&stderr_directory, "large_stderr");
    stderr_launch.max_stderr_bytes = 12;
    let error = pool
        .invoke(stderr_launch.clone(), request(&[], &[]))
        .expect_err("oversized diagnostic must be rejected");
    assert!(error.to_string().contains("stderr was 128 bytes"));
    assert!(error
        .to_string()
        .contains("request completed with status 0"));
    assert!(!worker_error(&error).uncertain_effect);
    assert_eq!(worker_error(&error).completed_status, Some(0));
    assert_eq!(
        worker_error(&error).kind,
        worker_backend::WorkerErrorKind::OutputLimit
    );
    assert_eq!(worker_error(&error).captured_stdout, Some(Vec::new()));
    assert_eq!(worker_error(&error).captured_stderr, Some(vec![b'e'; 128]));
    let stderr_pid = recorded_pid(&stderr_directory);
    let error = pool
        .invoke(stderr_launch.clone(), request(&[], &[]))
        .expect_err("later capped diagnostic remains synchronized");
    assert_eq!(
        worker_error(&error).kind,
        worker_backend::WorkerErrorKind::OutputLimit
    );
    assert_eq!(
        std::fs::read_to_string(stderr_directory.path().join("launches"))
            .expect("launch count marker")
            .lines()
            .count(),
        1,
        "a complete oversized diagnostic should leave the worker reusable"
    );
    // SAFETY: signal 0 sends nothing and takes only an integer. `stderr_pid` is the worker
    // recorded in stderr_directory; `pool` still owns it until the drop on the next line,
    // so it has not been reaped and the pid still names it.
    assert_eq!(unsafe { libc::kill(stderr_pid, 0) }, 0);
    drop(pool);
    assert_reaped(stderr_pid);
    assert_reaped(oversized_output_pid);
}

#[test]
fn status_and_diagnostic_are_preserved() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let response = WorkerPool::default()
        .invoke(launch(&directory, "status"), request(&[], &[]))
        .expect("status worker response");
    assert_eq!(response.status, -17);
    assert_eq!(response.output, [0, 255, 1]);
    assert_eq!(response.stderr, "é: diagnostic".as_bytes());
}

#[test]
fn declared_secret_environment_registers_redaction_without_changing_typed_bytes() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let registry = secret_fixture_registry();
    let response = WorkerPool::default()
        .invoke(
            launch_for_registry(&directory, "secret_stderr", registry.clone()),
            request(&[], &[]),
        )
        .expect("worker response");
    assert_eq!(response.output, b"worker-secret-value");
    assert_eq!(response.stderr, b"worker-secret-value");
    assert_eq!(
        security::secret_redactor(&registry).redact_text("worker-secret-value"),
        "[REDACTED]"
    );

    let capped_directory = TempDir::new().expect("temporary capped worker directory");
    let capped_registry = secret_fixture_registry();
    let mut worker_launch =
        launch_for_registry(&capped_directory, "secret_stderr", capped_registry);
    worker_launch.max_stderr_bytes = 1;
    let error = WorkerPool::default()
        .invoke(worker_launch, request(&[], &[]))
        .expect_err("oversized diagnostic retains a completed response");
    assert_eq!(
        worker_error(&error).captured_stdout,
        Some(b"worker-secret-value".to_vec())
    );
    assert_eq!(
        worker_error(&error).captured_stderr,
        Some(b"worker-secret-value".to_vec())
    );
    assert!(
        !error.to_string().contains("worker-secret-value"),
        "error display must not interpolate captured secret diagnostics"
    );
}

#[test]
fn runtime_error_display_redacts_secret_but_structured_bytes_stay_raw() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary runtime worker directory");
    let launch_marker = directory.path().join("runtime-launches");
    let effect_marker = directory.path().join("runtime-effects");
    let manifest_source = serde_json::json!({
        "name": "worker.fixture",
        "binary": python3().to_string_lossy(),
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "json_lines_worker",
            "environment": {"set": {"FLOW_SECRET": "worker-secret-value"}},
            "lifecycle": {
                "kind": "json_lines",
                "startup_arguments": [
                    fixture().to_string_lossy(),
                    "--worker",
                    "secret_failure",
                    launch_marker.to_string_lossy(),
                    effect_marker.to_string_lossy()
                ]
            },
            "outcomes": [
                {"status": 0, "name": "success", "kind": "success"},
                {"status": 2, "name": "failure", "kind": "failure"}
            ]
        }
    });
    let registry = manifest::Registry::from_sources(&[manifest_source.to_string()])
        .expect("valid runtime worker manifest");
    registry.set_host_state(
        security::DataFlowPolicy::from_json(
            br#"{"version":1,"secrets":{"environment":{"worker.fixture":["FLOW_SECRET"]}}}"#,
        )
        .expect("valid data flow policy"),
    );
    let options = runtime::RunOptions {
        max_output_bytes: MAX_FRAME,
        command_timeout: Some(Duration::from_secs(5)),
        ..runtime::RunOptions::new(directory.path().to_path_buf())
    };
    let error = runtime::run(
        &parser::parse("run worker.fixture\n").expect("valid fixture program"),
        &registry,
        &options,
    )
    .err()
    .expect("declared failure should be retained as a runtime error");

    assert!(error.to_string().contains("[REDACTED]"));
    assert!(!error.to_string().contains("worker-secret-value"));
    let Value::Record(fields) = runtime::execution_result::error_value(&error) else {
        panic!("runtime error value must be a record");
    };
    let captured_bytes = |field: &str| {
        let Some(Value::List(bytes)) = fields.get(field) else {
            panic!("expected captured {field} bytes");
        };
        bytes
            .iter()
            .map(|value| match value {
                Value::Integer(byte) => u8::try_from(*byte).expect("byte in range"),
                _ => panic!("expected byte value"),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(captured_bytes("stdout"), b"worker-secret-value");
    assert_eq!(captured_bytes("stderr"), b"worker-secret-value");
}

#[test]
fn oversized_request_is_rejected_before_worker_spawn() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let pool = WorkerPool::default();
    let mut worker_launch = launch(&directory, "echo_input");
    worker_launch.max_frame_bytes = 16;
    let worker_request = request(&[], &[0; 64]);
    let spawned = worker_request.spawned.clone();
    let error = pool
        .invoke(worker_launch, worker_request)
        .expect_err("oversized request frame must be rejected");

    assert_eq!(
        worker_error(&error).kind,
        worker_backend::WorkerErrorKind::OutputLimit
    );
    assert!(!worker_error(&error).uncertain_effect);
    assert_eq!(worker_error(&error).completed_status, None);
    assert!(!spawned.load(Ordering::Relaxed));
    assert!(!directory.path().join("launches").exists());
}

#[test]
fn timeout_and_cancellation_kill_and_reap_worker() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().expect("temporary worker directory");
    let pool = WorkerPool::default();
    let mut slow_launch = launch(&directory, "slow");
    slow_launch.timeout = Duration::from_secs(1);
    let started = Instant::now();
    let timeout_request = request(&[], &[]);
    let timeout_spawned = timeout_request.spawned.clone();
    let error = pool
        .invoke(slow_launch, timeout_request)
        .expect_err("worker timeout must fail");
    assert!(error.to_string().contains("outcome is uncertain"));
    assert!(worker_error(&error).uncertain_effect);
    assert_eq!(worker_error(&error).completed_status, None);
    assert_eq!(
        worker_error(&error).kind,
        worker_backend::WorkerErrorKind::Timeout
    );
    assert!(timeout_spawned.load(Ordering::Relaxed));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_reaped(recorded_pid(&directory));

    let cancel_directory = TempDir::new().expect("cancellation worker directory");
    let cancel_pool = Arc::new(WorkerPool::default());
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancel_request = request(&[], &[]);
    cancel_request.cancelled = cancelled.clone();
    let cancellation_spawned = cancel_request.spawned.clone();
    let invocation_pool = cancel_pool.clone();
    let invocation_launch = launch(&cancel_directory, "slow");
    let invocation =
        thread::spawn(move || invocation_pool.invoke(invocation_launch, cancel_request));

    let marker = cancel_directory.path().join("effects");
    let marker_deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < marker_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "worker did not receive the cancellation test request"
    );
    cancelled.store(true, Ordering::Relaxed);
    let error = invocation
        .join()
        .expect("invocation thread did not panic")
        .expect_err("cancellation must fail");
    assert!(error.to_string().contains("outcome is uncertain"));
    assert!(worker_error(&error).uncertain_effect);
    assert_eq!(
        worker_error(&error).kind,
        worker_backend::WorkerErrorKind::Cancelled
    );
    assert!(cancellation_spawned.load(Ordering::Relaxed));
    assert_reaped(recorded_pid(&cancel_directory));
}

#[test]
fn malformed_or_missing_response_kills_worker_without_replay() {
    let _guard = lock_worker_tests();
    for mode in ["wrong_id", "eof", "partial", "invalid_json"] {
        let directory = TempDir::new().expect("temporary worker directory");
        let pool = WorkerPool::default();
        let worker_request = request(&[], &[]);
        let spawned = worker_request.spawned.clone();
        let error = pool
            .invoke(launch(&directory, mode), worker_request)
            .expect_err("invalid response must fail");
        assert!(error.to_string().contains("outcome is uncertain"));
        assert!(worker_error(&error).uncertain_effect);
        let expected_kind = match mode {
            "wrong_id" | "invalid_json" => worker_backend::WorkerErrorKind::ProtocolError,
            "eof" | "partial" => worker_backend::WorkerErrorKind::TransportError,
            _ => unreachable!(),
        };
        assert_eq!(worker_error(&error).kind, expected_kind);
        assert!(spawned.load(Ordering::Relaxed));
        assert_eq!(worker_error(&error).captured_stdout, None);
        assert_eq!(worker_error(&error).captured_stderr, None);
        assert_reaped(recorded_pid(&directory));
    }

    let directory = TempDir::new().expect("effect worker directory");
    let pool = WorkerPool::default();
    let effect_launch = launch(&directory, "effect_then_eof");
    let error = pool
        .invoke(effect_launch.clone(), request(&[], &[]))
        .expect_err("missing response after effect must fail");
    assert!(error.to_string().contains("outcome is uncertain"));
    assert!(worker_error(&error).uncertain_effect);
    assert_eq!(
        worker_error(&error).kind,
        worker_backend::WorkerErrorKind::TransportError
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("effects"))
            .expect("fixture recorded its effect")
            .lines()
            .count(),
        1,
        "an uncertain request must never be replayed automatically"
    );
    assert_reaped(recorded_pid(&directory));

    let error = pool
        .invoke(effect_launch, request(&[], &[]))
        .expect_err("a later explicit invocation may start a new worker");
    assert!(error.to_string().contains("outcome is uncertain"));
    assert!(worker_error(&error).uncertain_effect);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("effects"))
            .expect("fixture recorded second explicit effect")
            .lines()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("launches"))
            .expect("fixture recorded both explicit launches")
            .lines()
            .count(),
        2
    );
    for line in std::fs::read_to_string(directory.path().join("launches"))
        .expect("fixture recorded both explicit launches")
        .lines()
    {
        assert_reaped(line.parse().expect("numeric worker pid"));
    }
}

#[test]
fn parallel_elements_charge_completed_worker_output_to_group_budget() {
    let _guard = lock_worker_tests();
    let directory = TempDir::new().unwrap();
    let launch_marker = directory.path().join("launches");
    let effect_marker = directory.path().join("effects");
    let manifest_source = serde_json::json!({
        "name": "worker.fixture",
        "binary": python3().to_string_lossy(),
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "json_lines_worker",
            "lifecycle": {
                "kind": "json_lines",
                "startup_arguments": [fixture().to_string_lossy(), "--worker", "large_output_effect", launch_marker.to_string_lossy(), effect_marker.to_string_lossy()]
            }
        }
    });
    let registry = manifest::Registry::from_sources(&[manifest_source.to_string()]).unwrap();
    let options = runtime::RunOptions {
        max_output_bytes: 4096,
        command_timeout: Some(Duration::from_secs(5)),
        ..runtime::RunOptions::new(directory.path().to_path_buf())
    };
    let items = (0..20).map(|_| "\"x\"").collect::<Vec<_>>().join(", ");
    let program = parser::parse(&format!(
        "parallel for each item in [{items}] at most 1 as results\n  run worker.fixture\n"
    ))
    .unwrap();
    let error = runtime::run(&program, &registry, &options)
        .err()
        .expect("group must exceed the shared output budget");
    assert!(format!("{error:#}").contains("parallel for each output exceeded"));
    assert_eq!(
        std::fs::read_to_string(effect_marker)
            .unwrap()
            .lines()
            .count(),
        17
    );
}
