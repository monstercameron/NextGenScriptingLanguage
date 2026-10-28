//! Durable, file-only multi-target changes with explicit compensation.
//!
//! This is a recovery protocol, not a filesystem-wide atomic transaction.
//! Files become visible one at a time. The journal makes an interrupted run
//! inspectable and lets a caller explicitly complete or compensate it when the
//! current contents still match one of the recorded states.

use super::filesystem::{self, PathLock};
use super::{checkpoint_work, failed, invalid, required_string, AppletError, ExecutionContext};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const VERSION: u32 = 1;
const MAX_FILES: usize = 32;
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;
const DEFAULT_LOCK_TIMEOUT_MS: u64 = 30_000;
const MAX_LOCK_TIMEOUT_MS: u64 = 300_000;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransactionState {
    Prepared,
    Applying,
    RollingBack,
    Committed,
    RolledBack,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryState {
    Pending,
    Applying,
    Applied,
    Compensating,
    Compensated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalData {
    version: u32,
    state: TransactionState,
    entries: Vec<JournalEntry>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEntry {
    path: EncodedPath,
    before_hash: String,
    after_hash: String,
    /// Unix permission bits captured with the before image. Other platform
    /// metadata is intentionally not represented by this file-content journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before_mode: Option<u32>,
    after_exists: bool,
    before_blob: BlobRef,
    after_blob: BlobRef,
    state: EntryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobRef {
    name: String,
    sha256: String,
    length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    checksum: String,
    data: JournalData,
}

#[derive(Debug, Clone, Serialize)]
struct EnvelopeRef<'a> {
    checksum: String,
    data: &'a JournalData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EncodedPath(Vec<u8>);

impl<'de> Deserialize<'de> for EncodedPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        #[cfg(windows)]
        if bytes.len() % 2 != 0 {
            return Err(serde::de::Error::custom(
                "transaction path must contain complete UTF-16 units",
            ));
        }
        Ok(Self(bytes))
    }
}

impl EncodedPath {
    fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
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
        #[cfg(not(any(unix, windows)))]
        {
            Self(path.to_string_lossy().as_bytes().to_vec())
        }
    }

    fn to_path(&self) -> Result<PathBuf, AppletError> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            Ok(PathBuf::from(std::ffi::OsString::from_vec(self.0.clone())))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let units: Vec<u16> = self
                .0
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
                .collect();
            Ok(PathBuf::from(std::ffi::OsString::from_wide(&units)))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let text = std::str::from_utf8(&self.0)
                .map_err(|_| invalid("transaction path encoding is invalid"))?;
            Ok(PathBuf::from(text))
        }
    }
}

struct PlannedEntry {
    path: PathBuf,
    requested_path: String,
    expected_hash: String,
    after: Vec<u8>,
    after_exists: bool,
}

