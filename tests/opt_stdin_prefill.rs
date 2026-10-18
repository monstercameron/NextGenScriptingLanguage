#![cfg(unix)]
//! Standalone integration test source for the round-14 stdin-prefill experiment.
//! Copy to `tests/round14_stdin_prefill.rs` and run before applying the runtime
//! patch; all behavioral cases use only the public `run_with_context` API.

use flow::{
    manifest::Registry,
    parser,
    runtime::{self, RunOptions, RunResult},
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    hint::black_box,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn registry(directory: &Path, binary: &str) -> Registry {
    let manifests = directory.join("manifests");
    fs::create_dir_all(&manifests).unwrap();
    let raw = json!({
        "name": "worker",
        "binary": binary,
        "accepts_stdin": true,
        "arguments": []
    });
    fs::write(manifests.join("worker.json"), raw.to_string()).unwrap();
    Registry::load(&manifests).unwrap()
}

fn executable(directory: &Path, name: &str, script: &str) -> std::path::PathBuf {
    let path = directory.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn run(
    directory: &Path,
    registry: &Registry,
    input: Vec<u8>,
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
) -> anyhow::Result<RunResult> {
    let program = parser::parse("then worker\n")?;
    runtime::run_with_context(
        &program,
        registry,
        &RunOptions {
            max_output_bytes: 4 * 1024 * 1024,
            command_timeout: Some(timeout),
            ..RunOptions::new(directory.to_path_buf())
        },
        BTreeMap::new(),
        input,
        cancelled,
    )
}

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

fn pipe_payload_sizes() -> Vec<usize> {
    let mut sizes = vec![
        512,
        513,
        4 * 1024,
        8 * 1024,
        16 * 1024,
        64 * 1024,
        64 * 1024 + 1,
        2 * 1024 * 1024,
        libc::PIPE_BUF,
        libc::PIPE_BUF + 1,
    ];
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

#[derive(Clone, Copy, Default)]
struct SelfUsage {
    user_cpu_ns: u64,
    system_cpu_ns: u64,
    voluntary_context_switches: u64,
    involuntary_context_switches: u64,
}

fn self_usage() -> SelfUsage {
    let mut raw = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(
        // SAFETY: `raw.as_mut_ptr()` points to the local MaybeUninit<rusage> above, which is
        // correctly sized and aligned for libc::rusage and writable. RUSAGE_SELF is a valid
        // who-argument, and getrusage only writes through the pointer.
        unsafe { libc::getrusage(libc::RUSAGE_SELF, raw.as_mut_ptr()) },
        0,
        "getrusage(RUSAGE_SELF) failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the assert above panics unless getrusage returned 0, and a 0 return means the
    // kernel filled the rusage at `raw`, so the MaybeUninit is fully initialized here.
    let raw = unsafe { raw.assume_init() };
    let timeval_ns = |value: &libc::timeval| {
        (value.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add((value.tv_usec as u64).saturating_mul(1_000))
    };
    SelfUsage {
        user_cpu_ns: timeval_ns(&raw.ru_utime),
        system_cpu_ns: timeval_ns(&raw.ru_stime),
        voluntary_context_switches: raw.ru_nvcsw as u64,
        involuntary_context_switches: raw.ru_nivcsw as u64,
    }
}

impl SelfUsage {
    fn delta_since(self, earlier: SelfUsage) -> SelfUsage {
        SelfUsage {
            user_cpu_ns: self.user_cpu_ns.saturating_sub(earlier.user_cpu_ns),
            system_cpu_ns: self.system_cpu_ns.saturating_sub(earlier.system_cpu_ns),
            voluntary_context_switches: self
                .voluntary_context_switches
                .saturating_sub(earlier.voluntary_context_switches),
            involuntary_context_switches: self
                .involuntary_context_switches
                .saturating_sub(earlier.involuntary_context_switches),
        }
    }

    fn add(&mut self, other: SelfUsage) {
        self.user_cpu_ns = self.user_cpu_ns.saturating_add(other.user_cpu_ns);
        self.system_cpu_ns = self.system_cpu_ns.saturating_add(other.system_cpu_ns);
        self.voluntary_context_switches = self
            .voluntary_context_switches
            .saturating_add(other.voluntary_context_switches);
        self.involuntary_context_switches = self
            .involuntary_context_switches
            .saturating_add(other.involuntary_context_switches);
    }
}

fn benchmark_payload_sizes() -> Vec<usize> {
    match std::env::var("ROUND14_STDIN_PAYLOAD_BYTES") {
        Ok(raw) => {
            let size = raw
                .parse::<usize>()
                .expect("ROUND14_STDIN_PAYLOAD_BYTES must be an integer");
            assert!(
                size <= 2 * 1024 * 1024,
                "ROUND14_STDIN_PAYLOAD_BYTES must be at most 2097152"
            );
            vec![size]
        }
        Err(std::env::VarError::NotPresent) => pipe_payload_sizes(),
        Err(error) => panic!("cannot read ROUND14_STDIN_PAYLOAD_BYTES: {error}"),
    }
}

#[test]
fn stdin_payload_sizes_round_trip_through_cat() {
    let directory = tempfile::tempdir().unwrap();
    let registry = registry(directory.path(), "/bin/cat");

    for size in pipe_payload_sizes() {
        let input = payload(size);
        let expected = input.clone();
        let result = run(
            directory.path(),
            &registry,
            input,
            Duration::from_secs(10),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(result.output, expected, "payload size {size}");
    }
}

#[test]
fn moderately_sized_input_is_safe_when_child_ignores_stdin() {
    let directory = tempfile::tempdir().unwrap();
    executable(
        directory.path(),
        "ignore-input",
        "#!/bin/sh\nprintf 'done\\n'\n",
    );
    let registry = registry(directory.path(), "./ignore-input");
    let result = run(
        directory.path(),
        &registry,
        payload(64 * 1024),
        Duration::from_secs(5),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(result.output, b"done\n");
}

fn sleeping_worker(directory: &Path) -> Registry {
    executable(
        directory,
        "sleeping-worker",
        "#!/bin/sh\nprintf '%s' \"$$\" > pid\nexec 1>&-\nexec /bin/sleep 30\n",
    );
    registry(directory, "./sleeping-worker")
}

fn assert_worker_reaped(directory: &Path) {
    let pid: libc::pid_t = fs::read_to_string(directory.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        // SAFETY: signal 0 sends nothing and takes only an integer. `pid` is the worker's `$$`
        // that the test's own shell script wrote to the `pid` file, parsed above.
        unsafe { libc::kill(pid, 0) },
        -1,
        "worker leaked after stdin handling"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn cancellation_reaps_child_during_moderate_input_command() {
    let directory = tempfile::tempdir().unwrap();
    let registry = sleeping_worker(directory.path());
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancelled.clone();
    let pid_file = directory.path().join("pid");
    let cancellation_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !pid_file.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        cancel_worker.store(true, Ordering::Relaxed);
    });

    let error = run(
        directory.path(),
        &registry,
        payload(64 * 1024),
        Duration::from_secs(5),
        cancelled,
    )
    .err()
    .expect("cancelled workflow should fail");
    cancellation_thread.join().unwrap();
    assert!(
        format!("{error:#}").contains("workflow cancelled"),
        "{error:#}"
    );
    assert_worker_reaped(directory.path());
}

#[test]
fn timeout_reaps_child_during_moderate_input_command() {
    let directory = tempfile::tempdir().unwrap();
    let registry = sleeping_worker(directory.path());
    let error = run(
        directory.path(),
        &registry,
        payload(64 * 1024),
        Duration::from_secs(1),
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .expect("timed out workflow should fail");
    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    assert_worker_reaped(directory.path());
}

#[test]
#[ignore = "manual raw run_with_context stdin payload latency samples"]
fn benchmark_run_with_context_cat_payloads() {
    let sample_count = std::env::var("ROUND14_STDIN_SAMPLES")
        .ok()
        .map(|raw| {
            raw.parse::<usize>()
                .expect("ROUND14_STDIN_SAMPLES must be an integer")
        })
        .unwrap_or(5);
    assert!(sample_count > 0, "sample count must be positive");

    let directory = tempfile::tempdir().unwrap();
    let registry = registry(directory.path(), "/bin/cat");
    let program = parser::parse("then worker\n").unwrap();
    let options = RunOptions {
        max_output_bytes: 4 * 1024 * 1024,
        command_timeout: Some(Duration::from_secs(10)),
        ..RunOptions::new(directory.path().to_path_buf())
    };
    runtime::validate_with_inputs(&program, &registry, BTreeSet::new(), true).unwrap();

    let mut measurements = Vec::new();
    for size in benchmark_payload_sizes() {
        let expected = payload(size);
        // Own each input before timing so the sample covers run_with_context,
        // process execution, and output collection, not test-side input cloning.
        let mut inputs = (0..sample_count)
            .map(|_| expected.clone())
            .collect::<Vec<_>>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut raw_samples_ns = Vec::with_capacity(sample_count);
        let mut usage = SelfUsage::default();

        for input in inputs.drain(..) {
            let bindings = BTreeMap::new();
            let cancelled = cancelled.clone();
            let usage_before = self_usage();
            let started = Instant::now();
            let result = runtime::run_with_context(
                &program,
                &registry,
                &options,
                bindings,
                black_box(input),
                cancelled,
            )
            .unwrap();
            let elapsed_ns = started.elapsed().as_nanos();
            let sample_usage = self_usage().delta_since(usage_before);
            raw_samples_ns.push(elapsed_ns);
            usage.add(sample_usage);
            assert_eq!(result.output, expected, "payload size {size}");
        }
        drop(inputs);
        measurements.push(json!({
            "payload_bytes": size,
            "raw_samples_ns": raw_samples_ns,
            "self_usage_delta": {
                "user_cpu_ns": usage.user_cpu_ns,
                "system_cpu_ns": usage.system_cpu_ns,
                "voluntary_context_switches": usage.voluntary_context_switches,
                "involuntary_context_switches": usage.involuntary_context_switches,
            },
        }));
    }

    println!(
        "ROUND14_STDIN_BENCH_JSON={}",
        serde_json::to_string(&json!({
            "measurement": "raw single-call run_with_context timings for a cat worker; includes process launch/wait and output collection",
            "sample_count_per_payload": sample_count,
            "payload_selection": "ROUND14_STDIN_PAYLOAD_BYTES selects one payload size in bytes, at most 2097152; unset runs the size matrix",
            "resource_scope": "aggregate per-call getrusage(RUSAGE_SELF) deltas; excludes child-process CPU and output assertions",
            "warmup": "none; samples are emitted in execution order without filtering",
            "payloads": measurements,
        }))
        .unwrap()
    );
}
