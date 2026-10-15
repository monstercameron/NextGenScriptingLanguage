//! Durable journal for opt-in, resumable finite and service execution.
use crate::ast::Value;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[allow(dead_code)]
mod leaf_cursor;

const LEGACY_JOURNAL_VERSION: u32 = 1;
const JOURNAL_VERSION: u32 = 2;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Settings for one checkpoint directory. The directory holds an exclusive lock,
/// a checksummed journal, and content-addressed state blobs.
#[derive(Debug, Clone)]
pub struct CheckpointOptions {
    pub directory: PathBuf,
    pub program_path: PathBuf,
    pub resume: bool,
    pub retry_uncertain: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct ExecutionState {
    pub cwd: PathBuf,
    pub output: Vec<u8>,
    pub bindings: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CompletedNode {
    pub fingerprint: String,
    pub start: usize,
    /// Exclusive step index.
    pub end: usize,
    #[serde(with = "path_serde")]
    pub cwd_before: PathBuf,
}

mod path_serde {
    use super::EncodedPath;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::path::{Path, PathBuf};

    pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        EncodedPath::from_path(path).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        EncodedPath::deserialize(deserializer).map(|path| path.to_path())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Running,
    Failed,
    Interrupted,
    Completed,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_signature: Option<String>,
    program_path: EncodedPath,
    initial_cwd: EncodedPath,
    mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EncodedPath(Vec<u8>);

impl<'de> Deserialize<'de> for EncodedPath {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        #[cfg(windows)]
        if bytes.len() % 2 != 0 {
            return Err(serde::de::Error::custom(
                "Windows checkpoint path must contain complete UTF-16 units",
            ));
        }
        Ok(Self(bytes))
    }
}
impl EncodedPath {
    fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            Self(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self(
                path.as_os_str()
                    .encode_wide()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            )
        }
    }
    fn to_path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            PathBuf::from(std::ffi::OsString::from_vec(self.0.clone()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let wide: Vec<u16> = self
                .0
                .as_chunks::<2>()
                .0
                .iter()
                .map(|v| u16::from_le_bytes([v[0], v[1]]))
                .collect();
            PathBuf::from(std::ffi::OsString::from_wide(&wide))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobRef {
    sha256: String,
    length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRef {
    cwd: EncodedPath,
    output: BlobRef,
    bindings: BTreeMap<String, BlobRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InFlightNode {
    fingerprint: String,
    start: usize,
    end: usize,
    line: usize,
    cwd_before: EncodedPath,
    retry_safe: bool,
}

impl InFlightNode {
    fn completed(&self) -> CompletedNode {
        CompletedNode {
            fingerprint: self.fingerprint.clone(),
            start: self.start,
            end: self.end,
            cwd_before: self.cwd_before.to_path(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalData {
    version: u32,
    identity: Identity,
    status: Status,
    completed: Vec<CompletedNode>,
    in_flight: Option<InFlightNode>,
    snapshot: SnapshotRef,
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    listener_cursor: Option<ListenerEventCursor>,
}

/// A local cursor for listener records admitted to a checkpoint. It is not a
/// producer offset or acknowledgement and cannot recover bytes read from an
/// uncooperative stdin/pipe before the event snapshot is durably published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListenerEventCursor {
    pub accepted_sequence: u64,
    pub committed_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_committed_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEnvelope {
    checksum: String,
    data: JournalData,
}

#[derive(Serialize)]
struct JournalEnvelopeRef<'a> {
    checksum: String,
    data: &'a JournalData,
}

#[derive(Default)]
struct DigestWriter(Sha256);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An open checkpoint journal. Holding this value also holds the nonblocking
/// exclusive lock for its entire lifetime.
pub(super) struct Journal {
    directory: PathBuf,
    blobs_directory: PathBuf,
    max_output_bytes: usize,
    retry_uncertain: bool,
    resuming: bool,
    persistence_failed: bool,
    preloaded_state: Option<ExecutionState>,
    data: JournalData,
    _lock: File,
}

impl Journal {
    #[cfg(all(test, unix))]
    pub fn open(
        options: &CheckpointOptions,
        initial_cwd: &Path,
        mode: &str,
        max_output_bytes: usize,
    ) -> Result<Self> {
        Self::open_with_inputs(options, initial_cwd, mode, max_output_bytes, None)
    }

    pub fn open_with_inputs(
        options: &CheckpointOptions,
        initial_cwd: &Path,
        mode: &str,
        max_output_bytes: usize,
        input_signature: Option<String>,
    ) -> Result<Self> {
        validate_mode(mode)?;
        let program_path = fs::canonicalize(&options.program_path)
            .with_context(|| format!("resolving program {}", options.program_path.display()))?;
        if !program_path.is_file() {
            bail!("checkpoint program path is not a file");
        }
        let initial_cwd = fs::canonicalize(initial_cwd)
            .with_context(|| format!("resolving initial directory {}", initial_cwd.display()))?;
        if !initial_cwd.is_dir() {
            bail!("checkpoint initial directory is not a directory");
        }

        // Normalize trailing separators/dot components before checking the leaf:
        // symlink_metadata("link/") would otherwise follow the link on Unix.
        let requested_directory: PathBuf = options.directory.components().collect();
        let checkpoint_dir_existed = fs::symlink_metadata(&requested_directory).is_ok();
        let original = real_directory(&requested_directory, "checkpoint", !options.resume)?;
        #[cfg(windows)]
        let original_identity = crate::platform::windows::directory_identity(&requested_directory)?;
        let directory = fs::canonicalize(&requested_directory).with_context(|| {
            format!(
                "resolving checkpoint directory {}",
                requested_directory.display()
            )
        })?;
        let resolved = real_directory(&directory, "checkpoint", false)?;
        #[cfg(unix)]
        if original.dev() != resolved.dev() || original.ino() != resolved.ino() {
            bail!("checkpoint directory changed while resolving its path");
        }
        #[cfg(windows)]
        {
            let _ = (original, resolved);
            if crate::platform::windows::directory_identity(&directory)? != original_identity {
                bail!("checkpoint directory changed while resolving its path");
            }
        }
        restrict_directory(&directory)?;
        if !options.resume && !checkpoint_dir_existed {
            if let Some(parent) = directory.parent() {
                sync_directory(parent)?;
            }
        }

        let lock_path = directory.join("lock");
        let lock = open_regular(&lock_path, "checkpoint lock", true)?;
        restrict_file(&lock)?;
        #[cfg(windows)]
        crate::platform::windows::lock(&lock)?;
        #[cfg(unix)]
        let lock_result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        #[cfg(unix)]
        if lock_result == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                bail!("checkpoint is already locked: {}", directory.display());
            }
            return Err(error).context("locking checkpoint");
        }

        let journal_path = directory.join("journal.json");
        let blobs_directory = directory.join("blobs");
        if options.resume {
            let mut data = read_journal(&journal_path)?;
            let expected = Identity {
                program_path: EncodedPath::from_path(&program_path),
                initial_cwd: EncodedPath::from_path(&initial_cwd),
                mode: mode.to_owned(),
                input_signature: input_signature.clone(),
            };
            if data.identity != expected {
                bail!(
                    "checkpoint belongs to a different program, directory, execution mode, or program inputs"
                );
            }
            if data.version != JOURNAL_VERSION && data.version != LEGACY_JOURNAL_VERSION {
                bail!("unsupported checkpoint version {}", data.version);
            }
            let migrate_legacy = data.version == LEGACY_JOURNAL_VERSION;
            if migrate_legacy {
                data.version = JOURNAL_VERSION;
            }
            real_directory(&blobs_directory, "checkpoint blob", false)?;
            restrict_directory(&blobs_directory)?;
            let mut journal = Self {
                directory,
                blobs_directory,
                max_output_bytes,
                retry_uncertain: options.retry_uncertain,
                resuming: options.resume,
                persistence_failed: false,
                preloaded_state: None,
                data,
                _lock: lock,
            };
            // Fail at open on truncated, missing, oversized, or corrupt state blobs.
            let restored = journal.restored_state()?;
            // Finite resumes and pending service executions need this exact state
            // immediately after compatibility checks. Idle services only validate
            // their previous snapshot before waiting for a new event.
            if journal.data.identity.mode == "finite" || journal.has_pending() {
                journal.preloaded_state = Some(restored);
            }
            if migrate_legacy {
                // The new optional cursor field is omitted when absent, so the
                // old v1 checksum verifies before this atomic v2 rewrite.
                journal.persist()?;
            }
            Ok(journal)
        } else {
            if fs::symlink_metadata(&journal_path).is_ok() {
                bail!("checkpoint already exists; use resume explicitly");
            }
            real_directory(&blobs_directory, "checkpoint blob", true)?;
            restrict_directory(&blobs_directory)?;
            sync_directory(&directory)?;
            let identity = Identity {
                program_path: EncodedPath::from_path(&program_path),
                initial_cwd: EncodedPath::from_path(&initial_cwd),
                mode: mode.to_owned(),
                input_signature: input_signature.clone(),
            };
            let initial = ExecutionState {
                cwd: initial_cwd,
                output: Vec::new(),
                bindings: BTreeMap::new(),
            };
            let snapshot = store_snapshot(&blobs_directory, &initial, max_output_bytes)?;
            let data = JournalData {
                version: JOURNAL_VERSION,
                identity,
                status: if mode == "finite" {
                    Status::Running
                } else {
                    Status::Idle
                },
                completed: Vec::new(),
                in_flight: None,
                snapshot,
                error: None,
                listener_cursor: None,
            };
            let mut journal = Self {
                directory,
                blobs_directory,
                max_output_bytes,
                retry_uncertain: options.retry_uncertain,
                resuming: options.resume,
                persistence_failed: false,
                preloaded_state: None,
                data,
                _lock: lock,
            };
            journal.persist()?;
            Ok(journal)
        }
    }

    pub fn completed_nodes(&self) -> &[CompletedNode] {
        &self.data.completed
    }

    pub fn restored_state(&self) -> Result<ExecutionState> {
        load_snapshot(
            &self.blobs_directory,
            &self.data.snapshot,
            self.max_output_bytes,
        )
    }

    /// Return the state already verified when resuming, avoiding a second full
    /// read and decode. Later calls reload the current durable snapshot.
    pub fn take_restored_state(&mut self) -> Result<ExecutionState> {
        if let Some(state) = self.preloaded_state.take() {
            Ok(state)
        } else {
            self.restored_state()
        }
    }

    pub fn has_pending(&self) -> bool {
        matches!(
            self.data.status,
            Status::Running | Status::Failed | Status::Interrupted
        )
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.data.status, Status::Completed | Status::Idle)
    }

    /// True after a checkpoint write failed. The caller must stop execution and
    /// leave the last durable in-flight intent untouched rather than recording
    /// a known execution failure over it.
    pub fn persistence_failed(&self) -> bool {
        self.persistence_failed
    }

    pub fn ensure_retry_allowed(&self) -> Result<()> {
        let uncertain = matches!(self.data.status, Status::Interrupted | Status::Running)
            && self
                .data
                .in_flight
                .as_ref()
                .is_some_and(|node| !node.retry_safe);
        if uncertain && !self.retry_uncertain {
            bail!(
                "checkpoint may contain an interrupted side effect; pass --retry-uncertain to retry"
            );
        }
        Ok(())
    }

    #[cfg(all(test, unix))]
    pub fn begin_execution(&mut self, state: &ExecutionState) -> Result<()> {
        self.begin_execution_parts(
            &state.cwd,
            &state.output,
            state
                .bindings
                .iter()
                .map(|(name, value)| (name.as_str(), value)),
        )
    }

    pub fn begin_execution_parts<'a>(
        &mut self,
        cwd: &Path,
        output: &[u8],
        bindings: impl IntoIterator<Item = (&'a str, &'a Value)>,
    ) -> Result<()> {
        self.begin_execution_parts_with_listener_event(cwd, output, bindings, None)
    }

    /// Durably admit one listener record and its handler snapshot under a
    /// local monotonic sequence. This cursor does not acknowledge or seek the
    /// upstream producer; bytes consumed before this call are outside it.
    pub fn begin_listener_event_parts<'a>(
        &mut self,
        cwd: &Path,
        output: &[u8],
        bindings: impl IntoIterator<Item = (&'a str, &'a Value)>,
    ) -> Result<()> {
        if !self.data.identity.mode.starts_with("service:") {
            bail!("listener event cursor is only valid for service checkpoints");
        }
        self.begin_execution_parts_with_listener_event(
            cwd,
            output,
            bindings,
            Some(hex_digest(output)),
        )
    }

    #[cfg(all(test, unix))]
    pub fn listener_event_cursor(&self) -> Option<ListenerEventCursor> {
        self.data.listener_cursor.clone()
    }

    fn begin_execution_parts_with_listener_event<'a>(
        &mut self,
        cwd: &Path,
        output: &[u8],
        bindings: impl IntoIterator<Item = (&'a str, &'a Value)>,
        listener_event_digest: Option<String>,
    ) -> Result<()> {
        if self.data.identity.mode == "finite" {
            bail!("begin_execution is only valid for service checkpoints");
        }
        self.ensure_retry_allowed()?;
        if self
            .data
            .listener_cursor
            .as_ref()
            .is_some_and(|cursor| cursor.pending_sha256.is_some())
        {
            bail!("a listener event is already pending in the checkpoint");
        }
        let next_listener_sequence = if listener_event_digest.is_some() {
            let accepted = self
                .data
                .listener_cursor
                .as_ref()
                .map_or(0, |cursor| cursor.accepted_sequence);
            Some(
                accepted
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("listener event sequence is exhausted"))?,
            )
        } else {
            None
        };
        let snapshot = store_snapshot_parts(
            &self.blobs_directory,
            cwd,
            output,
            bindings,
            self.max_output_bytes,
        )?;
        let previous_status = self.data.status;
        let previous_completed = std::mem::take(&mut self.data.completed);
        let previous_in_flight = self.data.in_flight.take();
        let previous_snapshot = std::mem::replace(&mut self.data.snapshot, snapshot);
        let previous_error = self.data.error.take();
        let previous_listener_cursor = self.data.listener_cursor.clone();
        if let Some(event_digest) = listener_event_digest {
            let cursor = self
                .data
                .listener_cursor
                .get_or_insert(ListenerEventCursor {
                    accepted_sequence: 0,
                    committed_sequence: 0,
                    pending_sha256: None,
                    last_committed_sha256: None,
                });
            cursor.accepted_sequence = next_listener_sequence.expect("listener sequence prepared");
            cursor.pending_sha256 = Some(event_digest);
        }
        self.data.status = Status::Running;
        if let Err(error) = self.persist() {
            self.data.status = previous_status;
            self.data.completed = previous_completed;
            self.data.in_flight = previous_in_flight;
            self.data.snapshot = previous_snapshot;
            self.data.error = previous_error;
            self.data.listener_cursor = previous_listener_cursor;
            return Err(error);
        }
        self.preloaded_state = None;
        Ok(())
    }

    pub fn begin_node(
        &mut self,
        fingerprint: &str,
        start: usize,
        end: usize,
        line: usize,
        cwd_before: &Path,
        retry_safe: bool,
    ) -> Result<()> {
        if start >= end {
            bail!("checkpoint node must cover at least one step");
        }
        if !self.has_pending() {
            bail!("checkpoint is not in a running execution");
        }
        if self.data.in_flight.is_some() {
            self.ensure_retry_allowed()?;
        }
        let previous_status = self.data.status;
        let previous_error = self.data.error.take();
        let previous_in_flight = self.data.in_flight.replace(InFlightNode {
            fingerprint: fingerprint.to_owned(),
            start,
            end,
            line,
            cwd_before: EncodedPath::from_path(cwd_before),
            retry_safe,
        });
        self.data.status = Status::Running;
        if let Err(error) = self.persist() {
            self.data.status = previous_status;
            self.data.error = previous_error;
            self.data.in_flight = previous_in_flight;
            return Err(error);
        }
        Ok(())
    }

    #[cfg(all(test, unix))]
    pub fn complete_node(&mut self, state: &ExecutionState) -> Result<()> {
        self.complete_node_parts(
            &state.cwd,
            &state.output,
            state
                .bindings
                .iter()
                .map(|(name, value)| (name.as_str(), value)),
        )
    }

    pub fn complete_node_parts<'a>(
        &mut self,
        cwd: &Path,
        output: &[u8],
        bindings: impl IntoIterator<Item = (&'a str, &'a Value)>,
    ) -> Result<()> {
        let completed = self
            .data
            .in_flight
            .as_ref()
            .ok_or_else(|| anyhow!("no checkpoint node is in flight"))?;
        let completed = completed.completed();
        let snapshot = match store_snapshot_parts(
            &self.blobs_directory,
            cwd,
            output,
            bindings,
            self.max_output_bytes,
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                // The node has already run. Keep the durable in-flight record so resume
                // can apply the retry policy instead of claiming a known failure.
                self.persistence_failed = true;
                return Err(error);
            }
        };
        let previous_completed_len = self.data.completed.len();
        self.data.completed.push(completed);
        let previous_in_flight = self.data.in_flight.take();
        let previous_snapshot = std::mem::replace(&mut self.data.snapshot, snapshot);
        let previous_error = self.data.error.take();
        if let Err(error) = self.persist() {
            self.data.completed.truncate(previous_completed_len);
            self.data.in_flight = previous_in_flight;
            self.data.snapshot = previous_snapshot;
            self.data.error = previous_error;
            return Err(error);
        }
        self.preloaded_state = None;
        Ok(())
    }