struct LoadedEntry {
    path: PathBuf,
    before: Vec<u8>,
    before_mode: Option<u32>,
    after: Vec<u8>,
    after_exists: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CurrentState {
    Before,
    After,
    Conflict,
}

/// Apply a bounded set of existing-file replacements and removals. A
/// successful result means every requested state was observed and the journal
/// says `committed`.
pub(super) fn apply(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    #[cfg(test)]
    {
        apply_with_fault(args, cwd, context, None, None)
    }
    #[cfg(not(test))]
    {
        apply_with_fault(args, cwd, context, None)
    }
}

fn apply_with_fault(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
    #[cfg(test)] crash_after_replace: Option<usize>,
    #[cfg(not(test))] _crash_after_replace: Option<usize>,
    #[cfg(test)] before_windows_target_preflight: Option<&dyn Fn(&Path)>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let timeout = lock_timeout(args)?;
    let journal = journal_path(required_string(args, "journal")?, cwd)?;
    let operations = parse_operations(args, cwd, context)?;
    if operations.is_empty() || operations.len() > MAX_FILES {
        return Err(invalid(format!(
            "args.operations must contain 1 through {MAX_FILES} file changes"
        )));
    }

    reject_path_collisions(&journal, &operations)?;
    #[cfg(not(windows))]
    let _target_locks = lock_targets(
        operations.iter().map(|operation| operation.path.as_path()),
        timeout,
        context,
    )?;

    let mut loaded = Vec::with_capacity(operations.len());
    let mut total = 0usize;
    for operation in operations {
        context.checkpoint()?;
        let (before, before_mode) = read_existing_regular_with_mode(&operation.path, context)?;
        let before_hash = digest_with_context(&before, context)?;
        if before_hash != operation.expected_hash {
            return Err(failed(
                "conflict",
                format!(
                    "expected content hash for {:?} is {}, found {}",
                    operation.requested_path, operation.expected_hash, before_hash
                ),
            ));
        }
        total = total
            .checked_add(before.len())
            .and_then(|value| value.checked_add(operation.after.len()))
            .ok_or_else(|| invalid("transaction payload size overflow"))?;
        if total > MAX_TOTAL_BYTES {
            return Err(invalid(format!(
                "transaction before/after content exceeds {MAX_TOTAL_BYTES} bytes"
            )));
        }
        loaded.push(LoadedEntry {
            path: operation.path,
            before,
            before_mode,
            after: operation.after,
            after_exists: operation.after_exists,
        });
    }

    let (stage, published) = create_journal_stage(&journal)?;
    let mut cleanup = DirectoryCleanup(Some(stage.clone()));
    let entries = write_blobs_and_entries(&stage, &loaded, context)?;
    let mut data = JournalData {
        version: VERSION,
        state: TransactionState::Prepared,
        entries,
        error: None,
    };
    persist_journal(&stage, &data, context)?;
    sync_directory(&stage).map_err(|error| {
        super::io_error(
            error,
            format!("syncing transaction journal {}", stage.display()),
        )
    })?;

    // Keep the lock attached to the journal across execution. Windows cannot
    // publish a directory while a child lock file is open, so it claims the
    // final name first and then verifies the prepared checksum under the lock.
    #[cfg(windows)]
    let _journal_lock = {
        fs::rename(&stage, &published).map_err(|error| {
            super::io_error(
                error,
                format!("publishing transaction journal {}", published.display()),
            )
        })?;
        cleanup.0 = None;
        let lock = lock_published_prepared_journal(&published, &data, timeout, context)?;
        sync_directory(published.parent().unwrap_or_else(|| Path::new("."))).map_err(|error| {
            failed(
                "uncertain_effect",
                format!(
                    "journal was published but its parent could not be synced: {}: {error}",
                    published.display()
                ),
            )
        })?;
        lock
    };
    #[cfg(not(windows))]
    let _journal_lock = {
        let lock =
            filesystem::acquire_path_lock(&stage.join("transaction.lock"), timeout, context)?;
        fs::rename(&stage, &published).map_err(|error| {
            super::io_error(
                error,
                format!("publishing transaction journal {}", published.display()),
            )
        })?;
        cleanup.0 = None;
        sync_directory(published.parent().unwrap_or_else(|| Path::new("."))).map_err(|error| {
            failed(
                "uncertain_effect",
                format!(
                    "journal was published but its parent could not be synced: {}: {error}",
                    published.display()
                ),
            )
        })?;
        lock
    };

    #[cfg(all(test, windows))]
    if let Some(before_preflight) = before_windows_target_preflight {
        before_preflight(&published);
    }
    #[cfg(all(test, not(windows)))]
    let _ = before_windows_target_preflight;

    #[cfg(windows)]
    let _target_locks = {
        let locks = lock_targets(
            loaded.iter().map(|entry| entry.path.as_path()),
            timeout,
            context,
        )
            .map_err(|error| AppletError {
                code: error.code,
                kind: error.kind,
                message: format!(
                    "acquiring target locks for transaction journal {}; the published prepared journal remains for explicit recovery: {}",
                    published.display(),
                    error.message
                ),
            })?;
        verify_loaded_targets_unchanged(&published, &loaded, context)?;
        locks
    };

    data.state = TransactionState::Applying;
    persist_journal(&published, &data, context)?;
    #[allow(unused_mut)]
    let mut replaced = 0usize;
    #[cfg(not(test))]
    let _ = replaced;
    for index in 0..data.entries.len() {
        context.checkpoint()?;
        data.entries[index].state = EntryState::Applying;
        persist_journal(&published, &data, context)?;
        let path = data.entries[index].path.to_path()?;
        let after = &loaded[index].after;
        let before_hash = data.entries[index].before_hash.clone();
        let (permission_policy, mode) = loaded[index]
            .before_mode
            .map(|mode| ("exact", Some(mode)))
            .unwrap_or(("preserve", None));
        let outcome = if data.entries[index].after_exists {
            filesystem::atomic_replace_locked(
                &path,
                after,
                &before_hash,
                "reject",
                permission_policy,
                mode,
                context,
            )
            .map(|_| ())
        } else {
            remove_locked(&path, &before_hash, context)
        };
        match outcome {
            Ok(_) => {
                #[cfg(test)]
                {
                    replaced += 1;
                    if crash_after_replace == Some(replaced) {
                        return Err(failed(
                            "injected_crash",
                            format!("simulated crash after applying {}", path.display()),
                        ));
                    }
                }
                data.entries[index].state = EntryState::Applied;
                persist_journal(&published, &data, context)?;
            }
            Err(error) => {
                data.error = Some(error.to_string());
                data.state = TransactionState::Partial;
                persist_journal(&published, &data, context)?;
                let rollback =
                    drive_journal(&published, &mut data, &loaded, Decision::Rollback, context);
                return match rollback {
                    Ok(()) => Err(failed(
                        "transaction_rolled_back",
                        format!(
                            "file transaction failed and recorded changes were compensated; journal: {}; cause: {error}",
                            published.display()
                        ),
                    )),
                    Err(rollback_error) => Err(failed(
                        "manual_recovery",
                        format!(
                            "file transaction failed and compensation is incomplete; inspect/recover journal {}: operation error: {error}; recovery error: {rollback_error}",
                            published.display()
                        ),
                    )),
                };
            }
        }
    }
    data.state = TransactionState::Committed;
    data.error = None;
    persist_journal(&published, &data, context)?;
    Ok(receipt(&published, &data, "committed"))
}

/// Explicitly complete or compensate a previously published file journal.
pub(super) fn recover(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let timeout = lock_timeout(args)?;
    let journal = journal_path(required_string(args, "journal")?, cwd)?;
    let decision = match required_string(args, "decision")? {
        "complete" => Decision::Complete,
        "rollback" => Decision::Rollback,
        other => {
            return Err(invalid(format!(
                "args.decision must be `complete` or `rollback`; received {other:?}"
            )));
        }
    };
    validate_journal_directory(&journal)?;
    let journal_file = journal.join("journal.json");
    match fs::symlink_metadata(&journal_file) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(failed(
                "journal_corrupt",
                format!("transaction journal is missing: {}", journal_file.display()),
            ));
        }
        Err(error) => {
            return Err(super::io_error(
                error,
                format!("checking transaction journal {}", journal_file.display()),
            ));
        }
    }
    // Validate the journal before creating a lock file in the supplied path.
    let _ = read_journal(&journal, context)?;
    // Only a directory that holds a valid journal gets its Windows ACL made private
    // again (Unix checks owner and mode above), so recover never rewrites the ACL of
    // an arbitrary directory it was pointed at.
    #[cfg(windows)]
    crate::platform::windows::restrict_directory(&journal).map_err(|error| {
        failed(
            "io_error",
            format!("protecting transaction journal directory: {error:#}"),
        )
    })?;
    let _journal_lock =
        filesystem::acquire_path_lock(&journal.join("transaction.lock"), timeout, context)?;
    let mut data = read_journal(&journal, context)?;
    let loaded = load_entries(&journal, &data, context)?;
    let operations: Vec<_> = data
        .entries
        .iter()
        .zip(&loaded)
        .map(|(entry, loaded)| PlannedEntry {
            path: loaded.path.clone(),
            requested_path: loaded.path.to_string_lossy().into_owned(),
            expected_hash: entry.before_hash.clone(),
            after: loaded.after.clone(),
            after_exists: loaded.after_exists,
        })
        .collect();
    let _target_locks = lock_targets(
        operations.iter().map(|operation| operation.path.as_path()),
        timeout,
        context,
    )?;
    drive_journal(&journal, &mut data, &loaded, decision, context)?;
    let status = match data.state {
        TransactionState::Committed => "committed",
        TransactionState::RolledBack => "rolled_back",
        _ => {
            return Err(failed(
                "manual_recovery",
                format!(
                    "transaction remains partial; inspect journal {}",
                    journal.display()
                ),
            ));
        }
    };
    Ok(receipt(&journal, &data, status))
}

#[derive(Clone, Copy)]
enum Decision {
    Complete,
    Rollback,
}

