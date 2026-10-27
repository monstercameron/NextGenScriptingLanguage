//! Bounded persistent workers for applets that implement the JSON-lines protocol.
//!
//! A worker is keyed by canonical applet name, executable path/content,
//! capability grant, working directory, environment snapshot, and startup
//! arguments. Each checked-out worker handles one request at a time. The pool
//! owns every child from spawn through kill/reap, including idle children
//! retained between calls. Frame limits count the terminating line feed and
//! apply to both directions.

#[cfg(any(unix, windows))]
use anyhow::Context;
use anyhow::{bail, Result};
#[cfg(any(unix, windows))]
use serde::{Deserialize, Serialize};
#[cfg(any(unix, windows))]
use sha2::{Digest, Sha256};
#[cfg(any(unix, windows))]
use std::collections::BTreeMap;
use std::collections::HashMap;
#[cfg(any(unix, windows))]
use std::collections::HashSet;
use std::error::Error as StdError;
#[cfg(any(unix, windows))]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt;
#[cfg(any(unix, windows))]
use std::fs::File;
#[cfg(any(unix, windows))]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsHandle;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
#[cfg(any(unix, windows))]
use std::sync::atomic::Ordering;
#[cfg(any(unix, windows))]
use std::sync::Condvar;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_POOL_CAPACITY: usize = 16;
/// Upper bound for an explicitly configured worker pool.
pub const MAX_POOL_CAPACITY: usize = 1024;
#[cfg(any(unix, windows))]
const MAX_POOL_WAIT: Duration = Duration::from_secs(30);
#[cfg(any(unix, windows))]
const IO_POLL_INTERVAL: Duration = Duration::from_millis(20);
#[cfg(any(unix, windows))]
const IO_CHUNK_BYTES: usize = 8192;

/// Spawn configuration for one applet worker identity.
///
/// `environment` is the complete environment snapshot inherited by the
/// worker, rather than an overlay. Use [`WorkerLaunch::inherited`] to capture
/// the current process environment consistently.
#[derive(Clone)]
pub(crate) struct WorkerLaunch {
    #[cfg(any(unix, windows))]
    pub registry: crate::manifest::Registry,
    pub applet_name: String,
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub environment: Vec<(OsString, OsString)>,
    policy_fingerprint: Option<[u8; 32]>,
    pub startup_arguments: Vec<OsString>,
    #[cfg(any(unix, windows))]
    pub protocol_version: u32,
    #[cfg(any(unix, windows))]
    pub timeout: Duration,
    #[cfg(any(unix, windows))]
    pub max_frame_bytes: usize,
    #[cfg(any(unix, windows))]
    pub max_stderr_bytes: usize,
}

/// The lifecycle limits a worker is launched under.
pub(crate) struct WorkerLimits {
    pub(crate) protocol_version: u32,
    pub(crate) timeout: Duration,
    pub(crate) max_frame_bytes: usize,
    pub(crate) max_stderr_bytes: usize,
}

impl WorkerLaunch {
    pub(crate) fn inherited(
        registry: crate::manifest::Registry,
        applet_name: String,
        binary: PathBuf,
        cwd: PathBuf,
        startup_arguments: Vec<OsString>,
        limits: WorkerLimits,
    ) -> Result<Self> {
        let WorkerLimits {
            protocol_version,
            timeout,
            max_frame_bytes,
            max_stderr_bytes,
        } = limits;
        let policy_fingerprint = crate::capabilities::policy_fingerprint(&registry, &applet_name)?;
        let environment = crate::capabilities::effective_environment(&registry, &applet_name)?;
        if crate::capabilities::policy_fingerprint(&registry, &applet_name)? != policy_fingerprint {
            bail!("applet capability policy changed while preparing worker launch");
        }
        #[cfg(not(any(unix, windows)))]
        let _ = (protocol_version, timeout, max_frame_bytes, max_stderr_bytes);
        Ok(Self {
            #[cfg(any(unix, windows))]
            registry,
            applet_name,
            binary,
            cwd,
            environment,
            policy_fingerprint,
            startup_arguments,
            #[cfg(any(unix, windows))]
            protocol_version,
            #[cfg(any(unix, windows))]
            timeout,
            #[cfg(any(unix, windows))]
            max_frame_bytes,
            #[cfg(any(unix, windows))]
            max_stderr_bytes,
        })
    }
}

/// One applet invocation. The deadline is supplied by the surrounding native
/// pipeline; the worker lifecycle timeout is applied in addition to it.
#[derive(Clone, Debug)]
#[cfg_attr(windows, allow(dead_code))]
pub(crate) struct WorkerRequest {
    pub arguments: Vec<String>,
    pub input: Vec<u8>,
    /// Values mapped through JSON argument nodes in v2 worker requests.
    pub value: Option<serde_json::Value>,
    /// JSON encoded outputs use the v2 `value` member instead of output bytes.
    pub json_output: bool,
    pub max_output_bytes: usize,
    /// Immutable sensitivity floor for the enclosing runtime invocation.
    /// The worker pool checks this against retained state before reusing a
    /// process and records it before transmitting the request.
    pub sensitivity: crate::security::SensitivitySet,
    pub deadline: Option<Instant>,
    pub cancelled: Arc<AtomicBool>,
    /// Set as soon as this invocation launches a worker, even if the
    /// invocation later fails with an uncertain result.
    pub spawned: Arc<AtomicBool>,
}

/// The worker's native-style result. `output` and `stderr` preserve their
/// bytes; the status is the exact signed integer returned in the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerResponse {
    pub status: i32,
    pub output: Vec<u8>,
    pub typed_json: Option<serde_json::Value>,
    pub stderr: Vec<u8>,
    /// True when this explicit call launched the helper process.
    pub spawned_process: bool,
}

/// A worker invocation failure with explicit effect certainty.
///
/// Runtime callers may downcast the `anyhow::Error` chain to this type. A
/// transmitted request without a complete, matching response is uncertain;
/// a rejected complete response retains its known status.
#[derive(Debug)]
pub(crate) struct WorkerError {
    pub kind: WorkerErrorKind,
    pub uncertain_effect: bool,
    pub completed_status: Option<i32>,
    /// Captured bytes from a complete valid response, when one was received.
    /// `Some(Vec::new())` means a complete empty stdout was captured; `None`
    /// means no complete valid response was available.
    pub captured_stdout: Option<Vec<u8>>,
    /// Captured diagnostics with the same `None` versus empty distinction.
    pub captured_stderr: Option<Vec<u8>>,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))]
pub(crate) enum WorkerErrorKind {
    Timeout,
    Cancelled,
    SpawnError,
    TransportError,
    ProtocolError,
    OutputLimit,
    IoError,
    CapacityError,
    UnknownError,
}

impl WorkerError {
    fn new(
        kind: WorkerErrorKind,
        uncertain_effect: bool,
        completed_status: Option<i32>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            uncertain_effect,
            completed_status,
            captured_stdout: None,
            captured_stderr: None,
            message: message.into(),
        }
    }

    #[cfg(any(unix, windows))]
    fn with_captured_response(mut self, stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        self.captured_stdout = Some(stdout);
        self.captured_stderr = Some(stderr);
        self
    }
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for WorkerError {}