    pub fn fail(&mut self, message: &str, cancelled: bool) -> Result<()> {
        if self.persistence_failed {
            bail!(
                "checkpoint persistence failed after a node ran; preserving its durable in-flight record: {message}"
            );
        }
        let previous_status = self.data.status;
        let previous_error = self.data.error.replace(message.to_owned());
        self.data.status = if cancelled {
            Status::Interrupted
        } else {
            Status::Failed
        };
        if let Err(error) = self.persist() {
            self.data.status = previous_status;
            self.data.error = previous_error;
            return Err(error);
        }
        Ok(())
    }

    /// An explicit resume may remove the unfinished suffix entirely. The
    /// completed prefix was validated by the runtime before calling this.
    pub fn discard_removed_tail(&mut self) -> Result<()> {
        if self.data.in_flight.is_none() {
            return Ok(());
        }
        if !self.resuming {
            bail!("only an explicit resume can discard an unfinished node");
        }
        self.ensure_retry_allowed()?;
        let previous_status = self.data.status;
        let previous_error = self.data.error.take();
        let previous_in_flight = self.data.in_flight.take();
        self.data.status = Status::Running;
        if let Err(error) = self.persist() {
            self.data.status = previous_status;
            self.data.error = previous_error;
            self.data.in_flight = previous_in_flight;
            return Err(error);
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.data.in_flight.is_some() {
            bail!("cannot finish with a checkpoint node in flight");
        }
        let listener_commit = match self.data.listener_cursor.as_ref() {
            Some(cursor) if cursor.pending_sha256.is_some() => {
                let sequence = cursor.accepted_sequence;
                if sequence != cursor.committed_sequence.saturating_add(1) {
                    bail!("listener event cursor sequence is inconsistent");
                }
                Some((
                    sequence,
                    cursor.pending_sha256.clone().expect("checked some"),
                ))
            }
            Some(cursor) if cursor.accepted_sequence != cursor.committed_sequence => {
                bail!("listener event cursor is missing a pending digest");
            }
            _ => None,
        };
        let previous_status = self.data.status;
        let previous_error = self.data.error.take();
        let previous_listener_cursor = self.data.listener_cursor.clone();
        if let (Some((sequence, digest)), Some(cursor)) =
            (listener_commit, self.data.listener_cursor.as_mut())
        {
            cursor.committed_sequence = sequence;
            cursor.last_committed_sha256 = Some(digest);
            cursor.pending_sha256 = None;
        }
        self.data.status = if self.data.identity.mode == "finite" {
            Status::Completed
        } else {
            Status::Idle
        };
        if let Err(error) = self.persist() {
            self.data.status = previous_status;
            self.data.error = previous_error;
            self.data.listener_cursor = previous_listener_cursor;
            return Err(error);
        }
        if self.data.identity.mode != "finite" {
            self.collect_unreferenced_blobs();
        }
        Ok(())
    }

    fn collect_unreferenced_blobs(&self) {
        let mut retained = BTreeMap::new();
        retained.insert(self.data.snapshot.output.sha256.as_str(), ());
        for blob in self.data.snapshot.bindings.values() {
            retained.insert(blob.sha256.as_str(), ());
        }
        let Ok(entries) = fs::read_dir(&self.blobs_directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.len() != 64
                || !name.bytes().all(|byte| byte.is_ascii_hexdigit())
                || retained.contains_key(name)
            {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn persist(&mut self) -> Result<()> {
        let envelope = JournalEnvelopeRef {
            checksum: serialized_digest(&self.data).context("encoding checkpoint journal")?,
            data: &self.data,
        };
        let bytes = serde_json::to_vec(&envelope).context("encoding checkpoint envelope")?;
        if bytes.len() > 16 * 1024 * 1024 {
            bail!("checkpoint journal exceeds 16 MiB");
        }
        match atomic_write(&self.directory, "journal", "json", &bytes)
            .context("persisting checkpoint journal")
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.persistence_failed = true;
                Err(error)
            }
        }
    }
}

fn real_directory(path: &Path, label: &str, create: bool) -> Result<fs::Metadata> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .with_context(|| format!("creating {label} directory {}", path.display()))?;
            fs::symlink_metadata(path)?
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading {label} directory {}", path.display()));
        }
    };
    #[cfg(windows)]
    if crate::platform::windows::is_reparse(&metadata) {
        bail!("checkpoint directory cannot be a reparse point");
    }
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("{label} path must be a real directory");
    }
    Ok(metadata)
}