fn drive_journal(
    directory: &Path,
    data: &mut JournalData,
    loaded: &[LoadedEntry],
    decision: Decision,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    context.checkpoint()?;
    let mut order: Vec<usize> = (0..data.entries.len()).collect();
    if matches!(decision, Decision::Rollback) {
        order.reverse();
        data.state = TransactionState::RollingBack;
    } else {
        data.state = TransactionState::Applying;
    }
    data.error = None;
    persist_journal(directory, data, context)?;

    for index in order {
        context.checkpoint()?;
        let entry = data.entries[index].clone();
        let path = entry.path.to_path()?;
        let current = match read_current_target(&path, context) {
            Ok(current) => current,
            Err(error) => {
                data.state = TransactionState::Partial;
                data.error = Some(error.to_string());
                persist_journal(directory, data, context)?;
                return Err(failed(
                    "manual_recovery",
                    format!(
                        "cannot classify transaction target {}: {error}",
                        path.display()
                    ),
                ));
            }
        };
        let before_matches = match current.as_deref() {
            Some(bytes) => digest_with_context(bytes, context)? == entry.before_hash,
            None => false,
        };
        let after_matches = match (current.as_deref(), entry.after_exists) {
            (None, false) => true,
            (Some(bytes), true) => digest_with_context(bytes, context)? == entry.after_hash,
            _ => false,
        };
        let state = if before_matches {
            CurrentState::Before
        } else if after_matches {
            CurrentState::After
        } else {
            CurrentState::Conflict
        };
        if state == CurrentState::Conflict {
            data.state = TransactionState::Partial;
            data.error = Some(format!("third-state content at {}", path.display()));
            persist_journal(directory, data, context)?;
            return Err(failed(
                "manual_recovery",
                format!(
                    "target {} matches neither recorded before nor after content; journal {} remains for manual recovery",
                    path.display(),
                    directory.display()
                ),
            ));
        }

        let needs_change = match decision {
            Decision::Complete => state == CurrentState::Before,
            Decision::Rollback => state == CurrentState::After,
        };
        if !needs_change {
            data.entries[index].state = match decision {
                Decision::Complete => EntryState::Applied,
                Decision::Rollback => EntryState::Compensated,
            };
            persist_journal(directory, data, context)?;
            continue;
        }

        data.entries[index].state = match decision {
            Decision::Complete => EntryState::Applying,
            Decision::Rollback => EntryState::Compensating,
        };
        persist_journal(directory, data, context)?;
        let (expected, replacement, replacement_exists): (&str, &[u8], bool) = match decision {
            Decision::Complete => (
                entry.before_hash.as_str(),
                loaded[index].after.as_slice(),
                entry.after_exists,
            ),
            Decision::Rollback => (
                if entry.after_exists {
                    entry.after_hash.as_str()
                } else {
                    "missing"
                },
                loaded[index].before.as_slice(),
                true,
            ),
        };
        // Both the committed write and compensation preserve the captured
        // Unix mode. For a removed file the target is absent, so `preserve`
        // would otherwise recreate it with the helper's private default.
        let (permission_policy, mode) = entry
            .before_mode
            .map(|mode| ("exact", Some(mode)))
            .unwrap_or(("preserve", None));
        let update = if replacement_exists {
            filesystem::atomic_replace_locked(
                &path,
                replacement,
                expected,
                "reject",
                permission_policy,
                mode,
                context,
            )
            .map(|_| ())
        } else {
            remove_locked(&path, expected, context)
        };
        if let Err(error) = update {
            data.state = TransactionState::Partial;
            data.error = Some(error.to_string());
            persist_journal(directory, data, context)?;
            return Err(failed(
                "uncertain_effect",
                format!(
                    "could not confirm transaction update for {}; journal {} is recoverable: {error}",
                    path.display(),
                    directory.display()
                ),
            ));
        }
        let observed = read_current_target(&path, context)?;
        let expected_after = match decision {
            Decision::Complete if entry.after_exists => Some(entry.after_hash.as_str()),
            Decision::Complete => None,
            Decision::Rollback => Some(entry.before_hash.as_str()),
        };
        let matches_expected = match (observed.as_deref(), expected_after) {
            (None, None) => true,
            (Some(bytes), Some(expected)) => digest_with_context(bytes, context)? == expected,
            _ => false,
        };
        if !matches_expected {
            data.state = TransactionState::Partial;
            data.error = Some(format!("post-write hash mismatch at {}", path.display()));
            persist_journal(directory, data, context)?;
            return Err(failed(
                "uncertain_effect",
                format!(
                    "post-write verification differed for {}; journal {} is recoverable",
                    path.display(),
                    directory.display()
                ),
            ));
        }
        data.entries[index].state = match decision {
            Decision::Complete => EntryState::Applied,
            Decision::Rollback => EntryState::Compensated,
        };
        persist_journal(directory, data, context)?;
    }

    data.state = match decision {
        Decision::Complete => TransactionState::Committed,
        Decision::Rollback => TransactionState::RolledBack,
    };
    data.error = None;
    persist_journal(directory, data, context)
}

fn parse_operations(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Vec<PlannedEntry>, AppletError> {
    // Each target validation below issues its own filesystem syscalls, and a
    // single syscall can still block inside the operating system; only the
    // slices between calls are interruptible.
    context.checkpoint()?;
    let operations = args
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("args.operations must be an array"))?;
    if operations.len() > MAX_FILES {
        return Err(invalid(format!(
            "transaction is limited to {MAX_FILES} files"
        )));
    }
    let mut parsed = Vec::with_capacity(operations.len());
    let mut seen = std::collections::BTreeSet::new();
    for (index, value) in operations.iter().enumerate() {
        context.checkpoint()?;
        let object = value
            .as_object()
            .ok_or_else(|| invalid(format!("args.operations[{index}] must be an object")))?;
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "path" | "expected_content_hash" | "action" | "content"
            ) {
                return Err(invalid(format!(
                    "args.operations[{index}] contains unsupported field {key:?}"
                )));
            }
        }
        let path_text = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("args.operations[{index}].path must be a string")))?;
        let requested = resolve_path(path_text, cwd);
        match fs::symlink_metadata(&requested) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "transaction target cannot be a symlink: {path_text:?}"
                )));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(invalid(format!(
                    "transaction target must be an existing regular file: {path_text:?}"
                )));
            }
            Ok(_) => {}
            Err(error) => {
                return Err(super::io_error(
                    error,
                    format!("checking transaction target {path_text:?}"),
                ));
            }
        }
        context.checkpoint()?;
        let path = fs::canonicalize(&requested)
            .map_err(|error| super::io_error(error, format!("resolving {path_text:?}")))?;
        context.checkpoint()?;
        if !seen.insert(path.clone()) {
            return Err(invalid(format!(
                "transaction contains duplicate target path {path_text:?}"
            )));
        }
        let expected_hash = object
            .get("expected_content_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid(format!(
                    "args.operations[{index}].expected_content_hash must be a string"
                ))
            })?
            .to_owned();
        if expected_hash.len() != 64
            || !expected_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(invalid(format!(
                "args.operations[{index}].expected_content_hash must be a lowercase SHA-256 hex digest"
            )));
        }
        let action = object
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("args.operations[{index}].action must be a string")))?;
        let (content, after_exists) = match action {
            "write" => (
                object
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        invalid(format!(
                            "args.operations[{index}].content must be a string for write"
                        ))
                    })?
                    .as_bytes()
                    .to_vec(),
                true,
            ),
            "remove" if !object.contains_key("content") => (Vec::new(), false),
            "remove" => {
                return Err(invalid(format!(
                    "args.operations[{index}].content must be omitted for remove"
                )));
            }
            other => {
                return Err(invalid(format!(
                    "args.operations[{index}].action must be `write` or `remove`; received {other:?}"
                )));
            }
        };
        if content.len() > MAX_FILE_BYTES {
            return Err(invalid(format!(
                "args.operations[{index}].content exceeds {MAX_FILE_BYTES} bytes"
            )));
        }
        parsed.push(PlannedEntry {
            path,
            requested_path: path_text.to_owned(),
            expected_hash,
            after: content,
            after_exists,
        });
    }
    parsed.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(parsed)
}

fn lock_targets<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    timeout: Duration,
    context: &ExecutionContext<'_>,
) -> Result<Vec<PathLock>, AppletError> {
    context.checkpoint()?;
    let mut locks: Vec<_> = paths
        .into_iter()
        .map(filesystem::default_lock_path)
        .collect::<Result<_, _>>()?;
    locks.sort();
    locks.dedup();
    let started = Instant::now();
    locks
        .into_iter()
        .map(|path| {
            context.checkpoint()?;
            let remaining = timeout.saturating_sub(started.elapsed());
            filesystem::acquire_path_lock(&path, remaining, context)
        })
        .collect()
}