#[cfg(any(unix, windows))]
fn as_worker_error(error: anyhow::Error, fallback: WorkerErrorKind) -> anyhow::Error {
    if error.downcast_ref::<WorkerError>().is_some() {
        error
    } else {
        WorkerError::new(fallback, false, None, format!("{error:#}")).into()
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct WorkerKey {
    applet_name: String,
    binary: PathBuf,
    content_identity: [u8; 32],
    protocol_version: u32,
    policy_fingerprint: Option<[u8; 32]>,
    cwd: PathBuf,
    environment: Vec<(OsString, OsString)>,
    startup_arguments: Vec<OsString>,
}

impl fmt::Debug for WorkerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerKey")
            .field("applet_name", &self.applet_name)
            .field("binary", &self.binary)
            .field("content_identity", &self.content_identity)
            .field("protocol_version", &self.protocol_version)
            .field("policy_fingerprint", &self.policy_fingerprint)
            .field("cwd", &self.cwd)
            .field(
                "environment_names",
                &self
                    .environment
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("startup_arguments", &self.startup_arguments)
            .finish()
    }
}

impl WorkerKey {
    fn matches_launch_metadata(
        &self,
        launch: &WorkerLaunch,
        environment: &[(OsString, OsString)],
    ) -> bool {
        self.applet_name == launch.applet_name
            && self.binary == launch.binary
            && self.protocol_version == launch.protocol_version
            && self.policy_fingerprint == launch.policy_fingerprint
            && self.cwd == launch.cwd
            && self.environment == environment
            && self.startup_arguments == launch.startup_arguments
    }

    #[cfg(any(unix, windows))]
    fn from_launch(
        launch: &WorkerLaunch,
        deadline: Option<Instant>,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        validate_launch(launch)?;
        let content_identity = binary_identity(&launch.binary, deadline, cancelled)?;
        let mut environment = launch.environment.clone();
        environment.sort_by(|left, right| left.0.cmp(&right.0));
        for pair in environment.windows(2) {
            if pair[0].0 == pair[1].0 {
                bail!("persistent worker environment contains duplicate keys");
            }
        }
        Ok(Self {
            applet_name: launch.applet_name.clone(),
            binary: launch.binary.clone(),
            content_identity,
            protocol_version: launch.protocol_version,
            policy_fingerprint: launch.policy_fingerprint,
            cwd: launch.cwd.clone(),
            environment,
            startup_arguments: launch.startup_arguments.clone(),
        })
    }
}

#[cfg(any(unix, windows))]
struct WorkerSlot {
    id: u64,
    key: WorkerKey,
    worker: Option<WorkerProcess>,
    busy: bool,
}

#[cfg(any(unix, windows))]
#[derive(Default)]
struct PoolState {
    slots: Vec<WorkerSlot>,
    next_id: u64,
}

/// A fixed-size cache of persistent worker processes.
///
/// The pool is suitable for `Registry::host_state`: cloned registries share
/// the same pool and dropping the last registry reference drops the pool,
/// which kills and reaps all retained workers.
pub struct WorkerPool {
    capacity: usize,
    #[cfg(any(unix, windows))]
    state: Mutex<PoolState>,
    sensitivity_history: Mutex<HashMap<WorkerKey, crate::security::SensitivitySet>>,
    #[cfg(any(unix, windows))]
    changed: Condvar,
}

impl Default for WorkerPool {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_POOL_CAPACITY).expect("default worker pool capacity is valid")
    }
}

impl WorkerPool {
    /// Create a pool with an explicit worker process budget.
    ///
    /// The default pool retains at most 16 workers. Custom capacities must be
    /// between 1 and [`MAX_POOL_CAPACITY`]. Workers are not evicted to make room
    /// for a new identity; callers should choose a budget that fits their
    /// registry's applet launch identities. Install a custom pool with
    /// `Registry::set_host_state` before the first worker invocation.
    pub fn with_capacity(capacity: usize) -> Result<Self> {
        if capacity == 0 || capacity > MAX_POOL_CAPACITY {
            bail!("worker pool capacity must be between 1 and {MAX_POOL_CAPACITY}");
        }
        Ok(Self {
            capacity,
            #[cfg(any(unix, windows))]
            state: Mutex::new(PoolState::default()),
            sensitivity_history: Mutex::new(HashMap::new()),
            #[cfg(any(unix, windows))]
            changed: Condvar::new(),
        })
    }

    /// Create an empty execution-scoped pool with this pool's validated
    /// capacity. Service sessions use this to honor registry configuration
    /// without retaining workers or sensitivity history beyond the service.
    #[cfg(feature = "graph")]
    pub(crate) fn isolated_like(&self) -> Self {
        Self::with_capacity(self.capacity).expect("existing worker pool capacity is valid")
    }

    pub(crate) const fn supported() -> bool {
        cfg!(any(unix, windows))
    }

    /// Read retained labels for matching launch metadata without hashing the
    /// executable. This metadata lock is independent of worker acquisition,
    /// so workflow preflight never waits for a slow request to finish. The
    /// dispatch path later narrows this conservative set to the exact key.
    pub(crate) fn sensitivity_history_for_launch(
        &self,
        launch: &WorkerLaunch,
    ) -> Result<crate::security::SensitivitySet> {
        let mut environment = launch.environment.clone();
        environment.sort_by(|left, right| left.0.cmp(&right.0));
        let history = self
            .sensitivity_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if history.is_empty() {
            return Ok(crate::security::SensitivitySet::empty());
        }

        // Preflight has no invocation budget for hashing executable contents.
        // Match every retained worker with the same bounded launch metadata
        // and conservatively join histories across binary-content variants.
        // Dispatch computes the full key under the request's deadline and
        // checks that exact process history before transmission.
        let mut sensitivities = crate::security::SensitivitySet::empty();
        for (key, retained) in history.iter() {
            if key.matches_launch_metadata(launch, &environment) {
                sensitivities.extend(retained);
            }
        }
        Ok(sensitivities)
    }

    /// Send one invocation to a persistent JSON-lines worker.
    ///
    /// Any framing, cancellation, timeout, or process failure after request
    /// bytes have been written is reported as uncertain. The failed worker is
    /// killed and reaped; this method never retries an invocation.
    pub(crate) fn invoke(
        &self,
        launch: WorkerLaunch,
        request: WorkerRequest,
    ) -> Result<WorkerResponse> {
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (launch, request);
            Err(WorkerError::new(
                WorkerErrorKind::UnknownError,
                false,
                None,
                "persistent JSON-lines applet workers are not supported on this platform",
            )
            .into())
        }