fn open_regular(path: &Path, label: &str, create: bool) -> Result<File> {
    // Reject known special files before open; recheck the opened descriptor too.
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("{label} must be a regular file");
        }
    }
    let mut options = crate::platform::private_options();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*};
        options
            .access_mode(
                GENERIC_READ | WRITE_DAC | READ_CONTROL | if create { GENERIC_WRITE } else { 0 },
            )
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {label} {}", path.display()))?;
    #[cfg(windows)]
    if crate::platform::windows::is_reparse(&file.metadata()?) {
        bail!("checkpoint file cannot be a reparse point");
    }
    if !file.metadata()?.is_file() {
        bail!("{label} must be a regular file");
    }
    Ok(file)
}

fn validate_mode(mode: &str) -> Result<()> {
    if mode != "finite" && !mode.starts_with("service:") {
        bail!("checkpoint mode must be `finite` or `service:<trigger hash>`");
    }
    if mode.starts_with("service:") && mode.len() == "service:".len() {
        bail!("service checkpoint mode requires a trigger hash");
    }
    Ok(())
}

fn read_journal(path: &Path) -> Result<JournalData> {
    let file = open_regular(path, "checkpoint journal", false)?;
    restrict_file(&file)?;
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 {
        bail!("checkpoint journal is too large");
    }
    let envelope: JournalEnvelope =
        serde_json::from_slice(&bytes).context("checkpoint journal is malformed or corrupt")?;
    if serialized_digest(&envelope.data)? != envelope.checksum {
        bail!("checkpoint journal checksum mismatch");
    }
    if envelope.data.version != JOURNAL_VERSION && envelope.data.version != LEGACY_JOURNAL_VERSION {
        bail!("unsupported checkpoint version {}", envelope.data.version);
    }
    if let Some(cursor) = &envelope.data.listener_cursor {
        if !envelope.data.identity.mode.starts_with("service:") {
            bail!("checkpoint journal contains a listener cursor outside service mode");
        }
        let pending = cursor.pending_sha256.as_deref();
        let last = cursor.last_committed_sha256.as_deref();
        let valid_hash = |hash: &str| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        };
        let consistent = if cursor.accepted_sequence == cursor.committed_sequence {
            pending.is_none()
        } else {
            cursor.accepted_sequence == cursor.committed_sequence.saturating_add(1)
                && pending.is_some()
        };
        if !consistent
            || pending.is_some_and(|hash| !valid_hash(hash))
            || last.is_some_and(|hash| !valid_hash(hash))
            || (cursor.committed_sequence == 0) != last.is_none()
        {
            bail!("checkpoint journal contains an invalid listener event cursor");
        }
    }
    if envelope
        .data
        .completed
        .iter()
        .any(|node| node.start >= node.end)
    {
        bail!("checkpoint journal contains an invalid completed node range");
    }
    if envelope
        .data
        .in_flight
        .as_ref()
        .is_some_and(|node| node.start >= node.end)
    {
        bail!("checkpoint journal contains an invalid in-flight node range");
    }
    Ok(envelope.data)
}