#[cfg(windows)]
fn verify_loaded_targets_unchanged(
    journal: &Path,
    loaded: &[LoadedEntry],
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    for entry in loaded {
        context.checkpoint()?;
        let (current, mode) =
            read_existing_regular_with_mode(&entry.path, context).map_err(|error| {
            if matches!(error.code, "not_found" | "invalid_request") {
                failed(
                    "conflict",
                    format!(
                        "transaction target {} changed while journal {} was being prepared; the prepared journal remains for explicit recovery",
                        entry.path.display(),
                        journal.display(),
                    ),
                )
            } else {
                AppletError {
                    code: error.code,
                    kind: error.kind,
                    message: format!(
                        "checking target {} before applying transaction journal {}; the published prepared journal remains for explicit recovery: {}",
                        entry.path.display(),
                        journal.display(),
                        error.message
                    ),
                }
            }
        })?;
        if current.as_slice() != entry.before.as_slice() || mode != entry.before_mode {
            return Err(failed(
                "conflict",
                format!(
                    "transaction target {} changed while journal {} was being prepared; the prepared journal remains for explicit recovery",
                    entry.path.display(),
                    journal.display(),
                ),
            ));
        }
    }
    Ok(())
}

fn reject_path_collisions(journal: &Path, operations: &[PlannedEntry]) -> Result<(), AppletError> {
    for operation in operations {
        if operation.path == *journal {
            return Err(invalid("transaction journal cannot be a target file"));
        }
        let lock = filesystem::default_lock_path(&operation.path)?;
        if lock == *journal || lock.starts_with(journal) {
            return Err(invalid("transaction journal overlaps a target lock file"));
        }
    }
    Ok(())
}

fn journal_path(path: &str, cwd: &Path) -> Result<PathBuf, AppletError> {
    let requested = resolve_path(path, cwd);
    let leaf = requested
        .file_name()
        .ok_or_else(|| invalid("journal path must name a directory"))?;
    let parent = requested
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)
        .map_err(|error| super::io_error(error, "resolving transaction journal parent"))?;
    let metadata = fs::metadata(&parent)
        .map_err(|error| super::io_error(error, "checking transaction journal parent"))?;
    if !metadata.is_dir() {
        return Err(invalid("transaction journal parent must be a directory"));
    }
    Ok(parent.join(leaf))
}