        #[cfg(any(unix, windows))]
        {
            if !Self::supported() {
                return Err(WorkerError::new(
                    WorkerErrorKind::UnknownError,
                    false,
                    None,
                    "persistent JSON-lines applet workers are not supported on this platform",
                )
                .into());
            }

            let started = Instant::now();
            let worker_deadline = started
                .checked_add(launch.timeout)
                .context("worker timeout is too large")?;
            let deadline = min_deadline(request.deadline, Some(worker_deadline));
            check_cancel_deadline(&request.cancelled, deadline)?;
            let request_frame = validate_request(&launch, &request)
                .map_err(|error| as_worker_error(error, WorkerErrorKind::UnknownError))?;
            ensure_policy_unchanged(&launch)
                .map_err(|error| as_worker_error(error, WorkerErrorKind::UnknownError))?;

            let key = WorkerKey::from_launch(&launch, deadline, &request.cancelled)
                .map_err(|error| as_worker_error(error, WorkerErrorKind::IoError))?;
            let mut lease = self.acquire(key.clone(), &request.cancelled, deadline)?;
            if let Err(error) = check_cancel_deadline(&request.cancelled, deadline) {
                if lease.worker.is_none() {
                    lease.invalidate();
                }
                return Err(error);
            }

            if let Some(worker) = lease.worker.as_mut() {
                match worker.probe_idle(&request.cancelled, deadline) {
                    Ok(true) => {}
                    Ok(false) => {
                        // A previous request completed, but this worker exited
                        // before the current invocation began. Starting a new
                        // helper here is a new explicit request, not a replay.
                        drop(lease.worker.take());
                        self.reset_sensitivity_history(&key);
                    }
                    Err(error) => {
                        lease.invalidate();
                        let kind = error
                            .downcast_ref::<WorkerError>()
                            .map(|worker_error| worker_error.kind)
                            .unwrap_or(WorkerErrorKind::UnknownError);
                        return Err(WorkerError::new(
                            kind,
                            false,
                            None,
                            format!("checking idle persistent worker: {error:#}"),
                        )
                        .into());
                    }
                }
            }

            let retained_sensitivities = self.sensitivity_history_for_key(&key);
            if retained_sensitivities
                .iter()
                .any(|label| !request.sensitivity.contains(&label))
            {
                return Err(WorkerError::new(
                    WorkerErrorKind::UnknownError,
                    false,
                    None,
                    format!(
                        "persistent applet worker retains sensitivities {:?} that are absent from the current workflow floor; request was not started",
                        retained_sensitivities.iter().collect::<Vec<_>>()
                    ),
                )
                .into());
            }

            let mut spawned_process = false;
            if lease.worker.is_none() {
                match WorkerProcess::spawn(&launch, &request.spawned) {
                    Ok(worker) => {
                        lease.worker = Some(worker);
                        spawned_process = true;
                    }
                    Err(error) => {
                        lease.invalidate();
                        return Err(WorkerError::new(
                            WorkerErrorKind::SpawnError,
                            false,
                            None,
                            format!("starting persistent applet worker: {error:#}"),
                        )
                        .into());
                    }
                }
            }
            if let Err(error) = check_cancel_deadline(&request.cancelled, deadline) {
                lease.invalidate();
                return Err(error);
            }
            if let Err(error) = ensure_policy_unchanged(&launch) {
                lease.invalidate();
                return Err(as_worker_error(error, WorkerErrorKind::UnknownError));
            }

            // Publish labels while this request owns the serialized slot so
            // concurrent workflow preflight can account for state about to be
            // sent to the worker.
            self.extend_sensitivity_history(&key, &request.sensitivity);

            let worker = lease
                .worker
                .as_mut()
                .expect("worker is spawned before exchange");
            match worker.exchange(&request, request_frame, launch.max_frame_bytes, deadline) {
                Ok(mut response) => {
                    let raw_stderr_bytes = response.stderr.len();
                    if raw_stderr_bytes > launch.max_stderr_bytes {
                        return Err(WorkerError::new(
                            WorkerErrorKind::OutputLimit,
                            false,
                            Some(response.status),
                            format!(
                                "persistent applet worker stderr was {} bytes, exceeding the maximum of {} bytes; request completed with status {} and may have effects",
                                raw_stderr_bytes,
                                launch.max_stderr_bytes,
                                response.status
                            ),
                        )
                        .with_captured_response(response.output, response.stderr)
                        .into());
                    }
                    if response.output.len() > request.max_output_bytes {
                        return Err(WorkerError::new(
                            WorkerErrorKind::OutputLimit,
                            false,
                            Some(response.status),
                            format!(
                                "persistent applet worker returned {} output bytes, exceeding the maximum of {} bytes; request completed with status {} and may have effects",
                                response.output.len(),
                                request.max_output_bytes,
                                response.status
                            ),
                        )
                        .with_captured_response(response.output, response.stderr)
                        .into());
                    }
                    response.spawned_process = spawned_process;
                    Ok(response)
                }
                Err(failure) => {
                    lease.invalidate();
                    let message = if failure.transmitted {
                        format!(
                            "persistent applet worker failed after request transmission; outcome is uncertain and may have effects; automatic retry is unsafe: {}",
                            failure.message
                        )
                    } else {
                        failure.message
                    };
                    Err(WorkerError::new(failure.kind, failure.transmitted, None, message).into())
                }
            }
        }
    }

    #[cfg(any(unix, windows))]
    fn acquire(
        &self,
        key: WorkerKey,
        cancelled: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<WorkerLease<'_>> {
        let pool_deadline = min_deadline(deadline, Instant::now().checked_add(MAX_POOL_WAIT));
        loop {
            check_cancel_deadline(cancelled, pool_deadline)
                .context("waiting for persistent worker pool capacity")?;
            let mut state = self.lock_state();

            if let Some(index) = state.slots.iter().position(|slot| slot.key == key) {
                if state.slots[index].busy {
                    let wait = pool_deadline
                        .map(|until| until.saturating_duration_since(Instant::now()))
                        .unwrap_or(MAX_POOL_WAIT)
                        .min(IO_POLL_INTERVAL);
                    let (guard, _) = self
                        .changed
                        .wait_timeout(state, wait)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(guard);
                    continue;
                }
                let id = state.slots[index].id;
                state.slots[index].busy = true;
                let worker = state.slots[index].worker.take();
                return Ok(WorkerLease::new(self, id, worker));
            }

            if state.slots.len() < self.capacity {
                let id = next_slot_id(&mut state);
                state.slots.push(WorkerSlot {
                    id,
                    key: key.clone(),
                    worker: None,
                    busy: true,
                });
                return Ok(WorkerLease::new(self, id, None));
            }

            return Err(WorkerError::new(
                WorkerErrorKind::CapacityError,
                false,
                None,
                format!(
                    "persistent worker pool capacity {} is full; request was not started",
                    self.capacity
                ),
            )
            .into());
        }
    }

    #[cfg(any(unix, windows))]
    fn lock_state(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(any(unix, windows))]
    fn sensitivity_history_for_key(&self, key: &WorkerKey) -> crate::security::SensitivitySet {
        self.sensitivity_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
            .unwrap_or_else(crate::security::SensitivitySet::empty)
    }

    #[cfg(any(unix, windows))]
    fn extend_sensitivity_history(
        &self,
        key: &WorkerKey,
        sensitivities: &crate::security::SensitivitySet,
    ) {
        let mut history = self
            .sensitivity_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        history
            .entry(key.clone())
            .or_insert_with(crate::security::SensitivitySet::empty)
            .extend(sensitivities);
    }

    #[cfg(any(unix, windows))]
    fn reset_sensitivity_history(&self, key: &WorkerKey) {
        self.sensitivity_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
    }

    #[cfg(any(unix, windows))]
    fn release(&self, id: u64, worker: Option<WorkerProcess>, retain: bool) {
        if retain {
            let mut retired = None;
            {
                let mut state = self.lock_state();
                if let Some(index) = state.slots.iter().position(|slot| slot.id == id) {
                    state.slots[index].worker = worker;
                    state.slots[index].busy = false;
                } else {
                    retired = worker;
                }
                self.changed.notify_one();
            }
            drop(retired);
            return;
        }

        // Keep the slot busy until its process is destroyed and its history
        // removed. A new identity-equivalent process must never inherit old
        // labels, and preflight must never miss labels for a live process.
        let key = self
            .lock_state()
            .slots
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| slot.key.clone());
        drop(worker);
        if let Some(key) = key {
            self.reset_sensitivity_history(&key);
        }
        let mut state = self.lock_state();
        if let Some(index) = state.slots.iter().position(|slot| slot.id == id) {
            state.slots.remove(index);
        }
        self.changed.notify_one();
    }
}