fn store_snapshot(
    blobs_directory: &Path,
    state: &ExecutionState,
    max_output_bytes: usize,
) -> Result<SnapshotRef> {
    store_snapshot_parts(
        blobs_directory,
        &state.cwd,
        &state.output,
        state
            .bindings
            .iter()
            .map(|(name, value)| (name.as_str(), value)),
        max_output_bytes,
    )
}

fn store_snapshot_parts<'a>(
    blobs_directory: &Path,
    cwd: &Path,
    output_bytes: &[u8],
    bindings: impl IntoIterator<Item = (&'a str, &'a Value)>,
    max_output_bytes: usize,
) -> Result<SnapshotRef> {
    if output_bytes.len() > max_output_bytes {
        bail!("checkpoint output exceeds maximum of {max_output_bytes} bytes");
    }
    let output = store_blob(blobs_directory, output_bytes, max_output_bytes)?;
    let mut binding_refs = BTreeMap::new();
    for (name, value) in bindings {
        if value_size(value) > max_output_bytes {
            bail!("checkpoint binding {name:?} exceeds maximum of {max_output_bytes} bytes");
        }
        let bytes = serde_json::to_vec(value)
            .with_context(|| format!("encoding checkpoint binding {name:?}"))?;
        let encoded_limit = max_output_bytes.saturating_mul(6).saturating_add(16);
        binding_refs.insert(
            name.to_owned(),
            store_blob(blobs_directory, &bytes, encoded_limit)?,
        );
    }
    Ok(SnapshotRef {
        cwd: EncodedPath::from_path(cwd),
        output,
        bindings: binding_refs,
    })
}

fn load_snapshot(
    blobs_directory: &Path,
    snapshot: &SnapshotRef,
    max_output_bytes: usize,
) -> Result<ExecutionState> {
    let output = load_blob(blobs_directory, &snapshot.output, max_output_bytes)?;
    let encoded_limit = max_output_bytes.saturating_mul(6).saturating_add(16);
    let mut bindings = BTreeMap::new();
    for (name, reference) in &snapshot.bindings {
        let bytes = load_blob(blobs_directory, reference, encoded_limit)
            .with_context(|| format!("loading checkpoint binding {name:?}"))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding checkpoint binding {name:?}"))?;
        if value_size(&value) > max_output_bytes {
            bail!("checkpoint binding {name:?} exceeds maximum of {max_output_bytes} bytes");
        }
        bindings.insert(name.clone(), value);
    }
    Ok(ExecutionState {
        cwd: snapshot.cwd.to_path(),
        output,
        bindings,
    })
}

fn value_size(value: &Value) -> usize {
    match value {
        Value::String(value) | Value::Reference(value) => value.len(),
        Value::Integer(_) => std::mem::size_of::<i64>(),
        Value::Bool(_) => 1,
        Value::Null => 0,
        Value::Number(value) => value.to_string().len(),
        Value::Record(values) => values.iter().fold(0usize, |size, (key, value)| {
            size.saturating_add(key.len())
                .saturating_add(value_size(value))
        }),
        Value::List(values) => values
            .iter()
            .fold(0usize, |size, item| size.saturating_add(value_size(item))),
    }
}