fn resolve_path(path: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn create_journal_stage(final_path: &Path) -> Result<(PathBuf, PathBuf), AppletError> {
    if fs::symlink_metadata(final_path).is_ok() {
        return Err(failed(
            "journal_exists",
            format!(
                "transaction journal already exists: {}",
                final_path.display()
            ),
        ));
    }
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    let name = final_path
        .file_name()
        .ok_or_else(|| invalid("journal path must name a directory"))?;
    for _ in 0..100 {
        let sequence = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let mut stage_name = std::ffi::OsString::from(".");
        stage_name.push(name);
        stage_name.push(format!(".prepare.{}.{}", std::process::id(), sequence));
        let stage = parent.join(stage_name);
        #[allow(unused_mut)]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&stage) {
            Ok(()) => {
                restrict_directory(&stage)?;
                return Ok((stage, final_path.to_path_buf()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(super::io_error(
                    error,
                    format!("creating transaction journal stage {}", stage.display()),
                ));
            }
        }
    }
    Err(failed(
        "temporary_path_exhausted",
        "could not allocate a transaction journal staging directory",
    ))
}

#[cfg(windows)]
fn lock_published_prepared_journal(
    journal: &Path,
    prepared: &JournalData,
    timeout: Duration,
    context: &ExecutionContext<'_>,
) -> Result<PathLock, AppletError> {
    context.checkpoint()?;
    let lock = filesystem::acquire_path_lock(&journal.join("transaction.lock"), timeout, context)
        .map_err(
        |error| AppletError {
            code: error.code,
            kind: error.kind,
            message: format!(
                "acquiring lock for published transaction journal {}; inspect it after any active recovery completes: {}",
                journal.display(),
                error.message
            ),
        },
    )?;
    let current = read_journal(journal, context)?;
    if current.state != TransactionState::Prepared
        || serialized_digest(&current, context)? != serialized_digest(prepared, context)?
    {
        return Err(failed(
            "conflict",
            format!(
                "transaction journal {} changed before its initial apply acquired the lock; leaving it untouched for explicit inspection or recovery",
                journal.display()
            ),
        ));
    }
    Ok(lock)
}

struct DirectoryCleanup(Option<PathBuf>);
impl Drop for DirectoryCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn write_blobs_and_entries(
    directory: &Path,
    loaded: &[LoadedEntry],
    context: &ExecutionContext<'_>,
) -> Result<Vec<JournalEntry>, AppletError> {
    let mut entries = Vec::with_capacity(loaded.len());
    for (index, item) in loaded.iter().enumerate() {
        context.checkpoint()?;
        let before_name = format!("{index:02}.before");
        let after_name = format!("{index:02}.after");
        write_private_new(&directory.join(&before_name), &item.before, context)?;
        write_private_new(&directory.join(&after_name), &item.after, context)?;
        let before_hash = digest_with_context(&item.before, context)?;
        let after_hash = digest_with_context(&item.after, context)?;
        entries.push(JournalEntry {
            path: EncodedPath::from_path(&item.path),
            before_hash: before_hash.clone(),
            after_hash: after_hash.clone(),
            before_mode: item.before_mode,
            after_exists: item.after_exists,
            before_blob: BlobRef {
                name: before_name,
                sha256: before_hash,
                length: item.before.len() as u64,
            },
            after_blob: BlobRef {
                name: after_name,
                sha256: after_hash,
                length: item.after.len() as u64,
            },
            state: EntryState::Pending,
        });
    }
    Ok(entries)
}

fn load_entries(
    directory: &Path,
    data: &JournalData,
    context: &ExecutionContext<'_>,
) -> Result<Vec<LoadedEntry>, AppletError> {
    if data.entries.is_empty() || data.entries.len() > MAX_FILES {
        return Err(failed(
            "journal_corrupt",
            "transaction journal has an invalid file count",
        ));
    }
    let mut total = 0usize;
    let mut entries = Vec::with_capacity(data.entries.len());
    let mut paths = std::collections::BTreeSet::new();
    for entry in &data.entries {
        context.checkpoint()?;
        let path = entry.path.to_path()?;
        if !path.is_absolute() || !paths.insert(path.clone()) {
            return Err(failed(
                "journal_corrupt",
                "transaction journal has a relative or duplicate target path",
            ));
        }
        if entry.before_mode.is_some_and(|mode| mode > 0o7777) {
            return Err(failed(
                "journal_corrupt",
                "transaction journal contains an invalid before permission mode",
            ));
        }
        #[cfg(not(unix))]
        if entry.before_mode.is_some() {
            return Err(failed(
                "journal_corrupt",
                "transaction journal contains a Unix mode on this platform",
            ));
        }
        let before = read_blob(directory, &entry.before_blob, context)?;
        let after = read_blob(directory, &entry.after_blob, context)?;
        if digest_with_context(&before, context)? != entry.before_hash
            || digest_with_context(&after, context)? != entry.after_hash
        {
            return Err(failed(
                "journal_corrupt",
                "transaction journal blob hash does not match its recorded target hash",
            ));
        }
        total = total
            .checked_add(before.len())
            .and_then(|value| value.checked_add(after.len()))
            .ok_or_else(|| failed("journal_corrupt", "transaction journal size overflow"))?;
        if total > MAX_TOTAL_BYTES {
            return Err(failed(
                "journal_corrupt",
                "transaction journal content exceeds its size limit",
            ));
        }
        if !entry.after_exists && !after.is_empty() {
            return Err(failed(
                "journal_corrupt",
                "removed transaction entries must have an empty after blob",
            ));
        }
        entries.push(LoadedEntry {
            path,
            before,
            before_mode: entry.before_mode,
            after,
            after_exists: entry.after_exists,
        });
    }
    Ok(entries)
}

fn read_blob(
    directory: &Path,
    blob: &BlobRef,
    context: &ExecutionContext<'_>,
) -> Result<Vec<u8>, AppletError> {
    context.checkpoint()?;
    let valid_name = matches!(blob.name.len(), 8 | 9)
        && blob.name.as_bytes()[0..2].iter().all(u8::is_ascii_digit)
        && matches!(&blob.name[2..], ".before" | ".after");
    if !valid_name {
        return Err(failed(
            "journal_corrupt",
            "transaction journal has an invalid blob name",
        ));
    }
    if blob.length > MAX_FILE_BYTES as u64 {
        return Err(failed(
            "journal_corrupt",
            "transaction blob exceeds its size limit",
        ));
    }
    let path = directory.join(&blob.name);
    let mut file = open_private_regular(&path, "transaction blob")?;
    let mut bytes = Vec::with_capacity(blob.length.min(MAX_FILE_BYTES as u64) as usize);
    read_chunked(&mut file, &mut bytes, &path, context)?;
    if bytes.len() != blob.length as usize || digest_with_context(&bytes, context)? != blob.sha256 {
        return Err(failed(
            "journal_corrupt",
            format!("transaction blob {} is truncated or corrupt", blob.name),
        ));
    }
    Ok(bytes)
}

fn read_journal(
    directory: &Path,
    context: &ExecutionContext<'_>,
) -> Result<JournalData, AppletError> {
    context.checkpoint()?;
    validate_journal_directory(directory)?;
    let path = directory.join("journal.json");
    let mut file = open_private_regular(&path, "transaction journal")?;
    let mut bytes = Vec::new();
    read_chunked_bounded(&mut file, &mut bytes, &path, MAX_JOURNAL_BYTES, context)?;
    if bytes.len() > MAX_JOURNAL_BYTES {
        return Err(failed(
            "journal_corrupt",
            "transaction journal is too large",
        ));
    }
    let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|error| {
        failed(
            "journal_corrupt",
            format!("malformed transaction journal: {error}"),
        )
    })?;
    if serialized_digest(&envelope.data, context)? != envelope.checksum {
        return Err(failed(
            "journal_corrupt",
            "transaction journal checksum mismatch",
        ));
    }
    if envelope.data.version != VERSION {
        return Err(failed(
            "journal_corrupt",
            format!(
                "unsupported transaction journal version {}",
                envelope.data.version
            ),
        ));
    }
    Ok(envelope.data)
}

fn persist_journal(
    directory: &Path,
    data: &JournalData,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    context.checkpoint()?;
    let envelope = EnvelopeRef {
        checksum: serialized_digest(data, context)?,
        data,
    };
    let bytes = serde_json::to_vec(&envelope).map_err(|error| {
        failed(
            "journal_error",
            format!("encoding transaction journal: {error}"),
        )
    })?;
    if bytes.len() > MAX_JOURNAL_BYTES {
        return Err(failed("journal_error", "transaction journal exceeds 1 MiB"));
    }
    let final_path = directory.join("journal.json");
    let temporary = directory.join(format!(
        ".journal.{}.{}.tmp",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    // A cancelled or expired budget while writing the temporary leaves a
    // `.journal.*.tmp` file behind unless it is removed here; the journal
    // itself keeps its last persisted state for explicit recovery.
    write_private_new(&temporary, &bytes, context).inspect_err(|_error| {
        let _ = fs::remove_file(&temporary);
    })?;
    // A single replace or sync call can still block inside the operating
    // system; only the slices between calls are interruptible. Failure codes
    // are unchanged.
    context.checkpoint()?;
    if let Err(error) = crate::platform::replace_file(&temporary, &final_path) {
        let _ = fs::remove_file(&temporary);
        return Err(failed(
            "uncertain_effect",
            format!(
                "could not publish transaction journal {}: {error}",
                final_path.display()
            ),
        ));
    }
    context.checkpoint()?;
    sync_directory(directory).map_err(|error| {
        failed(
            "uncertain_effect",
            format!("journal update was published but directory sync failed: {error}"),
        )
    })?;
    context.checkpoint()?;
    Ok(())
}

fn serialized_digest(
    data: &JournalData,
    context: &ExecutionContext<'_>,
) -> Result<String, AppletError> {
    let bytes = serde_json::to_vec(data).map_err(|error| {
        failed(
            "journal_error",
            format!("encoding journal checksum: {error}"),
        )
    })?;
    digest_with_context(&bytes, context)
}

fn write_private_new(
    path: &Path,
    bytes: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    context.checkpoint()?;
    let mut options = crate::platform::private_options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::GENERIC_WRITE,
            Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, READ_CONTROL, WRITE_DAC},
        };
        options
            .access_mode(GENERIC_WRITE | WRITE_DAC | READ_CONTROL)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path).map_err(|error| {
        super::io_error(error, format!("creating private state {}", path.display()))
    })?;
    restrict_file(&file)?;
    // A cancelled budget mid-write leaves a partial file behind unless it is
    // removed here. A sync failure keeps the fully written file in place so
    // the journal directory shows what was persisted.
    if let Err(error) = write_chunked(&mut file, bytes, path, context) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    context.checkpoint()?;
    file.sync_all().map_err(|error| {
        super::io_error(error, format!("syncing private state {}", path.display()))
    })?;
    context.checkpoint()?;
    Ok(())
}

fn open_private_regular(path: &Path, label: &str) -> Result<File, AppletError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| super::io_error(error, format!("checking {label} {}", path.display())))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(failed(
            "journal_corrupt",
            format!("{label} must be a regular file: {}", path.display()),
        ));
    }
    check_private_metadata(&metadata, label)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| super::io_error(error, format!("opening {label} {}", path.display())))?;
    if !file
        .metadata()
        .map_err(|error| super::io_error(error, format!("checking {label}")))?
        .is_file()
    {
        return Err(failed(
            "journal_corrupt",
            format!("{label} is not a regular file"),
        ));
    }
    check_private_metadata(
        &file
            .metadata()
            .map_err(|error| super::io_error(error, format!("checking {label}")))?,
        label,
    )?;
    Ok(file)
}