#[cfg(any(unix, windows))]
fn next_slot_id(state: &mut PoolState) -> u64 {
    loop {
        state.next_id = state.next_id.checked_add(1).unwrap_or(1).max(1);
        if !state.slots.iter().any(|slot| slot.id == state.next_id) {
            return state.next_id;
        }
    }
}

#[cfg(any(unix, windows))]
struct WorkerLease<'a> {
    pool: &'a WorkerPool,
    id: u64,
    worker: Option<WorkerProcess>,
    retain: bool,
}

#[cfg(any(unix, windows))]
impl<'a> WorkerLease<'a> {
    fn new(pool: &'a WorkerPool, id: u64, worker: Option<WorkerProcess>) -> Self {
        Self {
            pool,
            id,
            worker,
            retain: true,
        }
    }

    fn invalidate(&mut self) {
        self.retain = false;
    }
}

#[cfg(any(unix, windows))]
impl Drop for WorkerLease<'_> {
    fn drop(&mut self) {
        self.pool.release(self.id, self.worker.take(), self.retain);
    }
}

#[derive(Serialize)]
#[cfg(any(unix, windows))]
struct RequestFrame<'a> {
    id: u64,
    arguments: &'a [String],
    input: &'a [u8],
}

#[derive(Serialize)]
#[cfg(any(unix, windows))]
struct RequestFrameV2<'a> {
    version: u32,
    id: u64,
    arguments: &'a [String],
    input_base64: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a serde_json::Value>,
    json_output: bool,
}

#[derive(Deserialize)]
#[cfg(any(unix, windows))]
#[serde(deny_unknown_fields)]
struct ResponseFrame {
    id: u64,
    status: i32,
    output: Vec<u8>,
    #[serde(default)]
    stderr: Option<String>,
}

#[cfg(any(unix, windows))]
fn parse_worker_json_strict(bytes: &[u8]) -> Result<serde_json::Value> {
    #[cfg(test)]
    STRICT_JSON_PARSE_CALLS.with(|calls| calls.set(calls.get() + 1));
    use serde::Deserialize;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = WorkerJsonValue::deserialize(&mut deserializer).context("parsing worker JSON")?;
    deserializer.end().context("parsing worker JSON")?;
    Ok(value.into_json())
}

#[cfg(test)]
thread_local! {
    static STRICT_JSON_PARSE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[allow(dead_code)] // Used by a unit test in the parent runtime module.
pub(crate) fn reset_strict_json_parse_calls() {
    STRICT_JSON_PARSE_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
#[allow(dead_code)] // Used by a unit test in the parent runtime module.
pub(crate) fn strict_json_parse_calls() -> usize {
    STRICT_JSON_PARSE_CALLS.with(std::cell::Cell::get)
}

#[cfg(any(unix, windows))]
enum WorkerJsonValue {
    Null,
    Boolean(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

#[cfg(any(unix, windows))]
impl WorkerJsonValue {
    fn into_json(self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Boolean(value) => serde_json::Value::Bool(value),
            Self::Number(value) => serde_json::Value::Number(value),
            Self::String(value) => serde_json::Value::String(value),
            Self::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(Self::into_json).collect())
            }
            Self::Object(values) => serde_json::Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, value.into_json()))
                    .collect(),
            ),
        }
    }
}

#[cfg(any(unix, windows))]
impl<'de> Deserialize<'de> for WorkerJsonValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct WorkerJsonVisitor;
        impl<'de> serde::de::Visitor<'de> for WorkerJsonVisitor {
            type Value = WorkerJsonValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value with unique object keys")
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WorkerJsonValue::Null)
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WorkerJsonValue::Null)
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WorkerJsonValue::Boolean(value))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WorkerJsonValue::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WorkerJsonValue::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(WorkerJsonValue::Number)
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WorkerJsonValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(WorkerJsonValue::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(WorkerJsonValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, WorkerJsonValue>()? {
                    if values.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate worker JSON object key {key:?}"
                        )));
                    }
                }
                Ok(WorkerJsonValue::Object(values))
            }
        }
        deserializer.deserialize_any(WorkerJsonVisitor)
    }
}

#[cfg(any(unix, windows))]
const WORKER_HANDSHAKE: &[u8] =
    b"{\"type\":\"handshake\",\"protocol\":\"ngsl.json-lines-worker\",\"versions\":[2]}\n";

#[cfg(unix)]
type WorkerOutput = std::process::ChildStdout;
#[cfg(windows)]
type WorkerOutput = crate::platform::windows::Pump;

#[cfg(any(unix, windows))]
struct WorkerProcess {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: WorkerOutput,
    #[cfg(windows)]
    job: crate::platform::windows::Job,
    next_request_id: u64,
    protocol_version: u32,
    negotiated: bool,
}

#[cfg(any(unix, windows))]
struct ExchangeFailure {
    kind: WorkerErrorKind,
    message: String,
    transmitted: bool,
}