fn store_blob(directory: &Path, bytes: &[u8], limit: usize) -> Result<BlobRef> {
    if bytes.len() > limit {
        bail!("checkpoint blob exceeds maximum of {limit} bytes");
    }
    let digest = hex_digest(bytes);
    let path = directory.join(&digest);
    if path.exists() {
        let reference = BlobRef {
            sha256: digest,
            length: bytes.len().try_into()?,
        };
        verify_existing_blob(directory, &reference, bytes, limit)?;
        return Ok(reference);
    }
    atomic_write(directory, &digest, "blob", bytes)?;
    Ok(BlobRef {
        sha256: digest,
        length: bytes.len().try_into()?,
    })
}

fn verify_existing_blob(
    directory: &Path,
    reference: &BlobRef,
    expected: &[u8],
    limit: usize,
) -> Result<()> {
    if reference.length > limit.try_into()? {
        bail!("checkpoint blob exceeds maximum of {limit} bytes");
    }
    let path = directory.join(&reference.sha256);
    let file = open_regular(&path, "checkpoint blob", false)?;
    if file.metadata()?.len() != reference.length {
        bail!("checkpoint blob length mismatch at {}", path.display());
    }

    let read_limit = limit.saturating_add(1).try_into().unwrap_or(u64::MAX);
    let mut file = file.take(read_limit);
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut offset = 0usize;
    let mut matches_expected = true;
    loop {
        let count = match file.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if count == 0 {
            break;
        }
        let Some(end) = offset.checked_add(count) else {
            bail!("checkpoint blob exceeds maximum of {limit} bytes");
        };
        if end > limit {
            bail!("checkpoint blob exceeds maximum of {limit} bytes");
        }
        digest.update(&buffer[..count]);
        if expected.get(offset..end) != Some(&buffer[..count]) {
            matches_expected = false;
        }
        offset = end;
    }
    if offset as u64 != reference.length || format!("{:x}", digest.finalize()) != reference.sha256 {
        bail!("checkpoint blob checksum mismatch at {}", path.display());
    }
    if !matches_expected {
        bail!(
            "checkpoint blob collision or corruption at {}",
            path.display()
        );
    }
    Ok(())
}

fn load_blob(directory: &Path, reference: &BlobRef, limit: usize) -> Result<Vec<u8>> {
    if reference.length > limit.try_into()? {
        bail!("checkpoint blob exceeds maximum of {limit} bytes");
    }
    if reference.sha256.len() != 64
        || !reference
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("checkpoint contains an invalid blob digest");
    }
    let path = directory.join(&reference.sha256);
    let file = open_regular(&path, "checkpoint blob", false)?;
    if file.metadata()?.len() != reference.length {
        bail!("checkpoint blob length mismatch at {}", path.display());
    }
    let mut bytes = Vec::with_capacity(reference.length.try_into()?);
    file.take(limit.saturating_add(1).try_into().unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("checkpoint blob exceeds maximum of {limit} bytes");
    }
    if bytes.len() as u64 != reference.length || hex_digest(&bytes) != reference.sha256 {
        bail!("checkpoint blob checksum mismatch at {}", path.display());
    }
    Ok(bytes)
}

fn atomic_write(directory: &Path, stem: &str, suffix: &str, bytes: &[u8]) -> Result<()> {
    atomic_write_with(directory, stem, suffix, bytes, |file, bytes| {
        file.write_all(bytes).and_then(|()| file.sync_all())
    })
}