fn read_existing_regular(
    path: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Vec<u8>, AppletError> {
    read_existing_regular_with_mode(path, context).map(|(content, _)| content)
}

fn read_existing_regular_with_mode(
    path: &Path,
    context: &ExecutionContext<'_>,
) -> Result<(Vec<u8>, Option<u32>), AppletError> {
    context.checkpoint()?;
    let link_metadata = fs::symlink_metadata(path).map_err(|error| {
        super::io_error(
            error,
            format!("checking transaction target {}", path.display()),
        )
    })?;
    if !link_metadata.is_file() || link_metadata.file_type().is_symlink() {
        return Err(invalid(format!(
            "transaction target must remain an existing regular file: {}",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path).map_err(|error| {
        super::io_error(
            error,
            format!("opening transaction target {}", path.display()),
        )
    })?;
    let before = file
        .metadata()
        .map_err(|error| super::io_error(error, "checking transaction target"))?;
    if !before.is_file() {
        return Err(invalid(format!(
            "transaction target is not regular: {}",
            path.display()
        )));
    }
    let mut content = Vec::new();
    read_chunked_bounded(&mut file, &mut content, path, MAX_FILE_BYTES, context)?;
    if content.len() > MAX_FILE_BYTES {
        return Err(invalid(format!(
            "transaction target exceeds {MAX_FILE_BYTES} bytes: {}",
            path.display()
        )));
    }
    let after = file
        .metadata()
        .map_err(|error| super::io_error(error, "rechecking transaction target"))?;
    let link_after = fs::symlink_metadata(path)
        .map_err(|error| super::io_error(error, "rechecking transaction target path"))?;
    if !same_target_metadata(&before, &after)
        || !same_target_metadata(&link_metadata, &link_after)
        || link_after.file_type().is_symlink()
    {
        return Err(failed(
            "conflict",
            format!(
                "transaction target changed while it was read: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    let mode = Some(before.mode() & 0o7777);
    #[cfg(not(unix))]
    let mode = None;
    Ok((content, mode))
}

fn read_current_target(
    path: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Option<Vec<u8>>, AppletError> {
    context.checkpoint()?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(failed(
            "manual_recovery",
            format!(
                "transaction target is no longer a regular file: {}",
                path.display()
            ),
        )),
        Ok(_) => read_existing_regular(path, context).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(super::io_error(
            error,
            format!("checking transaction target {}", path.display()),
        )),
    }
}

/// Remove an existing target after a fresh content-hash check while its
/// stable advisory path lock is held by the caller. Locks coordinate Flow's
/// applets; arbitrary writers that ignore the lock remain outside that CAS
/// boundary.
fn remove_locked(
    path: &Path,
    expected_hash: &str,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    context.checkpoint()?;
    let before = read_existing_regular(path, context)?;
    let observed_hash = digest_with_context(&before, context)?;
    if observed_hash != expected_hash {
        return Err(failed(
            "conflict",
            format!(
                "content changed before transaction removal at {}: expected {}, found {}",
                path.display(),
                expected_hash,
                observed_hash
            ),
        ));
    }
    // A single remove or sync call can still block inside the operating
    // system; only the slices between calls are interruptible. Failure codes
    // are unchanged.
    context.checkpoint()?;
    fs::remove_file(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            failed(
                "conflict",
                format!(
                    "target disappeared before transaction removal: {}",
                    path.display()
                ),
            )
        } else {
            super::io_error(
                error,
                format!("removing transaction target {}", path.display()),
            )
        }
    })?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    context.checkpoint()?;
    sync_directory(parent).map_err(|error| {
        failed(
            "uncertain_effect",
            format!("target was removed but its parent could not be synced: {error}"),
        )
    })?;
    context.checkpoint()?;
    if read_current_target(path, context)?.is_some() {
        return Err(failed(
            "uncertain_effect",
            format!(
                "target still exists after transaction removal: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn same_target_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.mode() == right.mode()
            && left.len() == right.len()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        left.is_file() == right.is_file()
            && left.len() == right.len()
            && left.modified().ok() == right.modified().ok()
    }
}

fn validate_journal_directory(path: &Path) -> Result<(), AppletError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        super::io_error(
            error,
            format!("checking transaction journal {}", path.display()),
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(failed(
            "journal_corrupt",
            format!(
                "transaction journal must be a real directory: {}",
                path.display()
            ),
        ));
    }
    check_private_metadata(&metadata, "transaction journal directory")?;
    Ok(())
}

fn restrict_directory(path: &Path) -> Result<(), AppletError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| super::io_error(error, "protecting transaction journal directory"))?;
    #[cfg(windows)]
    crate::platform::windows::restrict_directory(path).map_err(|error| {
        failed(
            "io_error",
            format!("protecting transaction journal directory: {error:#}"),
        )
    })?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| super::io_error(error, "checking transaction journal directory"))?;
    check_private_metadata(&metadata, "transaction journal directory")
}

fn restrict_file(file: &File) -> Result<(), AppletError> {
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| super::io_error(error, "protecting transaction journal file"))?;
    #[cfg(windows)]
    crate::platform::windows::restrict_file(file).map_err(|error| {
        failed(
            "io_error",
            format!("protecting transaction journal file: {error:#}"),
        )
    })?;
    check_private_metadata(
        &file
            .metadata()
            .map_err(|error| super::io_error(error, "checking transaction journal file"))?,
        "transaction journal file",
    )
}

fn check_private_metadata(metadata: &fs::Metadata, label: &str) -> Result<(), AppletError> {
    #[cfg(unix)]
    // SAFETY: geteuid(2) takes no arguments, cannot fail (POSIX), and has no memory preconditions.
    // Its result is compared with the owner uid in `metadata`.
    if metadata.uid() != crate::platform::unix_ffi::effective_uid() || metadata.mode() & 0o077 != 0
    {
        return Err(failed(
            "journal_untrusted",
            format!("{label} must be owned by the current user and private"),
        ));
    }
    #[cfg(windows)]
    if crate::platform::windows::is_reparse(metadata) {
        return Err(failed(
            "journal_untrusted",
            format!("{label} cannot be a reparse point"),
        ));
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (metadata, label);
    Ok(())
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn lock_timeout(args: &Map<String, Value>) -> Result<Duration, AppletError> {
    let millis = match args.get("lock_timeout_ms") {
        None => DEFAULT_LOCK_TIMEOUT_MS,
        Some(value) => value
            .as_u64()
            .filter(|value| *value <= MAX_LOCK_TIMEOUT_MS)
            .ok_or_else(|| {
                invalid(format!(
                    "args.lock_timeout_ms must be an integer from 0 through {MAX_LOCK_TIMEOUT_MS}"
                ))
            })?,
    };
    Ok(Duration::from_millis(millis))
}

/// Chunk between budget checks; each slice is plain CPU work with no helper thread.
const IO_CHUNK_BYTES: usize = 64 * 1024;

fn digest_with_context(
    bytes: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<String, AppletError> {
    let mut hasher = Sha256::new();
    for chunk in bytes.chunks(IO_CHUNK_BYTES) {
        context.checkpoint()?;
        hasher.update(chunk);
    }
    context.checkpoint()?;
    Ok(to_hex_digest(hasher))
}

fn to_hex_digest(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn read_chunked(
    file: &mut File,
    content: &mut Vec<u8>,
    path: &Path,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    read_chunked_bounded(file, content, path, MAX_FILE_BYTES, context)
}

fn read_chunked_bounded(
    file: &mut File,
    content: &mut Vec<u8>,
    path: &Path,
    limit: usize,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    let cap = limit.saturating_add(1);
    let mut chunk = vec![0u8; IO_CHUNK_BYTES];
    let mut work = 0usize;
    loop {
        context.checkpoint()?;
        let end = content.len().saturating_add(IO_CHUNK_BYTES).min(cap);
        let want = end.saturating_sub(content.len());
        if want == 0 {
            break;
        }
        let read = file
            .read(&mut chunk[..want])
            .map_err(|error| super::io_error(error, format!("reading {}", path.display())))?;
        if read == 0 {
            break;
        }
        content.extend_from_slice(&chunk[..read]);
        work += 1;
        checkpoint_work(context, work)?;
        if content.len() >= cap {
            break;
        }
    }
    Ok(())
}

fn write_chunked(
    file: &mut File,
    bytes: &[u8],
    path: &Path,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    let mut written = 0usize;
    let mut work = 0usize;
    while written < bytes.len() {
        context.checkpoint()?;
        let end = written.saturating_add(IO_CHUNK_BYTES).min(bytes.len());
        file.write_all(&bytes[written..end]).map_err(|error| {
            super::io_error(error, format!("writing private state {}", path.display()))
        })?;
        written = end;
        work += 1;
        checkpoint_work(context, work)?;
    }
    Ok(())
}

#[cfg(test)]
fn digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn receipt(directory: &Path, data: &JournalData, status: &str) -> Value {
    let entries: Vec<Value> = data
        .entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "path": entry.path.to_path().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default(),
                "before_hash": entry.before_hash,
                "after_hash": entry.after_hash,
                "before_mode": entry.before_mode,
                "after_exists": entry.after_exists,
                "state": entry.state,
            })
        })
        .collect();
    serde_json::json!({
        "status": status,
        "journal": directory.to_string_lossy(),
        "entries": entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(journal: &Path, files: &[(&Path, &str, &str)]) -> Map<String, Value> {
        serde_json::from_value(serde_json::json!({
            "journal": journal,
            "operations": files.iter().map(|(path, old, new)| serde_json::json!({
                "path": path,
                "expected_content_hash": digest(old.as_bytes()),
                "action": "write",
                "content": new,
            })).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn newly_created_private_journal_files_can_be_published_and_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let published = temp.path().join("journal.json");
        let first = temp.path().join(".journal.first.tmp");
        let second = temp.path().join(".journal.second.tmp");

        write_private_new(
            &first,
            b"first private journal",
            &ExecutionContext::unrestricted(),
        )
        .unwrap();
        crate::platform::replace_file(&first, &published).unwrap();
        assert_eq!(fs::read(&published).unwrap(), b"first private journal");

        write_private_new(
            &second,
            b"replacement journal",
            &ExecutionContext::unrestricted(),
        )
        .unwrap();
        crate::platform::replace_file(&second, &published).unwrap();
        assert_eq!(fs::read(published).unwrap(), b"replacement journal");
    }

    #[cfg(windows)]
    #[test]
    fn publisher_refuses_a_journal_recovered_during_the_lock_gap() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config.json");
        fs::write(&target, b"before").unwrap();
        let loaded = vec![LoadedEntry {
            path: target.clone(),
            before: b"before".to_vec(),
            before_mode: None,
            after: b"after".to_vec(),
            after_exists: true,
        }];
        let journal = temp.path().join("journal");
        let (stage, published) = create_journal_stage(&journal).unwrap();
        let context = ExecutionContext::unrestricted();
        let entries = write_blobs_and_entries(&stage, &loaded, &context).unwrap();
        let prepared = JournalData {
            version: VERSION,
            state: TransactionState::Prepared,
            entries,
            error: None,
        };
        persist_journal(&stage, &prepared, &context).unwrap();
        fs::rename(&stage, &published).unwrap();

        let recovery_lock = filesystem::acquire_path_lock(
            &published.join("transaction.lock"),
            Duration::from_millis(DEFAULT_LOCK_TIMEOUT_MS),
            &context,
        )
        .unwrap();
        let recovery_entries = load_entries(&published, &prepared, &context).unwrap();
        let mut recovered = read_journal(&published, &context).unwrap();
        drive_journal(
            &published,
            &mut recovered,
            &recovery_entries,
            Decision::Complete,
            &context,
        )
        .unwrap();
        drop(recovery_lock);

        let error = match lock_published_prepared_journal(
            &published,
            &prepared,
            Duration::from_millis(DEFAULT_LOCK_TIMEOUT_MS),
            &context,
        ) {
            Ok(_) => panic!("publisher must not reapply a recovered journal"),
            Err(error) => error,
        };
        assert_eq!(error.code, "conflict");
        assert!(error.message.contains(published.to_string_lossy().as_ref()));
        assert_eq!(
            read_journal(&published, &context).unwrap().state,
            TransactionState::Committed
        );
        assert_eq!(fs::read(target).unwrap(), b"after");
    }

    #[cfg(windows)]
    #[test]
    fn target_preflight_conflict_names_preserved_prepared_journal() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config.json");
        fs::write(&target, b"before").unwrap();
        let loaded = vec![LoadedEntry {
            path: target.clone(),
            before: b"before".to_vec(),
            before_mode: None,
            after: b"after".to_vec(),
            after_exists: true,
        }];
        let journal = temp.path().join("journal");
        let (stage, published) = create_journal_stage(&journal).unwrap();
        let context = ExecutionContext::unrestricted();
        let entries = write_blobs_and_entries(&stage, &loaded, &context).unwrap();
        let prepared = JournalData {
            version: VERSION,
            state: TransactionState::Prepared,
            entries,
            error: None,
        };
        persist_journal(&stage, &prepared, &context).unwrap();
        fs::rename(&stage, &published).unwrap();

        let _journal_lock = filesystem::acquire_path_lock(
            &published.join("transaction.lock"),
            Duration::from_millis(DEFAULT_LOCK_TIMEOUT_MS),
            &context,
        )
        .unwrap();
        fs::write(&target, b"third-party update").unwrap();

        let error = verify_loaded_targets_unchanged(&published, &loaded, &context).unwrap_err();
        assert_eq!(error.code, "conflict");
        assert!(error.message.contains(published.to_string_lossy().as_ref()));
        assert!(error.message.contains("remains for explicit recovery"));
        assert_eq!(
            read_journal(&published, &context).unwrap().state,
            TransactionState::Prepared
        );
        assert_eq!(fs::read(target).unwrap(), b"third-party update");
    }

    #[cfg(windows)]
    #[test]
    fn apply_rejects_target_drift_after_publication_without_overwriting_it() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("journal");
        let target = temp.path().join("config.json");
        fs::write(&target, b"before").unwrap();
        let plan = args(&journal, &[(&target, "before", "after")]);
        let changed_target = target.clone();
        let context = ExecutionContext::unrestricted();
        let interpose_target_drift = move |published: &Path| {
            assert_eq!(
                read_journal(published, &context).unwrap().state,
                TransactionState::Prepared
            );
            fs::write(&changed_target, b"third-party update").unwrap();
        };

        let error = apply_with_fault(
            &plan,
            temp.path(),
            &context,
            None,
            Some(&interpose_target_drift),
        )
        .unwrap_err();

        let published_journal = fs::canonicalize(&journal).unwrap();
        assert_eq!(error.code, "conflict");
        assert!(error
            .message
            .contains(published_journal.to_string_lossy().as_ref()));
        assert!(error.message.contains("remains for explicit recovery"));
        assert_eq!(fs::read(&target).unwrap(), b"third-party update");
        assert_eq!(
            read_journal(&journal, &context).unwrap().state,
            TransactionState::Prepared
        );
    }

    fn remove_args(journal: &Path, path: &Path, old: &[u8]) -> Map<String, Value> {
        serde_json::from_value(serde_json::json!({
            "journal": journal,
            "operations": [{
                "path": path,
                "expected_content_hash": digest(old),
                "action": "remove"
            }]
        }))
        .unwrap()
    }

    #[test]
    fn committed_file_set_can_be_explicitly_compensated_and_completed() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.conf");
        let second = temp.path().join("second.conf");
        let journal = temp.path().join("journal");
        fs::write(&first, "before one").unwrap();
        fs::write(&second, "before two").unwrap();
        let plan = args(
            &journal,
            &[
                (first.as_path(), "before one", "after one"),
                (second.as_path(), "before two", "after two"),
            ],
        );

        let context = ExecutionContext::unrestricted();
        let applied = apply(&plan, temp.path(), &context).unwrap();
        assert_eq!(applied["status"], "committed");
        assert_eq!(fs::read_to_string(&first).unwrap(), "after one");
        assert_eq!(fs::read_to_string(&second).unwrap(), "after two");

        let rollback = serde_json::json!({"journal": journal, "decision": "rollback"});
        let rolled_back = recover(rollback.as_object().unwrap(), temp.path(), &context).unwrap();
        assert_eq!(rolled_back["status"], "rolled_back");
        assert_eq!(fs::read_to_string(&first).unwrap(), "before one");
        assert_eq!(fs::read_to_string(&second).unwrap(), "before two");

        let complete = serde_json::json!({"journal": journal, "decision": "complete"});
        let completed = recover(complete.as_object().unwrap(), temp.path(), &context).unwrap();
        assert_eq!(completed["status"], "committed");
        assert_eq!(fs::read_to_string(&first).unwrap(), "after one");
        assert_eq!(fs::read_to_string(&second).unwrap(), "after two");
    }

    #[test]
    fn crash_after_rename_before_progress_is_reconciled_by_hash() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.conf");
        let second = temp.path().join("second.conf");
        let journal = temp.path().join("journal");
        fs::write(&first, "before one").unwrap();
        fs::write(&second, "before two").unwrap();
        let plan = args(
            &journal,
            &[
                (first.as_path(), "before one", "after one"),
                (second.as_path(), "before two", "after two"),
            ],
        );

        let context = ExecutionContext::unrestricted();
        let crashed = apply_with_fault(&plan, temp.path(), &context, Some(1), None);
        assert!(crashed.is_err());
        assert_eq!(fs::read_to_string(&first).unwrap(), "after one");
        assert_eq!(fs::read_to_string(&second).unwrap(), "before two");

        let rollback = serde_json::json!({"journal": journal, "decision": "rollback"});
        let recovered = recover(rollback.as_object().unwrap(), temp.path(), &context).unwrap();
        assert_eq!(recovered["status"], "rolled_back");
        assert_eq!(fs::read_to_string(&first).unwrap(), "before one");
        assert_eq!(fs::read_to_string(&second).unwrap(), "before two");
    }

    #[test]
    fn recovery_refuses_a_third_party_state_and_preserves_the_journal() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.conf");
        let second = temp.path().join("second.conf");
        let journal = temp.path().join("journal");
        fs::write(&first, "before one").unwrap();
        fs::write(&second, "before two").unwrap();
        let plan = args(
            &journal,
            &[
                (first.as_path(), "before one", "after one"),
                (second.as_path(), "before two", "after two"),
            ],
        );
        let context = ExecutionContext::unrestricted();
        assert!(apply_with_fault(&plan, temp.path(), &context, Some(1), None).is_err());
        fs::write(&first, "manual edit").unwrap();

        let rollback = serde_json::json!({"journal": journal, "decision": "rollback"});
        let error = recover(rollback.as_object().unwrap(), temp.path(), &context).unwrap_err();
        assert_eq!(error.code, "manual_recovery");
        assert_eq!(fs::read_to_string(&first).unwrap(), "manual edit");
        assert!(journal.join("journal.json").is_file());
    }

    #[test]
    fn duplicate_targets_and_mismatched_hashes_are_rejected_before_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("one.conf");
        let journal = temp.path().join("journal");
        fs::write(&file, "before").unwrap();
        let duplicate = args(
            &journal,
            &[
                (file.as_path(), "before", "one"),
                (file.as_path(), "before", "two"),
            ],
        );
        let context = ExecutionContext::unrestricted();
        assert!(apply(&duplicate, temp.path(), &context).is_err());
        let mismatch = args(&journal, &[(file.as_path(), "wrong", "new")]);
        assert!(apply(&mismatch, temp.path(), &context).is_err());
        assert_eq!(fs::read_to_string(file).unwrap(), "before");
        assert!(!journal.exists());
    }

    #[test]
    fn removal_is_journaled_and_can_be_explicitly_compensated_or_completed() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join(".obsolete");
        let journal = temp.path().join("journal");
        fs::write(&target, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        }
        let plan = remove_args(&journal, &target, b"");

        let context = ExecutionContext::unrestricted();
        let applied = apply(&plan, temp.path(), &context).unwrap();
        assert_eq!(applied["status"], "committed");
        assert_eq!(applied["entries"][0]["after_exists"], false);
        assert!(!target.exists());

        let rollback = serde_json::json!({"journal": journal, "decision": "rollback"});
        let rolled_back = recover(rollback.as_object().unwrap(), temp.path(), &context).unwrap();
        assert_eq!(rolled_back["status"], "rolled_back");
        assert_eq!(fs::read(&target).unwrap(), b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
                0o640
            );
        }

        let complete = serde_json::json!({"journal": journal, "decision": "complete"});
        let completed = recover(complete.as_object().unwrap(), temp.path(), &context).unwrap();
        assert_eq!(completed["status"], "committed");
        assert!(!target.exists());
    }

    #[test]
    fn crash_after_unlink_is_classified_as_absent_and_explicitly_recovered() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("obsolete");
        let journal = temp.path().join("journal");
        fs::write(&target, b"old contents").unwrap();
        let plan = remove_args(&journal, &target, b"old contents");

        let context = ExecutionContext::unrestricted();
        let crashed = apply_with_fault(&plan, temp.path(), &context, Some(1), None);
        assert!(crashed.is_err());
        assert!(!target.exists());

        let rollback = serde_json::json!({"journal": journal, "decision": "rollback"});
        let recovered = recover(rollback.as_object().unwrap(), temp.path(), &context).unwrap();
        assert_eq!(recovered["status"], "rolled_back");
        assert_eq!(fs::read(&target).unwrap(), b"old contents");
    }
}