#[cfg(any(unix, windows))]
impl WorkerProcess {
    fn spawn(launch: &WorkerLaunch, spawned: &AtomicBool) -> Result<Self> {
        use std::process::Stdio;

        ensure_policy_unchanged(launch)?;
        crate::security::capture_declared_environment_secrets(
            &launch.registry,
            &launch.applet_name,
            &launch.environment,
        )
        .context("registering declared worker environment secrets")?;
        let mut command = crate::capabilities::command_for(
            &launch.binary,
            launch.startup_arguments.iter(),
            &launch.registry,
            &launch.applet_name,
            &crate::working_dir::WorkingDirectory::plain(launch.cwd.clone()),
        )?;
        command
            .current_dir(&launch.cwd)
            .env_clear()
            .envs(launch.environment.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Worker applet diagnostics are returned inside the response frame.
            // Keep the transport process's own stderr from blocking the pipe.
            .stderr(Stdio::null());
        #[cfg(unix)]
        let (mut child, ()) = {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            let child = command.spawn().with_context(|| {
                format!("launching worker executable {}", launch.binary.display())
            })?;
            (child, ())
        };
        #[cfg(windows)]
        let (mut child, job) = crate::platform::windows::Job::spawn(&mut command)
            .with_context(|| format!("launching worker executable {}", launch.binary.display()))?;
        spawned.store(true, Ordering::Relaxed);
        let input = match child.stdin.take() {
            Some(input) => input,
            None => {
                #[cfg(unix)]
                let _ = UnreapedWorker { child: &mut child }.kill_group_and_reap();
                #[cfg(windows)]
                {
                    job.terminate();
                    let _ = child.wait();
                }
                bail!("worker stdin pipe was not created");
            }
        };
        let output = match child.stdout.take() {
            Some(output) => output,
            None => {
                #[cfg(unix)]
                let _ = UnreapedWorker { child: &mut child }.kill_group_and_reap();
                #[cfg(windows)]
                {
                    job.terminate();
                    let _ = child.wait();
                }
                bail!("worker stdout pipe was not created");
            }
        };
        #[cfg(unix)]
        if let Err(error) = set_nonblocking(&input).and_then(|()| set_nonblocking(&output)) {
            let _ = UnreapedWorker { child: &mut child }.kill_group_and_reap();
            return Err(error).context("configuring worker protocol pipes");
        }
        #[cfg(windows)]
        let output = crate::platform::windows::Pump::new(output);
        Ok(Self {
            child,
            input,
            output,
            #[cfg(windows)]
            job,
            next_request_id: 1,
            protocol_version: launch.protocol_version,
            negotiated: launch.protocol_version == 1,
        })
    }

    fn probe_idle(&mut self, cancelled: &AtomicBool, deadline: Option<Instant>) -> Result<bool> {
        #[cfg(unix)]
        {
            let _ = (cancelled, deadline);
            let mut byte = [0u8; 1];
            loop {
                match self.output.read(&mut byte) {
                    Ok(0) => return Ok(false),
                    Ok(_) => {
                        return Err(WorkerError::new(
                            WorkerErrorKind::ProtocolError,
                            false,
                            None,
                            "persistent worker wrote unsolicited bytes between requests",
                        )
                        .into());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        return Ok(true);
                    }
                    Err(error) => {
                        return Err(WorkerError::new(
                            WorkerErrorKind::IoError,
                            false,
                            None,
                            format!("checking idle worker stdout: {error}"),
                        )
                        .into());
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            let exited = match self.child.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(error) => {
                    return Err(WorkerError::new(
                        WorkerErrorKind::IoError,
                        false,
                        None,
                        format!("checking idle worker process: {error}"),
                    )
                    .into());
                }
            };
            if exited {
                // The direct worker cannot accept another request. Terminating
                // its Job also closes stdout inherited by any child processes;
                // drain one bounded read so already-written protocol violations
                // are reported before deciding to replace the idle worker.
                self.job.terminate();
            }
            let mut byte = [0u8; 1];
            loop {
                if exited {
                    check_cancel_deadline(cancelled, deadline)?;
                }
                match self.output.read(&mut byte) {
                    Ok(0) => return Ok(false),
                    Ok(_) => {
                        return Err(WorkerError::new(
                            WorkerErrorKind::ProtocolError,
                            false,
                            None,
                            "persistent worker wrote unsolicited bytes between requests",
                        )
                        .into());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if !exited {
                            return Ok(true);
                        }
                        wait_for_worker_output(&mut self.output, deadline);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                        return Ok(false);
                    }
                    Err(error) => {
                        return Err(WorkerError::new(
                            WorkerErrorKind::IoError,
                            false,
                            None,
                            format!("checking idle worker stdout: {error}"),
                        )
                        .into());
                    }
                }
            }
        }
    }

    fn exchange(
        &mut self,
        request: &WorkerRequest,
        mut frame: Vec<u8>,
        max_frame_bytes: usize,
        deadline: Option<Instant>,
    ) -> std::result::Result<WorkerResponse, ExchangeFailure> {
        if self.protocol_version == 2 && !self.negotiated {
            self.negotiate_v2(request, max_frame_bytes, deadline)?;
        }
        let id = self.next_request_id;
        let Some(next_id) = id.checked_add(1) else {
            return Err(ExchangeFailure {
                kind: WorkerErrorKind::UnknownError,
                message: "persistent worker request id space exhausted".into(),
                transmitted: false,
            });
        };
        patch_request_id(&mut frame, id).map_err(|error| ExchangeFailure {
            kind: WorkerErrorKind::ProtocolError,
            message: format!("patching persistent worker request id: {error}"),
            transmitted: false,
        })?;

        let transmitted = self.write_frame(frame, request, deadline)?;
        self.next_request_id = next_id;

        let response_bytes = match read_frame(
            &mut self.output,
            max_frame_bytes,
            &request.cancelled,
            deadline,
        ) {
            Ok(bytes) => bytes,
            Err(error) => return Err(exchange_failure(error, transmitted)),
        };
        let (response_id, status, output, typed_json, stderr) = if self.protocol_version == 1 {
            let response: ResponseFrame =
                serde_json::from_slice(&response_bytes).map_err(|error| ExchangeFailure {
                    kind: WorkerErrorKind::ProtocolError,
                    message: format!("invalid persistent worker JSON response: {error}"),
                    transmitted,
                })?;
            (
                response.id,
                response.status,
                response.output,
                None,
                response.stderr.unwrap_or_default().into_bytes(),
            )
        } else {
            let value =
                parse_worker_json_strict(&response_bytes).map_err(|error| ExchangeFailure {
                    kind: WorkerErrorKind::ProtocolError,
                    message: format!("invalid persistent worker v2 JSON response: {error}"),
                    transmitted,
                })?;
            let object = value.as_object().ok_or_else(|| ExchangeFailure {
                kind: WorkerErrorKind::ProtocolError,
                message: "persistent worker v2 response must be a JSON object".into(),
                transmitted,
            })?;
            let allowed = [
                "version",
                "id",
                "status",
                "output_base64",
                "value",
                "stderr",
            ];
            if object.keys().any(|key| !allowed.contains(&key.as_str())) {
                return Err(ExchangeFailure {
                    kind: WorkerErrorKind::ProtocolError,
                    message: "persistent worker v2 response contains an unknown field".into(),
                    transmitted,
                });
            }
            if object.get("version").and_then(serde_json::Value::as_u64) != Some(2) {
                return Err(ExchangeFailure {
                    kind: WorkerErrorKind::ProtocolError,
                    message: "persistent worker response did not use negotiated frame version 2"
                        .into(),
                    transmitted,
                });
            }
            let response_id = object
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| ExchangeFailure {
                    kind: WorkerErrorKind::ProtocolError,
                    message: "persistent worker v2 response id must be an unsigned integer".into(),
                    transmitted,
                })?;
            let status = object
                .get("status")
                .and_then(serde_json::Value::as_i64)
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(|| ExchangeFailure {
                    kind: WorkerErrorKind::ProtocolError,
                    message: "persistent worker v2 response status must be a signed 32-bit integer"
                        .into(),
                    transmitted,
                })?;
            let stderr = match object.get("stderr") {
                None | Some(serde_json::Value::Null) => Vec::new(),
                Some(serde_json::Value::String(value)) => value.as_bytes().to_vec(),
                _ => {
                    return Err(ExchangeFailure {
                        kind: WorkerErrorKind::ProtocolError,
                        message: "persistent worker v2 stderr must be a string".into(),
                        transmitted,
                    });
                }
            };
            let (output, typed_json) = if request.json_output {
                let Some(value) = object.get("value") else {
                    return Err(ExchangeFailure {
                        kind: WorkerErrorKind::ProtocolError,
                        message: "persistent worker v2 JSON output requires a value field".into(),
                        transmitted,
                    });
                };
                if object.contains_key("output_base64") {
                    return Err(ExchangeFailure {
                        kind: WorkerErrorKind::ProtocolError,
                        message:
                            "persistent worker v2 JSON output cannot also include output_base64"
                                .into(),
                        transmitted,
                    });
                }
                let mut output = serde_json::to_vec(value).map_err(|error| ExchangeFailure {
                    kind: WorkerErrorKind::ProtocolError,
                    message: format!("encoding persistent worker typed JSON output: {error}"),
                    transmitted,
                })?;
                output.push(b'\n');
                (output, Some(value.clone()))
            } else {
                if object.contains_key("value") {
                    return Err(ExchangeFailure {
                        kind: WorkerErrorKind::ProtocolError,
                        message: "persistent worker v2 byte output cannot include a value field"
                            .into(),
                        transmitted,
                    });
                }
                let encoded = object
                    .get("output_base64")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| ExchangeFailure {
                        kind: WorkerErrorKind::ProtocolError,
                        message: "persistent worker v2 byte output requires output_base64".into(),
                        transmitted,
                    })?;
                let output =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
                        .map_err(|error| ExchangeFailure {
                            kind: WorkerErrorKind::ProtocolError,
                            message: format!("invalid persistent worker base64 output: {error}"),
                            transmitted,
                        })?;
                (output, None)
            };
            (response_id, status, output, typed_json, stderr)
        };
        if response_id != id {
            return Err(ExchangeFailure {
                kind: WorkerErrorKind::ProtocolError,
                message: format!(
                    "persistent worker response id {} did not match request id {id}",
                    response_id
                ),
                transmitted,
            });
        }
        Ok(WorkerResponse {
            status,
            output,
            typed_json,
            stderr,
            spawned_process: false,
        })
    }

    fn negotiate_v2(
        &mut self,
        request: &WorkerRequest,
        max_frame_bytes: usize,
        deadline: Option<Instant>,
    ) -> std::result::Result<(), ExchangeFailure> {
        if WORKER_HANDSHAKE.len() > max_frame_bytes {
            return Err(ExchangeFailure {
                kind: WorkerErrorKind::OutputLimit,
                message: format!("worker v2 handshake exceeds {max_frame_bytes} bytes"),
                transmitted: false,
            });
        }
        if let Err(mut failure) = self.write_frame(WORKER_HANDSHAKE.to_vec(), request, deadline) {
            // The handshake cannot have run the user's request, so it does not
            // make the invocation's effect uncertain.
            failure.transmitted = false;
            return Err(failure);
        }
        let response = read_frame(
            &mut self.output,
            max_frame_bytes,
            &request.cancelled,
            deadline,
        )
        .map_err(|error| exchange_failure(error, false))?;
        let value = parse_worker_json_strict(&response).map_err(|error| ExchangeFailure {
            kind: WorkerErrorKind::ProtocolError,
            message: format!("invalid persistent worker v2 handshake response: {error}"),
            transmitted: false,
        })?;
        let object = value.as_object().ok_or_else(|| ExchangeFailure {
            kind: WorkerErrorKind::ProtocolError,
            message: "persistent worker v2 handshake response must be a JSON object".into(),
            transmitted: false,
        })?;
        if object.len() != 3
            || object.get("type").and_then(serde_json::Value::as_str) != Some("handshake")
            || object.get("protocol").and_then(serde_json::Value::as_str)
                != Some("ngsl.json-lines-worker")
            || object.get("version").and_then(serde_json::Value::as_u64) != Some(2)
        {
            return Err(ExchangeFailure {
                kind: WorkerErrorKind::ProtocolError,
                message: "persistent worker did not acknowledge JSON-lines frame version 2".into(),
                transmitted: false,
            });
        }
        self.negotiated = true;
        Ok(())
    }

    fn write_frame(
        &mut self,
        bytes: Vec<u8>,
        request: &WorkerRequest,
        deadline: Option<Instant>,
    ) -> std::result::Result<bool, ExchangeFailure> {
        #[cfg(unix)]
        {
            let mut transmitted = false;
            let mut written = 0;
            while written < bytes.len() {
                if let Err(error) = check_cancel_deadline(&request.cancelled, deadline) {
                    return Err(exchange_failure(error, transmitted));
                }
                match self.input.write(&bytes[written..]) {
                    Ok(0) => {
                        return Err(ExchangeFailure {
                            kind: WorkerErrorKind::TransportError,
                            message: "writing worker request made no progress".into(),
                            transmitted,
                        });
                    }
                    Ok(count) => {
                        written += count;
                        transmitted = true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if let Err(error) = wait_ready(
                            self.input.as_raw_fd(),
                            libc::POLLOUT,
                            &request.cancelled,
                            deadline,
                        ) {
                            return Err(exchange_failure(error, transmitted));
                        }
                    }
                    Err(error) => {
                        return Err(ExchangeFailure {
                            kind: if error.kind() == std::io::ErrorKind::BrokenPipe {
                                WorkerErrorKind::TransportError
                            } else {
                                WorkerErrorKind::IoError
                            },
                            message: format!("writing worker request: {error}"),
                            transmitted,
                        });
                    }
                }
            }
            Ok(transmitted)
        }
        #[cfg(windows)]
        {
            let input = crate::platform::windows::duplicate_handle_file(self.input.as_handle())
                .map_err(|error| ExchangeFailure {
                    kind: WorkerErrorKind::IoError,
                    message: format!("duplicating worker stdin handle: {error:#}"),
                    transmitted: false,
                })?;
            let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel::<()>(1);
            let writer = std::thread::spawn(move || {
                let _finished = WriterFinished(finished_tx);
                let mut input = input;
                let mut written = 0;
                let mut attempted = false;
                while written < bytes.len() {
                    attempted = true;
                    match input.write(&bytes[written..]) {
                        Ok(0) => {
                            return (
                                written,
                                attempted,
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::WriteZero,
                                    "writing worker request made no progress",
                                )),
                            );
                        }
                        Ok(count) => written += count,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return (written, attempted, Err(error)),
                    }
                }
                (written, attempted, Ok(()))
            });

            loop {
                let wait = deadline
                    .map(|until| until.saturating_duration_since(Instant::now()))
                    .unwrap_or(IO_POLL_INTERVAL)
                    .min(IO_POLL_INTERVAL);
                if !matches!(
                    finished_rx.recv_timeout(wait),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    let (written, _, result) = writer.join().map_err(|_| ExchangeFailure {
                        kind: WorkerErrorKind::IoError,
                        message: "worker stdin writer thread panicked".into(),
                        transmitted: true,
                    })?;
                    if let Err(error) = result {
                        return Err(ExchangeFailure {
                            kind: if error.kind() == std::io::ErrorKind::BrokenPipe {
                                WorkerErrorKind::TransportError
                            } else {
                                WorkerErrorKind::IoError
                            },
                            message: format!("writing worker request: {error}"),
                            transmitted: written > 0,
                        });
                    }
                    return Ok(written > 0);
                }

                if let Err(error) = check_cancel_deadline(&request.cancelled, deadline) {
                    self.job.terminate();
                    crate::platform::windows::cancel_synchronous_io(&writer);
                    let transmitted = writer
                        .join()
                        .map(|(written, attempted, _)| written > 0 || attempted)
                        .unwrap_or(true);
                    return Err(exchange_failure(error, transmitted));
                }
            }
        }
    }
}