fn atomic_write_with(
    directory: &Path,
    stem: &str,
    suffix: &str,
    bytes: &[u8],
    mut write_and_sync: impl FnMut(&mut File, &[u8]) -> io::Result<()>,
) -> Result<()> {
    let target = if suffix == "json" {
        directory.join(format!("{stem}.{suffix}"))
    } else {
        directory.join(stem)
    };
    let mut temporary = None;
    for _ in 0..100 {
        let number = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".{stem}-{}-{number}.tmp", std::process::id()));
        match crate::platform::private_options()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(error) = write_and_sync(&mut file, bytes) {
                    // Close before unlinking so cleanup also works on Windows.
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error).with_context(|| {
                        format!("writing checkpoint temporary {}", path.display())
                    });
                }
                temporary = Some(path);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating checkpoint temporary file"),
        }
    }
    let temporary =
        temporary.ok_or_else(|| anyhow!("could not allocate checkpoint temporary file"))?;
    if let Err(error) = crate::platform::replace_file(&temporary, &target) {
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("publishing checkpoint file {}", target.display()));
    }
    sync_directory(directory)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)
        .with_context(|| format!("opening directory {} for sync", directory.display()))?
        .sync_all()
        .with_context(|| format!("syncing directory {}", directory.display()))
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn serialized_digest<T: Serialize>(value: &T) -> Result<String> {
    let mut digest = DigestWriter::default();
    serde_json::to_writer(&mut digest, value)?;
    Ok(format!("{:x}", digest.0.finalize()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, CheckpointOptions, PathBuf) {
        let temp = tempdir().unwrap();
        let program_path = temp.path().join("flow.txt");
        fs::write(&program_path, "workflow").unwrap();
        let initial_cwd = temp.path().join("work");
        fs::create_dir(&initial_cwd).unwrap();
        let options = CheckpointOptions {
            directory: temp.path().join("checkpoint"),
            program_path,
            resume: false,
            retry_uncertain: false,
        };
        (temp, options, initial_cwd)
    }

    fn state(cwd: &Path, output: &[u8], binding: &str) -> ExecutionState {
        ExecutionState {
            cwd: fs::canonicalize(cwd).unwrap(),
            output: output.to_vec(),
            bindings: BTreeMap::from([("result".to_owned(), Value::String(binding.to_owned()))]),
        }
    }

    fn obstruct_journal_target(options: &CheckpointOptions) -> JournalData {
        let journal_path = options.directory.join("journal.json");
        let backup_path = options.directory.join("journal-before-failed-write.json");
        fs::copy(&journal_path, &backup_path).unwrap();
        let previous = read_journal(&backup_path).unwrap();
        fs::remove_file(&journal_path).unwrap();
        fs::create_dir(&journal_path).unwrap();
        previous
    }

    #[test]
    fn checkpoint_file_open_rejects_symlinks_and_special_files_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::write(&target, b"unchanged").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(open_regular(&link, "checkpoint file", true).is_err());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
        let fifo = temp.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open_regular(&fifo, "checkpoint file", true).is_err());
        assert!(open_regular(temp.path(), "checkpoint file", false).is_err());
        let mut regular = open_regular(&target, "checkpoint file", false).unwrap();
        let mut bytes = Vec::new();
        regular.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"unchanged");
    }

    #[test]
    fn listener_event_cursor_is_persisted_before_handler_and_committed_after_emit() {
        let (_temp, mut options, cwd) = setup();
        let trigger_hash = "0123456789abcdef";
        let mut journal =
            Journal::open(&options, &cwd, &format!("service:{trigger_hash}"), 1024).unwrap();
        let event = b"event one\n";
        let event_digest = hex_digest(event);
        let binding = Value::String(String::from_utf8(event.to_vec()).unwrap());
        journal
            .begin_listener_event_parts(&cwd, event, [("event", &binding)])
            .unwrap();
        let cursor = journal.listener_event_cursor().unwrap();
        assert_eq!(cursor.accepted_sequence, 1);
        assert_eq!(cursor.committed_sequence, 0);
        assert_eq!(
            cursor.pending_sha256.as_deref(),
            Some(event_digest.as_str())
        );
        assert!(journal
            .begin_execution_parts(&cwd, b"wrong next run", std::iter::empty())
            .is_err());
        drop(journal);

        // A crash after durable admission restores the exact pending event.
        options.resume = true;
        let mut resumed =
            Journal::open(&options, &cwd, &format!("service:{trigger_hash}"), 1024).unwrap();
        let restored = resumed.take_restored_state().unwrap();
        assert_eq!(restored.output, event);
        assert_eq!(restored.bindings.get("event"), Some(&binding));
        let cursor = resumed.listener_event_cursor().unwrap();
        assert_eq!(cursor.accepted_sequence, 1);
        assert_eq!(cursor.committed_sequence, 0);

        // The completed handler and successful sink delivery advance the
        // local cursor together when finish persists the idle checkpoint.
        resumed.begin_node("handler", 0, 1, 1, &cwd, true).unwrap();
        resumed
            .complete_node(&state(&cwd, b"handled", "handled"))
            .unwrap();
        resumed.finish().unwrap();
        let cursor = resumed.listener_event_cursor().unwrap();
        assert_eq!(cursor.accepted_sequence, 1);
        assert_eq!(cursor.committed_sequence, 1);
        assert!(cursor.pending_sha256.is_none());
        assert_eq!(
            cursor.last_committed_sha256.as_deref(),
            Some(event_digest.as_str())
        );
    }

    #[test]
    fn checkpoint_v1_without_cursor_migrates_to_v2_on_explicit_resume() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal.data.version = LEGACY_JOURNAL_VERSION;
        journal.persist().unwrap();
        drop(journal);

        options.resume = true;
        let journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        assert_eq!(journal.data.version, JOURNAL_VERSION);
        assert_eq!(journal.listener_event_cursor(), None);
        let persisted = read_journal(&options.directory.join("journal.json")).unwrap();
        assert_eq!(persisted.version, JOURNAL_VERSION);
    }

    #[test]
    fn listener_cursor_is_rejected_outside_service_checkpoints() {
        let (_temp, options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal.data.listener_cursor = Some(ListenerEventCursor {
            accepted_sequence: 0,
            committed_sequence: 0,
            pending_sha256: None,
            last_committed_sha256: None,
        });
        journal.persist().unwrap();
        assert!(read_journal(&options.directory.join("journal.json")).is_err());
    }

    #[test]
    fn finite_journal_restores_completed_snapshot_and_refuses_implicit_reuse() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        assert!(journal.has_pending());
        assert_eq!(
            journal.restored_state().unwrap(),
            ExecutionState {
                cwd: fs::canonicalize(&cwd).unwrap(),
                output: Vec::new(),
                bindings: BTreeMap::new(),
            }
        );
        journal.begin_node("node-1", 0, 2, 1, &cwd, true).unwrap();
        let completed_state = state(&cwd, b"saved bytes", "named bytes");
        journal.complete_node(&completed_state).unwrap();
        journal.finish().unwrap();
        assert!(journal.is_finished());
        assert_eq!(journal.completed_nodes().len(), 1);
        assert_eq!(journal.restored_state().unwrap(), completed_state);
        drop(journal);

        assert!(Journal::open(&options, &cwd, "finite", 1024).is_err());
        options.resume = true;
        let resumed = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        assert!(resumed.is_finished());
        assert_eq!(resumed.restored_state().unwrap(), completed_state);
    }

    #[test]
    fn persisted_checksum_matches_canonical_journal_data() {
        let (_temp, options, cwd) = setup();
        let _journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        let bytes = fs::read(options.directory.join("journal.json")).unwrap();
        let envelope: JournalEnvelope = serde_json::from_slice(&bytes).unwrap();
        let canonical = serde_json::to_vec(&envelope.data).unwrap();
        assert_eq!(envelope.checksum, hex_digest(&canonical));
    }

    #[test]
    fn existing_blob_reuse_streams_and_still_detects_corruption() {
        let temp = tempdir().unwrap();
        let directory = temp.path().join("blobs");
        fs::create_dir(&directory).unwrap();
        let bytes = b"content-addressed payload";
        let first = store_blob(&directory, bytes, 1024).unwrap();
        let second = store_blob(&directory, bytes, 1024).unwrap();
        assert_eq!(first, second);

        let path = directory.join(&first.sha256);
        fs::write(&path, b"corrupt-addressed payload").unwrap();
        let error = store_blob(&directory, bytes, 1024).unwrap_err();
        assert!(format!("{error:#}").contains("checksum mismatch"));
    }

    #[test]
    fn completing_a_resumed_node_invalidates_the_preloaded_snapshot() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        let first = state(&cwd, b"first", "binding one");
        journal.begin_node("first", 0, 1, 1, &cwd, true).unwrap();
        journal.complete_node(&first).unwrap();
        journal.begin_node("second", 1, 2, 2, &cwd, true).unwrap();
        drop(journal);

        options.resume = true;
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        assert!(journal.preloaded_state.is_some());
        let second = state(&cwd, b"second", "binding two");
        journal.complete_node(&second).unwrap();
        assert_eq!(journal.take_restored_state().unwrap(), second);
    }

    #[test]
    fn begin_node_persist_failure_restores_completed_prefix() {
        let (_temp, options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        let completed = state(&cwd, b"prefix output", "prefix binding");
        journal.begin_node("prefix", 0, 1, 1, &cwd, true).unwrap();
        journal.complete_node(&completed).unwrap();
        let prefix = journal.completed_nodes().to_vec();

        let durable = obstruct_journal_target(&options);
        assert_eq!(durable.completed, prefix);
        assert!(durable.in_flight.is_none());
        assert!(journal.begin_node("next", 1, 2, 2, &cwd, false).is_err());

        assert!(journal.persistence_failed());
        assert_eq!(journal.data.status, durable.status);
        assert_eq!(journal.data.error, durable.error);
        assert_eq!(journal.data.in_flight, durable.in_flight);
        assert_eq!(journal.completed_nodes(), prefix);
        assert_eq!(journal.restored_state().unwrap(), completed);
    }

    #[test]
    fn complete_node_persist_failure_preserves_prefix_and_uncertain_intent() {
        let (_temp, options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        let completed = state(&cwd, b"prefix output", "prefix binding");
        journal.begin_node("prefix", 0, 1, 1, &cwd, true).unwrap();
        journal.complete_node(&completed).unwrap();
        journal
            .begin_node("uncertain effect", 1, 2, 2, &cwd, false)
            .unwrap();
        let prefix = journal.completed_nodes().to_vec();
        let intent = journal.data.in_flight.clone().unwrap();

        let durable = obstruct_journal_target(&options);
        assert_eq!(durable.completed, prefix);
        assert_eq!(durable.in_flight.as_ref(), Some(&intent));
        assert!(journal
            .complete_node(&state(&cwd, b"effect output", "effect binding"))
            .is_err());

        assert!(journal.persistence_failed());
        assert_eq!(journal.data.status, durable.status);
        assert_eq!(journal.data.error, durable.error);
        assert_eq!(journal.completed_nodes(), prefix);
        assert_eq!(journal.data.in_flight.as_ref(), Some(&intent));
        assert_eq!(journal.restored_state().unwrap(), completed);
        assert!(journal.ensure_retry_allowed().is_err());
    }

    #[test]
    fn fail_persist_failure_restores_running_uncertain_intent() {
        let (_temp, options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal
            .begin_node("uncertain effect", 0, 1, 1, &cwd, false)
            .unwrap();
        let intent = journal.data.in_flight.clone().unwrap();

        let durable = obstruct_journal_target(&options);
        assert_eq!(durable.status, Status::Running);
        assert!(durable.error.is_none());
        assert_eq!(durable.in_flight.as_ref(), Some(&intent));
        assert!(journal.fail("cancelled", true).is_err());

        assert!(journal.persistence_failed());
        assert_eq!(journal.data.status, Status::Running);
        assert!(journal.data.error.is_none());
        assert_eq!(journal.data.in_flight.as_ref(), Some(&intent));
        assert!(journal.ensure_retry_allowed().is_err());
    }

    #[test]
    fn finish_persist_failure_restores_running_completed_prefix() {
        let (_temp, options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        let completed = state(&cwd, b"prefix output", "prefix binding");
        journal.begin_node("prefix", 0, 1, 1, &cwd, true).unwrap();
        journal.complete_node(&completed).unwrap();
        let prefix = journal.completed_nodes().to_vec();

        let durable = obstruct_journal_target(&options);
        assert_eq!(durable.status, Status::Running);
        assert_eq!(durable.completed, prefix);
        assert!(journal.finish().is_err());

        assert!(journal.persistence_failed());
        assert_eq!(journal.data.status, Status::Running);
        assert_eq!(journal.data.error, durable.error);
        assert_eq!(journal.completed_nodes(), prefix);
        assert_eq!(journal.restored_state().unwrap(), completed);
    }

    #[test]
    fn begin_execution_persist_failure_restores_idle_service_snapshot() {
        let (_temp, options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "service:trigger-hash", 1024).unwrap();
        let completed = state(&cwd, b"previous event", "previous binding");
        journal.begin_execution(&completed).unwrap();
        journal.begin_node("handler", 0, 1, 1, &cwd, true).unwrap();
        journal.complete_node(&completed).unwrap();
        journal.finish().unwrap();
        let prefix = journal.completed_nodes().to_vec();

        let durable = obstruct_journal_target(&options);
        assert_eq!(durable.status, Status::Idle);
        assert_eq!(durable.completed, prefix);
        assert!(durable.in_flight.is_none());
        let next_event = state(&cwd, b"next event", "next binding");
        assert!(journal.begin_execution(&next_event).is_err());

        assert!(journal.persistence_failed());
        assert_eq!(journal.data.status, Status::Idle);
        assert_eq!(journal.data.error, durable.error);
        assert_eq!(journal.data.in_flight, durable.in_flight);
        assert_eq!(journal.completed_nodes(), prefix);
        assert_eq!(journal.restored_state().unwrap(), completed);
    }

    #[test]
    fn completed_node_preserves_non_utf8_unix_working_directory() {
        use std::os::unix::ffi::OsStringExt;

        let cwd = PathBuf::from(std::ffi::OsString::from_vec(b"work-\xff".to_vec()));
        let node = CompletedNode {
            fingerprint: "non-utf8".to_owned(),
            start: 0,
            end: 1,
            cwd_before: cwd.clone(),
        };
        let serialized = serde_json::to_vec(&node).unwrap();
        let restored: CompletedNode = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(restored.cwd_before, cwd);
    }

    #[test]
    fn lock_is_exclusive_and_identity_must_match() {
        let (_temp, mut options, cwd) = setup();
        let journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        options.resume = true;
        assert!(Journal::open(&options, &cwd, "finite", 1024).is_err());
        drop(journal);
        assert!(Journal::open(&options, &cwd, "service:other", 1024).is_err());
        assert!(Journal::open(&options, &cwd, "finite", 1024).is_ok());
    }

    #[test]
    fn interrupted_unsafe_node_requires_explicit_retry() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal.begin_node("effect", 0, 1, 1, &cwd, false).unwrap();
        journal.fail("interrupted", true).unwrap();
        drop(journal);

        options.resume = true;
        let journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        assert!(journal.ensure_retry_allowed().is_err());
        drop(journal);

        options.retry_uncertain = true;
        let journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        assert!(journal.ensure_retry_allowed().is_ok());

        drop(journal);
        let (_temp, mut running_options, running_cwd) = setup();
        let mut journal = Journal::open(&running_options, &running_cwd, "finite", 1024).unwrap();
        journal
            .begin_node("crash-window", 0, 1, 1, &running_cwd, false)
            .unwrap();
        drop(journal); // A process crash leaves durable status Running.
        running_options.resume = true;
        let journal = Journal::open(&running_options, &running_cwd, "finite", 1024).unwrap();
        assert!(journal.ensure_retry_allowed().is_err());
    }

    #[test]
    fn interruption_without_unsafe_in_flight_node_needs_no_override() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal.fail("cancelled between nodes", true).unwrap();
        drop(journal);

        options.resume = true;
        let journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        assert!(journal.ensure_retry_allowed().is_ok());
        drop(journal);

        options.retry_uncertain = false;
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal
            .begin_node("read-only", 0, 1, 1, &cwd, true)
            .unwrap();
        journal.fail("cancelled read-only node", true).unwrap();
        assert!(journal.ensure_retry_allowed().is_ok());
    }

    #[test]
    fn failed_completion_persistence_keeps_durable_in_flight_intent() {
        let (_temp, mut options, cwd) = setup();
        // Keep the journal's output cap small so snapshot creation fails after the
        // node ran, before complete_node can clear its in-flight record.
        let mut journal = Journal::open(&options, &cwd, "finite", 2).unwrap();
        journal
            .begin_node("external-effect", 0, 1, 1, &cwd, false)
            .unwrap();
        assert!(journal
            .complete_node(&state(&cwd, b"too large", "x"))
            .is_err());
        assert!(journal.fail("completion failed", false).is_err());
        drop(journal);

        options.resume = true;
        let journal = Journal::open(&options, &cwd, "finite", 2).unwrap();
        assert_eq!(journal.data.status, Status::Running);
        assert_eq!(
            journal
                .data
                .in_flight
                .as_ref()
                .map(|node| node.fingerprint.as_str()),
            Some("external-effect")
        );
    }

    #[test]
    fn known_failure_allows_explicit_resume_without_uncertain_override() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal.begin_node("effect", 0, 1, 1, &cwd, false).unwrap();
        journal.fail("command returned nonzero", false).unwrap();
        drop(journal);

        options.resume = true;
        let journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        assert!(journal.ensure_retry_allowed().is_ok());
    }

    #[test]
    fn service_execution_resets_per_event_journal_and_returns_idle() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "service:trigger-hash", 1024).unwrap();
        assert!(journal.is_finished());
        let event = state(&cwd, b"event\n", "event\n");
        journal.begin_execution(&event).unwrap();
        journal.begin_node("handler", 0, 1, 3, &cwd, true).unwrap();
        let completed = state(&cwd, b"handled\n", "event\n");
        journal.complete_node(&completed).unwrap();
        journal.finish().unwrap();
        assert!(journal.is_finished());
        assert_eq!(journal.completed_nodes().len(), 1);
        assert_eq!(journal.restored_state().unwrap(), completed);
        drop(journal);

        options.resume = true;
        let journal = Journal::open(&options, &cwd, "service:trigger-hash", 1024).unwrap();
        assert!(journal.is_finished());
        assert_eq!(journal.completed_nodes().len(), 1);
    }

    #[test]
    fn service_finish_collects_old_event_blobs_but_keeps_current_snapshot() {
        let (_temp, options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "service:trigger-hash", 1024).unwrap();
        let first = state(&cwd, b"first event output", "first event binding");
        journal.begin_execution(&first).unwrap();
        journal
            .begin_node("handler-1", 0, 1, 1, &cwd, true)
            .unwrap();
        journal.complete_node(&first).unwrap();
        journal.finish().unwrap();
        let old_output = journal.data.snapshot.output.sha256.clone();
        let old_binding = journal.data.snapshot.bindings["result"].sha256.clone();

        let second = state(&cwd, b"second event output", "second event binding");
        journal.begin_execution(&second).unwrap();
        journal
            .begin_node("handler-2", 0, 1, 1, &cwd, true)
            .unwrap();
        journal.complete_node(&second).unwrap();
        journal.finish().unwrap();

        let current_output = journal.data.snapshot.output.sha256.clone();
        let current_binding = journal.data.snapshot.bindings["result"].sha256.clone();
        assert!(!journal.blobs_directory.join(old_output).exists());
        assert!(!journal.blobs_directory.join(old_binding).exists());
        assert!(journal.blobs_directory.join(current_output).is_file());
        assert!(journal.blobs_directory.join(current_binding).is_file());
        assert_eq!(journal.restored_state().unwrap(), second);
    }

    #[test]
    fn corrupt_journal_is_reported() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal.begin_node("node", 0, 1, 1, &cwd, true).unwrap();
        journal
            .complete_node(&state(&cwd, b"output", "binding"))
            .unwrap();
        let directory = options.directory.clone();
        drop(journal);

        let journal_path = directory.join("journal.json");
        let mut bytes = fs::read(&journal_path).unwrap();
        bytes[0] ^= 1;
        fs::write(&journal_path, bytes).unwrap();
        options.resume = true;
        assert!(Journal::open(&options, &cwd, "finite", 1024).is_err());
    }

    #[test]
    fn corrupt_blob_is_reported() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal.begin_node("node", 0, 1, 1, &cwd, true).unwrap();
        journal
            .complete_node(&state(&cwd, b"output", "binding"))
            .unwrap();
        let output_blob = journal.data.snapshot.output.sha256.clone();
        let blob_path = options.directory.join("blobs").join(output_blob);
        drop(journal);

        fs::write(blob_path, b"broken").unwrap();
        options.resume = true;
        assert!(Journal::open(&options, &cwd, "finite", 1024).is_err());
    }

    #[test]
    fn missing_resume_and_oversized_snapshot_are_rejected() {
        let (_temp, mut options, cwd) = setup();
        options.resume = true;
        assert!(Journal::open(&options, &cwd, "finite", 1024).is_err());
        options.resume = false;
        let mut journal = Journal::open(&options, &cwd, "finite", 2).unwrap();
        journal.begin_node("node", 0, 1, 1, &cwd, true).unwrap();
        assert!(journal
            .complete_node(&state(&cwd, b"too long", "x"))
            .is_err());
    }
    #[test]
    fn removing_an_uncertain_tail_requires_explicit_resume_and_retry() {
        let (_temp, mut options, cwd) = setup();
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal.begin_node("effect", 0, 1, 1, &cwd, false).unwrap();
        assert!(journal.discard_removed_tail().is_err());
        journal.fail("cancelled", true).unwrap();
        drop(journal);
        options.resume = true;
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        assert!(journal.discard_removed_tail().is_err());
        assert!(journal.discard_removed_tail().is_err());
        drop(journal);
        options.retry_uncertain = true;
        let mut journal = Journal::open(&options, &cwd, "finite", 1024).unwrap();
        journal.discard_removed_tail().unwrap();
        journal.finish().unwrap();
        assert!(journal.is_finished());
    }
}

fn restrict_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .context("restricting checkpoint directory permissions")
    }
    #[cfg(windows)]
    {
        crate::platform::windows::restrict_directory(path)
    }
}
fn restrict_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("restricting checkpoint file permissions")
    }
    #[cfg(windows)]
    {
        crate::platform::windows::restrict_file(file)
    }
}
#[cfg(windows)]
fn sync_directory(_directory: &Path) -> Result<()> {
    // Publication uses MOVEFILE_WRITE_THROUGH after flushing each file. Windows
    // does not provide the Unix directory-fsync contract through std::fs.
    Ok(())
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn checkpoint_atomic_write_removes_temporary_after_write_error() {
        let temp = tempdir().unwrap();
        let directory = temp.path();
        let target = directory.join("journal.json");
        fs::write(&target, b"previous journal").unwrap();

        let error = atomic_write_with(directory, "journal", "json", b"replacement", |file, _| {
            file.write_all(b"partial journal")?;
            Err(io::Error::other("synthetic checkpoint write failure"))
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("writing checkpoint temporary"));
        assert_eq!(fs::read(&target).unwrap(), b"previous journal");
        assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
    }

    #[test]
    fn checkpoint_atomic_write_removes_temporary_after_sync_error() {
        let temp = tempdir().unwrap();
        let directory = temp.path();
        let target = directory.join("journal.json");
        fs::write(&target, b"previous journal").unwrap();

        let error = atomic_write_with(
            directory,
            "journal",
            "json",
            b"replacement",
            |file, bytes| {
                file.write_all(bytes)?;
                Err(io::Error::other("synthetic checkpoint sync failure"))
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("writing checkpoint temporary"));
        assert_eq!(fs::read(&target).unwrap(), b"previous journal");
        assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
    }
}