#[cfg(any(unix, windows))]
impl Drop for WorkerProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // The group is signalled while the child is still unreaped; see UnreapedWorker.
            let _ = UnreapedWorker {
                child: &mut self.child,
            }
            .kill_group_and_reap();
        }
        #[cfg(windows)]
        {
            self.job.terminate();
            self.output.shutdown();
            let _ = self.child.wait();
        }
    }
}

#[cfg(any(unix, windows))]
fn exchange_failure(error: anyhow::Error, transmitted: bool) -> ExchangeFailure {
    let kind = error
        .downcast_ref::<WorkerError>()
        .map(|worker_error| worker_error.kind)
        .unwrap_or(WorkerErrorKind::UnknownError);
    ExchangeFailure {
        kind,
        message: format!("{error:#}"),
        transmitted,
    }
}

/// A worker's direct child before it is reaped. While the child is unreaped, its pid names
/// the worker's process group, so the group can be signalled only here. The only
/// method signals and then reaps, consuming the value, so the group signal can never
/// follow the reap.
#[cfg(unix)]
struct UnreapedWorker<'a> {
    child: &'a mut std::process::Child,
}

#[cfg(unix)]
impl UnreapedWorker<'_> {
    /// Kill the worker's process group, then reap the direct child. Errors from the kill
    /// are expected if the group already exited, and the reap still owns the child.
    fn kill_group_and_reap(self) -> std::io::Result<std::process::ExitStatus> {
        // Each worker starts a private process group.
        // SAFETY: kill(2) reads only its two integer arguments. The group id is the child's pid,
        // which is positive and fits pid_t, and the child was spawned with process_group(0), so
        // that pid is its own group. The child is unreaped here, because this method consumes
        // the value and reaps only afterwards, so the pid is reserved and not reused.
        unsafe {
            libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
        }
        self.child.wait()
    }
}

#[cfg(unix)]
fn set_nonblocking(file: &impl std::os::fd::AsRawFd) -> std::io::Result<()> {
    // `file` is borrowed for the whole call, so its descriptor stays open while the flags change.
    crate::platform::unix_ffi::set_nonblocking(file.as_raw_fd()).map(|_| ())
}

#[cfg(unix)]
fn wait_ready(
    descriptor: std::os::fd::RawFd,
    events: libc::c_short,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    loop {
        check_cancel_deadline(cancelled, deadline)?;
        let timeout = deadline
            .map(|until| until.saturating_duration_since(Instant::now()))
            .unwrap_or(IO_POLL_INTERVAL)
            .min(IO_POLL_INTERVAL);
        let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        // `millis` is clamped to 0..=i32::MAX above, so it is a valid poll timeout. An invalid
        // descriptor is reported in revents (POLLNVAL) and handled by the check below.
        let (ready, revents) = match crate::platform::unix_ffi::poll_one(descriptor, events, millis)
        {
            Ok(result) => result,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(WorkerError::new(
                    WorkerErrorKind::IoError,
                    false,
                    None,
                    format!("polling worker protocol pipe: {error}"),
                )
                .into());
            }
        };
        if ready == 0 {
            continue;
        }
        if revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(WorkerError::new(
                WorkerErrorKind::TransportError,
                false,
                None,
                "worker protocol pipe closed while waiting for readiness",
            )
            .into());
        }
        if revents & (events | libc::POLLHUP) != 0 {
            return Ok(());
        }
    }
}

#[cfg(any(unix, windows))]
fn read_frame(
    reader: &mut WorkerOutput,
    max_frame_bytes: usize,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(max_frame_bytes.min(4096));
    let mut chunk = [0u8; IO_CHUNK_BYTES];
    loop {
        let count = read_worker_chunk(reader, &mut chunk, cancelled, deadline)?;
        if count == 0 {
            return Err(WorkerError::new(
                WorkerErrorKind::TransportError,
                false,
                None,
                "persistent worker closed stdout before completing a response frame",
            )
            .into());
        }
        let received = &chunk[..count];
        if let Some(newline) = received.iter().position(|byte| *byte == b'\n') {
            let prefix = &received[..newline];
            let total = frame.len().saturating_add(prefix.len()).saturating_add(1);
            if total > max_frame_bytes {
                return Err(WorkerError::new(
                    WorkerErrorKind::ProtocolError,
                    false,
                    None,
                    format!("persistent worker response frame exceeds {max_frame_bytes} bytes"),
                )
                .into());
            }
            frame.extend_from_slice(prefix);
            if newline + 1 != received.len() {
                return Err(WorkerError::new(
                    WorkerErrorKind::ProtocolError,
                    false,
                    None,
                    "persistent worker wrote bytes after a response frame",
                )
                .into());
            }
            return Ok(frame);
        }
        if frame.len().saturating_add(count).saturating_add(1) > max_frame_bytes {
            return Err(WorkerError::new(
                WorkerErrorKind::ProtocolError,
                false,
                None,
                format!("persistent worker response frame exceeds {max_frame_bytes} bytes"),
            )
            .into());
        }
        frame.extend_from_slice(received);
    }
}

#[cfg(unix)]
fn read_worker_chunk(
    reader: &mut WorkerOutput,
    buffer: &mut [u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<usize> {
    loop {
        check_cancel_deadline(cancelled, deadline)?;
        match reader.read(buffer) {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_ready(reader.as_raw_fd(), libc::POLLIN, cancelled, deadline)?;
            }
            Err(error) => {
                return Err(WorkerError::new(
                    WorkerErrorKind::IoError,
                    false,
                    None,
                    format!("reading persistent worker response: {error}"),
                )
                .into());
            }
        }
    }
}

/// Signals the writer's completion when the thread returns or unwinds.
#[cfg(windows)]
struct WriterFinished(std::sync::mpsc::SyncSender<()>);

#[cfg(windows)]
impl Drop for WriterFinished {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Wait for worker output, bounded by the deadline and the poll interval.
#[cfg(windows)]
fn wait_for_worker_output(reader: &mut WorkerOutput, deadline: Option<Instant>) {
    let remaining = deadline
        .map(|until| until.saturating_duration_since(Instant::now()))
        .unwrap_or(IO_POLL_INTERVAL);
    reader.wait_readable(IO_POLL_INTERVAL.min(remaining));
}

#[cfg(windows)]
fn read_worker_chunk(
    reader: &mut WorkerOutput,
    buffer: &mut [u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<usize> {
    loop {
        check_cancel_deadline(cancelled, deadline)?;
        match reader.read(buffer) {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_worker_output(reader, deadline);
            }
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(0),
            Err(error) => {
                return Err(WorkerError::new(
                    WorkerErrorKind::IoError,
                    false,
                    None,
                    format!("reading persistent worker response: {error}"),
                )
                .into());
            }
        }
    }
}

#[cfg(any(unix, windows))]
struct CappedBuffer {
    bytes: Vec<u8>,
    maximum: usize,
}

#[cfg(any(unix, windows))]
impl CappedBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(maximum.min(4096)),
            maximum,
        }
    }

    fn push_newline(&mut self) -> std::io::Result<()> {
        if self.bytes.len() >= self.maximum {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame limit reached",
            ));
        }
        self.bytes.push(b'\n');
        Ok(())
    }
}

#[cfg(any(unix, windows))]
impl Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let remaining = self.maximum.saturating_sub(self.bytes.len());
        if remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame limit reached",
            ));
        }
        let accepted = remaining.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..accepted]);
        Ok(accepted)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(any(unix, windows))]
fn validate_launch(launch: &WorkerLaunch) -> Result<()> {
    if !(1..=2).contains(&launch.protocol_version) {
        bail!("persistent worker protocol version must be 1 or 2");
    }
    if launch.applet_name.trim().is_empty() || launch.applet_name.contains('\0') {
        bail!("persistent worker applet name must be nonempty and contain no NUL byte");
    }
    if launch.max_frame_bytes == 0 {
        bail!("persistent worker frame limit must be positive");
    }
    if launch.timeout.is_zero() {
        bail!("persistent worker timeout must be positive");
    }
    if launch.binary.as_os_str().is_empty() {
        bail!("persistent worker executable path must be nonempty");
    }
    for argument in &launch.startup_arguments {
        if os_str_contains_nul(argument) {
            bail!("persistent worker startup argument contains a NUL byte");
        }
    }
    let mut seen = HashSet::new();
    for (name, value) in &launch.environment {
        if name.is_empty()
            || os_str_contains_nul(name)
            || os_str_contains_nul(value)
            || name.as_encoded_bytes().contains(&b'=')
        {
            bail!("persistent worker environment contains an invalid key or value");
        }
        if !seen.insert(name) {
            bail!("persistent worker environment contains duplicate keys");
        }
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn validate_request(launch: &WorkerLaunch, request: &WorkerRequest) -> Result<Vec<u8>> {
    if request
        .arguments
        .iter()
        .any(|argument| argument.contains('\0'))
    {
        bail!("persistent worker argument contains a NUL byte");
    }
    // The serializer writes through a capped sink, so even a large input or
    // hostile string cannot allocate a frame beyond the manifest limit. Use
    // the widest request id to validate before launching or leasing a worker.
    if launch.max_frame_bytes < 2 {
        return Err(WorkerError::new(
            WorkerErrorKind::OutputLimit,
            false,
            None,
            "persistent worker frame limit is too small for JSON-lines framing",
        )
        .into());
    }
    if launch.protocol_version == 2 {
        let encoded_input_len = request
            .input
            .len()
            .checked_add(2)
            .and_then(|length| length.checked_div(3))
            .and_then(|blocks| blocks.checked_mul(4));
        if encoded_input_len.is_none_or(|length| length > launch.max_frame_bytes) {
            return Err(WorkerError::new(
                WorkerErrorKind::OutputLimit,
                false,
                None,
                format!(
                    "persistent worker request input exceeds the {} byte frame limit after base64 encoding",
                    launch.max_frame_bytes
                ),
            )
            .into());
        }
    }
    let mut frame = CappedBuffer::new(launch.max_frame_bytes);
    let encoded = if launch.protocol_version == 1 {
        serde_json::to_writer(
            &mut frame,
            &RequestFrame {
                id: u64::MAX,
                arguments: &request.arguments,
                input: &request.input,
            },
        )
    } else {
        let input_base64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &request.input);
        serde_json::to_writer(
            &mut frame,
            &RequestFrameV2 {
                version: 2,
                id: u64::MAX,
                arguments: &request.arguments,
                input_base64: &input_base64,
                value: request.value.as_ref(),
                json_output: request.json_output,
            },
        )
    };
    if encoded.is_err() || frame.push_newline().is_err() {
        return Err(WorkerError::new(
            WorkerErrorKind::OutputLimit,
            false,
            None,
            format!(
                "persistent worker request frame exceeds {} bytes",
                launch.max_frame_bytes
            ),
        )
        .into());
    }
    Ok(frame.bytes)
}

#[cfg(any(unix, windows))]
fn patch_request_id(frame: &mut Vec<u8>, id: u64) -> std::io::Result<()> {
    const PREFIX: &[u8] = b"\"id\":18446744073709551615";
    let Some(start) = frame
        .windows(PREFIX.len())
        .position(|window| window == PREFIX)
        .map(|offset| offset + b"\"id\":".len())
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request frame has no reserved id field",
        ));
    };
    frame.splice(start..start + 20, id.to_string().bytes());
    Ok(())
}

#[cfg(any(unix, windows))]
fn ensure_policy_unchanged(launch: &WorkerLaunch) -> Result<()> {
    if crate::capabilities::policy_fingerprint(&launch.registry, &launch.applet_name)?
        != launch.policy_fingerprint
    {
        bail!("applet capability policy changed after worker launch configuration was prepared");
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn os_str_contains_nul(value: &OsStr) -> bool {
    #[cfg(unix)]
    {
        value.as_encoded_bytes().contains(&0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        value.encode_wide().any(|unit| unit == 0)
    }
}

#[cfg(any(unix, windows))]
fn binary_identity(
    binary: &PathBuf,
    deadline: Option<Instant>,
    cancelled: &AtomicBool,
) -> Result<[u8; 32]> {
    let mut file = File::open(binary)
        .with_context(|| format!("opening persistent worker executable {}", binary.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_cancel_deadline(cancelled, deadline)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(any(unix, windows))]
fn min_deadline(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

#[cfg(any(unix, windows))]
fn check_cancel_deadline(cancelled: &AtomicBool, deadline: Option<Instant>) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(WorkerError::new(
            WorkerErrorKind::Cancelled,
            false,
            None,
            "persistent applet worker request cancelled",
        )
        .into());
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(WorkerError::new(
            WorkerErrorKind::Timeout,
            false,
            None,
            "persistent applet worker timed out",
        )
        .into());
    }
    Ok(())
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    #[test]
    fn limited_encoder_never_grows_past_frame_cap() {
        let mut output = CappedBuffer::new(8);
        let error = serde_json::to_writer(&mut output, &"x".repeat(100)).unwrap_err();
        assert!(error.is_io());
        assert!(output.bytes.len() <= 8);
    }

    #[test]
    fn v2_json_parser_rejects_duplicate_keys_at_every_depth() {
        assert!(parse_worker_json_strict(br#"{"value":{"count":1,"count":2}}"#).is_err());
        assert!(parse_worker_json_strict(br#"{"value":[{"count":1,"count":2}]}"#).is_err());
    }

    #[test]
    fn deadline_is_the_earlier_constraint() {
        let earlier = Instant::now() + Duration::from_secs(1);
        let later = earlier + Duration::from_secs(1);
        assert_eq!(min_deadline(Some(earlier), Some(later)), Some(earlier));
        assert_eq!(min_deadline(None, Some(later)), Some(later));
    }

    #[test]
    fn slot_id_wrap_skips_ids_still_in_use() {
        let mut state = PoolState {
            slots: vec![WorkerSlot {
                id: 1,
                key: WorkerKey {
                    applet_name: "test".to_owned(),
                    binary: PathBuf::from("worker"),
                    content_identity: [0; 32],
                    protocol_version: 1,
                    policy_fingerprint: None,
                    cwd: PathBuf::from("."),
                    environment: Vec::new(),
                    startup_arguments: Vec::new(),
                },
                worker: None,
                busy: false,
            }],
            next_id: u64::MAX,
        };

        assert_eq!(next_slot_id(&mut state), 2);
    }
}
