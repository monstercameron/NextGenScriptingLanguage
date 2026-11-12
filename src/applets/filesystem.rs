//! Filesystem operations use a retained advisory lock file shared by
//! cooperating callers, hash-based expected-content checks and a metadata
//! recheck immediately before atomic replacement. This is not a portable
//! compare-and-swap against arbitrary writers and does not make multiple files
//! transactional. The parent directory is assumed to be trusted: portable
//! path-based rename still has a narrow race with non-cooperating writers.
//!
//! A preserved final symlink is resolved to its canonical target and opened
//! without following another final symlink; the link and target identity are
//! rechecked before replacement. The default lock is derived from the
//! canonical identity of the target, so a symlink alias, a `..` spelling and
//! (on Windows) a case-only spelling share one lock file beside the referent.
//! Limits: hard links are separate directory entries and still get separate
//! locks; a target whose parent directory does not exist yet keeps its
//! lexical lock path; non-cooperating writers are not stopped by any lock. Writes sync the temp file; Unix then syncs
//! the parent directory, while Windows asks the OS to persist the rename
//! entry during replacement and syncs the parent afterwards. A successful
//! flush means the OS was asked, not that the entry provably survives power
//! loss. Permission preservation covers Unix mode bits only; ownership,
//! ACLs, extended attributes and timestamps are not copied. File content is
//! capped at 16 MiB to keep reads and CAS checks bounded.

use super::{
    checkpoint_work, failed, invalid, io_error, optional_bool, required_string, AppletError,
    ExecutionContext,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
/// Advisory-lock polling slice. Each slice rechecks the execution budget, so
/// cancellation and deadlines preempt a held lock within milliseconds. No
/// helper thread is spawned: the waiter polls and returns, never abandoning
/// work to a detached thread.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// File copy chunk. Reads and temp-file writes move through this many bytes
/// between budget checks, so large files stay interruptible while each
/// individual `read`/`write` system call can still block on a stalled
/// filesystem (for example NFS).
const IO_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymlinkPolicy {
    Preserve,
    Reject,
    Replace,
}

#[derive(Debug, Clone, Copy)]
enum PermissionPolicy {
    Preserve,
    Private,
    Exact(u32),
}

struct FileState {
    content: Vec<u8>,
    content_hash: String,
    snapshot: String,
    symlink: Option<String>,
    resolved_path: PathBuf,
    mode: Option<u32>,
}

struct BrokenSymlink {
    metadata: fs::Metadata,
    target: PathBuf,
}

pub(super) fn read(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let path = resolve_path(required_string(args, "path")?, cwd);
    let state = inspect(&path, context)?.ok_or_else(|| {
        super::absence(
            "not_found",
            format!("file does not exist: {}", path.display()),
        )
    })?;
    let content = String::from_utf8(state.content)
        .map_err(|error| invalid(format!("file is not UTF-8 text: {error}")))?;
    Ok(serde_json::json!({
        "content": content,
        "content_hash": state.content_hash,
        "snapshot": state.snapshot,
        "symlink": state.symlink,
        "mode": state.mode,
    }))
}

pub(super) fn write_atomic(
    args: &Map<String, Value>,
    input: &[u8],
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let path = resolve_path(required_string(args, "path")?, cwd);
    let _ = result_path(&path)?;
    let content = match args.get("content") {
        Some(Value::String(value)) => value.as_bytes().to_vec(),
        Some(_) => return Err(invalid("args.content must be a string")),
        None => input.to_vec(),
    };
    if content.len() > MAX_FILE_BYTES {
        return Err(invalid(format!(
            "file content exceeds the {MAX_FILE_BYTES}-byte limit"
        )));
    }
    let expected_hash = required_string(args, "expected_content_hash")?;
    let symlink_policy = parse_symlink_policy(args)?;
    let permission_policy = parse_permission_policy(args)?;
    let lock_path = lock_path(args, &path, cwd)?;
    validate_lock_path(&lock_path, &path)?;
    let lock_timeout = lock_timeout(args)?;

    with_path_lock(&lock_path, lock_timeout, context, || {
        write_atomic_locked(
            &path,
            &content,
            expected_hash,
            symlink_policy,
            permission_policy,
            context,
        )
    })
}

/// Remove one final filesystem entry only when the snapshot still matches.
/// For a symlink this removes the link itself and never follows it for the
/// unlink operation. Snapshot reads still inspect the target so callers can
/// use `fs.snapshot` for either regular files or live symlinks.
pub(super) fn unlink_checked(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let path = resolve_path(required_string(args, "path")?, cwd);
    let _ = result_path(&path)?;
    let expected_snapshot = required_string(args, "expected_snapshot")?;
    let lock_path = lock_path(args, &path, cwd)?;
    validate_lock_path(&lock_path, &path)?;
    let lock_timeout = lock_timeout(args)?;

    with_path_lock(&lock_path, lock_timeout, context, || {
        let initial = inspect(&path, context)?.ok_or_else(|| {
            super::absence(
                "not_found",
                format!("file does not exist: {}", path.display()),
            )
        })?;
        if initial.snapshot != expected_snapshot {
            return Err(failed(
                "conflict",
                format!("file changed since it was snapshotted: {}", path.display()),
            ));
        }

        // Re-read immediately before unlinking to narrow the window for
        // non-cooperating writers. The stable advisory lock coordinates only
        // cooperating applets; the containing directory must be trusted.
        context.checkpoint()?;
        let current = inspect(&path, context)?.ok_or_else(|| {
            failed(
                "conflict",
                format!("file disappeared before unlink: {}", path.display()),
            )
        })?;
        if current.snapshot != expected_snapshot {
            return Err(failed(
                "conflict",
                format!("file changed before unlink: {}", path.display()),
            ));
        }
        let symlink = current.symlink.clone();
        // A single unlink or sync call can still block inside the operating
        // system; only the slices between calls are interruptible. Error
        // codes are unchanged.
        context.checkpoint()?;
        fs::remove_file(&path).map_err(|error| {
            failed(
                "uncertain_effect",
                format!("could not confirm unlink of {}: {error}", path.display()),
            )
        })?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        context.checkpoint()?;
        sync_directory(parent).map_err(|error| {
            failed(
                "uncertain_effect",
                format!("entry was unlinked but parent sync failed: {error}"),
            )
        })?;
        context.checkpoint()?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(failed(
                    "uncertain_effect",
                    format!("entry remains after unlink: {}", path.display()),
                ));
            }
            Err(error) => {
                return Err(failed(
                    "uncertain_effect",
                    format!("could not verify unlink of {}: {error}", path.display()),
                ));
            }
        }
        Ok(serde_json::json!({
            "path": result_path(&path)?,
            "removed": true,
            "symlink": symlink,
        }))
    })
}

/// `fs.stat`: one typed record for `path`. A trailing symbolic link is not
/// followed unless `follow` is true, so `type` then names the link itself. An
/// absent path, or one whose parent is not a directory, answers
/// `{"type": "absent"}` with null fields rather than an error, as `fs.test`
/// does. A dangling link is absent under `follow`. `mode` is Unix permission
/// bits and `read_only` is the Windows attribute; each is null elsewhere.
/// `mtime_ns` is nanoseconds since the Unix epoch, null when the platform
/// does not report it.
pub(super) fn stat(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let path = resolve_path(required_string(args, "path")?, cwd);
    let follow = optional_bool(args, "follow", false)?;
    context.checkpoint()?;
    let looked_up = if follow {
        fs::metadata(&path)
    } else {
        fs::symlink_metadata(&path)
    };
    let metadata = match looked_up {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(serde_json::json!({
                "type": "absent",
                "size": null,
                "mode": null,
                "read_only": null,
                "mtime_ns": null,
            }));
        }
        Err(error) => return Err(io_error(error, format!("checking {}", path.display()))),
    };
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_file() {
        "file"
    } else if file_type.is_dir() {
        "directory"
    } else {
        "other"
    };
    Ok(serde_json::json!({
        "type": kind,
        "size": metadata.len(),
        "mode": file_mode(&metadata),
        "read_only": read_only_flag(&metadata),
        "mtime_ns": modified_ns(&metadata),
    }))
}

/// Windows read-only attribute; `None` on other platforms, where `mode` is the
/// meaningful permission report.
#[cfg(windows)]
fn read_only_flag(metadata: &fs::Metadata) -> Option<bool> {
    Some(metadata.permissions().readonly())
}

#[cfg(not(windows))]
fn read_only_flag(metadata: &fs::Metadata) -> Option<bool> {
    let _ = metadata;
    None
}

/// Modification time as signed nanoseconds since the Unix epoch. Times before
/// the epoch are negative. `None` when the platform reports no time or the
/// value does not fit in 64 bits.
fn modified_ns(metadata: &fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).ok(),
        Err(before) => i64::try_from(before.duration().as_nanos())
            .ok()
            .map(|nanos| -nanos),
    }
}

/// `fs.realpath`: the canonical form of an existing `path`, with every symbolic
/// link resolved. An absent path is refused with the `not_found` absence error
/// rather than answered, because it has no canonical name.
pub(super) fn realpath(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let path = resolve_path(required_string(args, "path")?, cwd);
    context.checkpoint()?;
    let resolved = fs::canonicalize(&path)
        .map_err(|error| io_error(error, format!("resolving {}", path.display())))?;
    let resolved = strip_verbatim(resolved);
    Ok(serde_json::json!({
        "realpath": result_path(&resolved)?,
    }))
}

pub(crate) struct PathLock {
    // The OS lock remains held until this file descriptor is dropped.
    _file: File,
}

pub(crate) fn acquire_path_lock(
    lock_path: &Path,
    timeout: Duration,
    context: &ExecutionContext<'_>,
) -> Result<PathLock, AppletError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Nonblocking so a FIFO planted at the lock path cannot hang the
        // open; the regular-file check below then refuses it.
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options
        .open(lock_path)
        .map_err(|error| io_error(error, format!("opening lock {}", lock_path.display())))?;
    let metadata = file
        .metadata()
        .map_err(|error| io_error(error, format!("checking lock {}", lock_path.display())))?;
    if !metadata.is_file() {
        return Err(invalid(format!(
            "lock path is not a regular file: {}",
            lock_path.display()
        )));
    }
    let started = Instant::now();
    loop {
        // Check the budget before each attempt so a cancelled or expired
        // caller preempts the wait instead of sleeping through it. This
        // returns the caller's cancellation or deadline error, never a
        // retried or converted lock result.
        context.checkpoint()?;
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(io_error(error, format!("locking {}", lock_path.display())));
            }
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() >= timeout => {
                // Re-check the budget so a wait that outlived its deadline
                // reports the deadline instead of a plain lock timeout.
                context.checkpoint()?;
                return Err(failed(
                    "lock_timeout",
                    format!("timed out waiting for lock {}", lock_path.display()),
                ));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                std::thread::sleep(
                    LOCK_POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())),
                );
            }
        }
    }
    Ok(PathLock { _file: file })
}

pub(super) fn with_path_lock<T>(
    lock_path: &Path,
    timeout: Duration,
    context: &ExecutionContext<'_>,
    action: impl FnOnce() -> Result<T, AppletError>,
) -> Result<T, AppletError> {
    // Keep this descriptor alive through the operation. The stable lock file
    // is intentionally retained: unlinking it could split concurrent lockers
    // across different inodes.
    let _guard = acquire_path_lock(lock_path, timeout, context)?;
    action()
}

/// Replace one path while the caller holds a stable advisory lock for it.
/// `path` must already be resolved against the workflow working directory.
pub(crate) fn atomic_replace_locked(
    path: &Path,
    content: &[u8],
    expected_hash: &str,
    symlink_policy: &str,
    permissions_policy: &str,
    mode: Option<u32>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    if content.len() > MAX_FILE_BYTES {
        return Err(invalid(format!(
            "file content exceeds the {MAX_FILE_BYTES}-byte limit"
        )));
    }
    write_atomic_locked(
        path,
        content,
        expected_hash,
        parse_symlink_policy_value(symlink_policy)?,
        parse_permission_policy_value(permissions_policy, mode)?,
        context,
    )
}

fn write_atomic_locked(
    path: &Path,
    content: &[u8],
    expected_hash: &str,
    symlink_policy: SymlinkPolicy,
    permission_policy: PermissionPolicy,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let _ = result_path(path)?;
    if symlink_policy == SymlinkPolicy::Reject {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "refusing to write through symlink {}",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error, format!("checking {}", path.display()))),
        }
    }
    let initial_broken_link = broken_symlink(path)?;
    if symlink_policy == SymlinkPolicy::Preserve && initial_broken_link.is_some() {
        return Err(super::absence(
            "not_found",
            format!("symlink target does not exist: {}", path.display()),
        ));
    }
    context.checkpoint()?;
    let initial = match inspect(path, context) {
        Ok(initial) => initial,
        Err(error)
            if symlink_policy == SymlinkPolicy::Replace
                && initial_broken_link.is_some()
                && error.kind == super::AppletErrorKind::Absence =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    match (&initial, expected_hash) {
        (None, "missing") => {}
        (None, _) => {
            return Err(failed(
                "conflict",
                format!(
                    "expected file content at {} but the file is missing",
                    path.display()
                ),
            ));
        }
        (Some(_), "missing") => {
            return Err(failed(
                "conflict",
                format!("expected {} to be missing", path.display()),
            ));
        }
        (Some(state), expected) if state.content_hash != expected => {
            return Err(failed(
                "conflict",
                format!("content changed since it was read: {}", path.display()),
            ));
        }
        (Some(_), _) => {}
    }

    if matches!(permission_policy, PermissionPolicy::Preserve) {
        if let Some(unchanged) = initial.as_ref().filter(|state| state.content == content) {
            return Ok(serde_json::json!({
                "path": result_path(path)?,
                "content_hash": unchanged.content_hash,
                "snapshot": unchanged.snapshot,
                "changed": false,
            }));
        }
    }

    let target = if symlink_policy == SymlinkPolicy::Preserve {
        initial
            .as_ref()
            .filter(|state| state.symlink.is_some())
            .map(|state| state.resolved_path.clone())
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .ok_or_else(|| invalid(format!("path has no file name: {}", target.display())))?;

    let mode = match permission_policy {
        PermissionPolicy::Private => Some(0o600),
        PermissionPolicy::Exact(mode) => Some(mode),
        PermissionPolicy::Preserve => initial
            .as_ref()
            .and_then(|state| state.mode)
            .or(Some(0o600)),
    };
    let (temp_path, mut temp_file) = create_temp(parent, file_name)?;
    let mut cleanup = TempCleanup(Some(temp_path.clone()));
    write_chunked(&mut temp_file, content, &temp_path, context)?;
    if let Some(mode) = mode {
        set_mode(&temp_file, mode).map_err(|error| {
            io_error(
                error,
                format!("setting permissions on {}", temp_path.display()),
            )
        })?;
    }
    // Each single sync, link-metadata, or rename call below can still block
    // inside the operating system; only the slices between calls are
    // interruptible. Error codes are unchanged: rename and parent-sync
    // failures keep reporting `uncertain_effect`.
    context.checkpoint()?;
    temp_file.sync_all().map_err(|error| {
        io_error(
            error,
            format!("syncing temporary file {}", temp_path.display()),
        )
    })?;
    drop(temp_file);

    context.checkpoint()?;
    let current_broken_link = broken_symlink(path)?;
    context.checkpoint()?;
    let current = match inspect(path, context) {
        Ok(current) => current,
        Err(error)
            if symlink_policy == SymlinkPolicy::Replace
                && current_broken_link.is_some()
                && error.kind == super::AppletErrorKind::Absence =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    if current.as_ref().map(|state| state.snapshot.as_str())
        != initial.as_ref().map(|state| state.snapshot.as_str())
        || !same_broken_symlink(initial_broken_link.as_ref(), current_broken_link.as_ref())
    {
        return Err(failed(
            "conflict",
            format!(
                "file changed while preparing atomic write: {}",
                path.display()
            ),
        ));
    }
    if symlink_policy == SymlinkPolicy::Reject
        && fs::symlink_metadata(path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    {
        return Err(failed(
            "conflict",
            format!(
                "path became a symlink while preparing write: {}",
                path.display()
            ),
        ));
    }
    if symlink_policy == SymlinkPolicy::Preserve {
        let target_now = current.as_ref().map(|state| &state.resolved_path);
        if initial
            .as_ref()
            .and_then(|state| state.symlink.as_ref())
            .is_some()
            && target_now != Some(&target)
        {
            return Err(failed(
                "conflict",
                format!(
                    "symlink target changed while preparing write: {}",
                    path.display()
                ),
            ));
        }
    }

    context.checkpoint()?;
    // On Windows this also asks the OS to persist the rename entry; on Unix
    // it is a plain rename and the parent flush below provides durability.
    crate::platform::replace_file(&temp_path, &target).map_err(|error| {
        if error.published {
            // The temporary is gone: the rename succeeded and only the
            // durability flush failed. The replacement must be treated as
            // possibly live instead of re-run.
            failed(
                "uncertain_effect",
                format!(
                    "replacement of {} was published but its directory entry may not be on disk: {}",
                    target.display(),
                    error.error
                ),
            )
        } else {
            failed(
                "uncertain_effect",
                format!(
                    "could not confirm replacement of {}: {}",
                    target.display(),
                    error.error
                ),
            )
        }
    })?;
    cleanup.0 = None;
    context.checkpoint()?;
    sync_directory(parent).map_err(|error| {
        failed(
            "uncertain_effect",
            format!(
                "replacement occurred but syncing its parent failed for {}: {error}",
                target.display()
            ),
        )
    })?;
    context.checkpoint()?;

    // Report the same path view that callers used to read the snapshot. In
    // particular, `preserve` must keep reporting the original symlink rather
    // than only the canonical target we atomically replaced.
    context.checkpoint()?;
    let written = inspect(path, context)
        .map_err(|error| {
            failed(
                "uncertain_effect",
                format!(
                    "replacement occurred but verification failed for {}: {error}",
                    path.display()
                ),
            )
        })?
        .ok_or_else(|| {
            failed(
                "uncertain_effect",
                format!(
                    "replacement occurred but the file is missing: {}",
                    path.display()
                ),
            )
        })?;
    Ok(serde_json::json!({
        "path": result_path(path)?,
        "content_hash": written.content_hash,
        "snapshot": written.snapshot,
        "changed": true,
    }))
}

pub(crate) fn write_jsonc_update(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
    updated: impl FnOnce(&str) -> Result<Option<String>, AppletError>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let path = resolve_path(required_string(args, "path")?, cwd);
    let _ = result_path(&path)?;
    let expected_hash = required_string(args, "expected_content_hash")?;
    let symlink_policy = parse_symlink_policy(args)?;
    let permission_policy = parse_permission_policy(args)?;
    let lock_path = lock_path(args, &path, cwd)?;
    validate_lock_path(&lock_path, &path)?;
    let lock_timeout = lock_timeout(args)?;
    with_path_lock(&lock_path, lock_timeout, context, || {
        let initial = inspect(&path, context)?.ok_or_else(|| {
            super::absence(
                "not_found",
                format!("file does not exist: {}", path.display()),
            )
        })?;
        if initial.content_hash != expected_hash {
            return Err(failed(
                "conflict",
                format!("content changed since it was read: {}", path.display()),
            ));
        }
        let original = String::from_utf8(initial.content.clone())
            .map_err(|error| invalid(format!("JSONC file is not UTF-8: {error}")))?;
        let Some(updated) = updated(&original)? else {
            context.checkpoint()?;
            let current = inspect(&path, context)?.ok_or_else(|| {
                failed(
                    "conflict",
                    format!("file disappeared during JSONC update: {}", path.display()),
                )
            })?;
            if current.snapshot != initial.snapshot {
                return Err(failed(
                    "conflict",
                    format!("file changed during JSONC update: {}", path.display()),
                ));
            }
            return Ok(serde_json::json!({
                "path": result_path(&path)?,
                "content_hash": initial.content_hash,
                "snapshot": initial.snapshot,
                "changed": false,
            }));
        };
        write_atomic_locked(
            &path,
            updated.as_bytes(),
            expected_hash,
            symlink_policy,
            permission_policy,
            context,
        )
    })
}

pub(crate) fn resolve_path(path: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// The exact text to return for a filesystem result path. Callers feed a
/// returned path into later operations, and a lossy rendering of a
/// non-UTF-8 path (for example a relative path joined to a non-UTF-8
/// working directory) can name a different existing file. Refuse such a
/// path before any mutation instead of returning a misleading result.
fn result_path(path: &Path) -> Result<&str, AppletError> {
    path.to_str().ok_or_else(|| {
        invalid(format!(
            "path is not valid UTF-8 and cannot be returned losslessly; refusing to operate so the result path cannot name a different file (use a UTF-8 working directory or an absolute UTF-8 path): {}",
            path.display()
        ))
    })
}

fn broken_symlink(path: &Path) -> Result<Option<BrokenSymlink>, AppletError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error, format!("checking {}", path.display()))),
    };
    match fs::canonicalize(path) {
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let target = fs::read_link(path)
                .map_err(|error| io_error(error, format!("reading symlink {}", path.display())))?;
            Ok(Some(BrokenSymlink { metadata, target }))
        }
        Err(error) => Err(io_error(
            error,
            format!("resolving symlink {}", path.display()),
        )),
    }
}

fn same_broken_symlink(left: Option<&BrokenSymlink>, right: Option<&BrokenSymlink>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            same_metadata(&left.metadata, &right.metadata) && left.target == right.target
        }
        (None, None) => true,
        _ => false,
    }
}

fn parse_symlink_policy(args: &Map<String, Value>) -> Result<SymlinkPolicy, AppletError> {
    parse_symlink_policy_value(required_string(args, "symlink_policy")?)
}

fn parse_symlink_policy_value(value: &str) -> Result<SymlinkPolicy, AppletError> {
    match value {
        "preserve" => Ok(SymlinkPolicy::Preserve),
        "reject" => Ok(SymlinkPolicy::Reject),
        "replace" => Ok(SymlinkPolicy::Replace),
        other => Err(invalid(format!(
            "args.symlink_policy must be preserve, reject, or replace; received {other:?}"
        ))),
    }
}

fn parse_permission_policy(args: &Map<String, Value>) -> Result<PermissionPolicy, AppletError> {
    let mode = args
        .get("mode")
        .and_then(Value::as_u64)
        .and_then(|mode| u32::try_from(mode).ok());
    parse_permission_policy_value(required_string(args, "permissions_policy")?, mode)
}

fn parse_permission_policy_value(
    value: &str,
    mode: Option<u32>,
) -> Result<PermissionPolicy, AppletError> {
    let policy = match value {
        "preserve" => Ok(PermissionPolicy::Preserve),
        "private" => Ok(PermissionPolicy::Private),
        "exact" => {
            let mode = mode.filter(|mode| *mode <= 0o7777).ok_or_else(|| {
                invalid("args.mode must be an integer permission mode from 0 through 4095")
            })?;
            Ok(PermissionPolicy::Exact(mode))
        }
        other => Err(invalid(format!(
            "args.permissions_policy must be preserve, private, or exact; received {other:?}"
        ))),
    }?;
    #[cfg(not(unix))]
    if !matches!(policy, PermissionPolicy::Preserve) {
        return Err(invalid(
            "private and exact permission policies are supported only on Unix",
        ));
    }
    Ok(policy)
}

fn lock_path(args: &Map<String, Value>, target: &Path, cwd: &Path) -> Result<PathBuf, AppletError> {
    if let Some(lock) = args.get("lock") {
        let lock = lock
            .as_str()
            .ok_or_else(|| invalid("args.lock must be a string"))?;
        return Ok(resolve_path(lock, cwd));
    }
    default_lock_path(target)
}

pub(crate) fn default_lock_path(target: &Path) -> Result<PathBuf, AppletError> {
    target
        .file_name()
        .ok_or_else(|| invalid(format!("path has no file name: {}", target.display())))?;
    // Every spelling of one file must name the same lock, so the lock sits
    // beside the canonical identity rather than beside the spelling given.
    let identity = lock_identity(target);
    let name = identity
        .file_name()
        .ok_or_else(|| invalid(format!("path has no file name: {}", target.display())))?;
    let mut lock_name = sibling_prefix(&case_folded_name(name));
    lock_name.push(".lock");
    let directory = identity.parent().unwrap_or(Path::new("."));
    Ok(directory.join(lock_name))
}

/// The canonical identity a default lock is derived from. A symlink alias
/// resolves to its referent, `./a/../a/f` resolves to `a/f`, and an existing
/// file on Windows takes its on-disk letter case. A target that does not
/// exist yet keeps its name inside its canonical parent, because the parent
/// is the only part that must already exist. When nothing canonicalizes (for
/// example a missing parent) the lexical path is used and the write itself
/// reports the error.
pub(crate) fn lock_identity(target: &Path) -> PathBuf {
    if let Ok(resolved) = fs::canonicalize(target) {
        return strip_verbatim(resolved);
    }
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match (fs::canonicalize(parent), target.file_name()) {
        (Ok(directory), Some(name)) => strip_verbatim(directory).join(name),
        _ => target.to_path_buf(),
    }
}

/// Windows canonical paths carry the verbatim `\\?\` prefix. Drive-letter
/// paths lose it so the lock path reads, and compares, like the lexical path.
/// UNC and other verbatim forms are returned unchanged.
#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let stripped = path.to_str().and_then(|text| {
        let rest = text.strip_prefix(r"\\?\")?;
        let drive = rest.as_bytes();
        (drive.len() >= 3
            && drive[0].is_ascii_alphabetic()
            && drive[1] == b':'
            && drive[2] == b'\\')
            .then(|| PathBuf::from(rest))
    });
    stripped.unwrap_or(path)
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

/// NTFS compares names case-insensitively, so on Windows names that differ
/// only in case (including long names that are hashed) fold to one lock. The
/// fold may also merge two distinct names on a case-sensitive directory; that
/// only serializes them and cannot make a write succeed that should conflict.
#[cfg(windows)]
fn case_folded_name(name: &std::ffi::OsStr) -> OsString {
    OsString::from(name.to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
fn case_folded_name(name: &std::ffi::OsStr) -> OsString {
    name.to_os_string()
}

/// The prefix of the lock and temporary files kept next to `file_name`:
/// `.NAME.execute-applet` while that leaves room for a suffix within the 255-byte
/// name limit of common file systems, otherwise a stable digest of the name.
pub(crate) fn sibling_prefix(file_name: &std::ffi::OsStr) -> OsString {
    const MAX_READABLE_NAME_BYTES: usize = 180;
    if file_name.len() <= MAX_READABLE_NAME_BYTES {
        let mut prefix = OsString::from(".");
        prefix.push(file_name);
        prefix.push(".execute-applet");
        return prefix;
    }
    let digest = Sha256::digest(file_name.as_encoded_bytes());
    OsString::from(format!(".execute-applet.{}", to_hex(&digest[..8])))
}

fn lock_timeout(args: &Map<String, Value>) -> Result<Duration, AppletError> {
    let timeout_ms = match args.get("lock_timeout_ms") {
        None => 30_000,
        Some(value) => value
            .as_u64()
            .filter(|value| *value <= 300_000)
            .ok_or_else(|| {
                invalid("args.lock_timeout_ms must be an integer from 0 through 300000")
            })?,
    };
    Ok(Duration::from_millis(timeout_ms))
}

pub(crate) fn validate_lock_path(lock: &Path, target: &Path) -> Result<(), AppletError> {
    if lock == target
        || (fs::canonicalize(lock).ok().is_some()
            && fs::canonicalize(lock).ok() == fs::canonicalize(target).ok())
    {
        return Err(invalid(
            "lock path must be distinct from the file being updated",
        ));
    }
    #[cfg(unix)]
    let (lock_metadata, target_metadata) = (
        fs::symlink_metadata(lock).ok(),
        fs::symlink_metadata(target).ok(),
    );
    #[cfg(unix)]
    if let (Some(lock_metadata), Some(target_metadata)) = (lock_metadata, target_metadata) {
        use std::os::unix::fs::MetadataExt;
        if lock_metadata.dev() == target_metadata.dev()
            && lock_metadata.ino() == target_metadata.ino()
        {
            return Err(invalid(
                "lock path must be distinct from the file being updated",
            ));
        }
    }
    Ok(())
}

fn inspect(path: &Path, context: &ExecutionContext<'_>) -> Result<Option<FileState>, AppletError> {
    context.checkpoint()?;
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error, format!("checking {}", path.display()))),
    };
    let symlink = if link_metadata.file_type().is_symlink() {
        context.checkpoint()?;
        let target = fs::read_link(path)
            .map_err(|error| io_error(error, format!("reading symlink {}", path.display())))?;
        Some(target.to_string_lossy().into_owned())
    } else {
        None
    };
    let resolved_path = fs::canonicalize(path)
        .map_err(|error| io_error(error, format!("resolving {}", path.display())))?;
    // Bounded open (threat model H7): the resolved path carries no symlink,
    // so refusing a final link here only re-verifies the resolution, while
    // the nonblocking open keeps a FIFO target from hanging.
    let mut file = super::open_regular_input(&resolved_path, path, true, context)?;
    let before = file
        .metadata()
        .map_err(|error| io_error(error, format!("checking {}", path.display())))?;
    if !before.is_file() {
        return Err(invalid(format!(
            "path is not a regular file: {}",
            path.display()
        )));
    }
    let mut content = Vec::new();
    read_chunked(&mut file, &mut content, path, context)?;
    if content.len() > MAX_FILE_BYTES {
        return Err(invalid(format!(
            "file exceeds the {MAX_FILE_BYTES}-byte read limit: {}",
            path.display()
        )));
    }
    let after = file
        .metadata()
        .map_err(|error| io_error(error, format!("checking {}", path.display())))?;
    if !same_metadata(&before, &after) {
        return Err(failed(
            "conflict",
            format!("file changed while reading: {}", path.display()),
        ));
    }
    let link_after = fs::symlink_metadata(path)
        .map_err(|error| io_error(error, format!("rechecking {}", path.display())))?;
    if !same_metadata(&link_metadata, &link_after)
        || (symlink.is_some()
            && fs::read_link(path)
                .ok()
                .map(|value| value.to_string_lossy().into_owned())
                != symlink)
        || fs::canonicalize(path).ok().as_deref() != Some(resolved_path.as_path())
    {
        return Err(failed(
            "conflict",
            format!("path changed while reading: {}", path.display()),
        ));
    }

    let content_hash = hash_chunked(&content, context)?;
    let mut hasher = Sha256::new();
    hasher.update(path.as_os_str().to_string_lossy().as_bytes());
    hasher.update([0]);
    add_metadata(&mut hasher, &link_metadata);
    hasher.update(symlink.as_deref().unwrap_or("").as_bytes());
    hasher.update([0]);
    hasher.update(resolved_path.as_os_str().to_string_lossy().as_bytes());
    hasher.update([0]);
    add_metadata(&mut hasher, &after);
    hasher.update(content_hash.as_bytes());
    let snapshot = to_hex(hasher.finalize().as_slice());
    let mode = file_mode(&after);
    Ok(Some(FileState {
        content,
        content_hash,
        snapshot,
        symlink,
        resolved_path,
        mode,
    }))
}

/// Copy at most `MAX_FILE_BYTES + 1` bytes from `file` in chunks, checking
/// the execution budget between chunks. Each individual `read` call can
/// still block on a stalled filesystem; only the slices between calls are
/// interruptible. Reads one byte past the limit so callers can distinguish
/// an over-limit file without a second pass.
fn read_chunked(
    file: &mut File,
    content: &mut Vec<u8>,
    path: &Path,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    let limit = MAX_FILE_BYTES + 1;
    let mut chunk = vec![0u8; IO_CHUNK_BYTES];
    let mut work = 0usize;
    loop {
        context.checkpoint()?;
        let end = content.len().saturating_add(IO_CHUNK_BYTES).min(limit);
        let want = end.saturating_sub(content.len());
        if want == 0 {
            // Already read past the limit; drain nothing further.
            break;
        }
        let read = file
            .read(&mut chunk[..want])
            .map_err(|error| io_error(error, format!("reading {}", path.display())))?;
        if read == 0 {
            break;
        }
        content.extend_from_slice(&chunk[..read]);
        work += 1;
        checkpoint_work(context, work)?;
        if content.len() >= limit {
            break;
        }
    }
    Ok(())
}

/// Write `content` to an already-created temp file in chunks, checking the
/// execution budget between chunks. A cancelled or expired caller stops
/// before the rename, so no partial replacement becomes visible.
fn write_chunked(
    file: &mut File,
    content: &[u8],
    temp_path: &Path,
    context: &ExecutionContext<'_>,
) -> Result<(), AppletError> {
    let mut written = 0usize;
    let mut work = 0usize;
    while written < content.len() {
        context.checkpoint()?;
        let end = written.saturating_add(IO_CHUNK_BYTES).min(content.len());
        file.write_all(&content[written..end]).map_err(|error| {
            io_error(
                error,
                format!("writing temporary file {}", temp_path.display()),
            )
        })?;
        written = end;
        work += 1;
        checkpoint_work(context, work)?;
    }
    Ok(())
}

/// Hash `content` in 64 KiB chunks, checking the budget for every chunk so
/// a large file stops promptly on cancellation or deadline instead of
/// hashing to completion first. Each chunk hash is plain CPU work; a thread
/// blocked inside a single operating-system call stays blocked, and only the
/// slices between calls are interruptible.
fn hash_chunked(content: &[u8], context: &ExecutionContext<'_>) -> Result<String, AppletError> {
    let mut hasher = Sha256::new();
    for chunk in content.chunks(IO_CHUNK_BYTES) {
        context.checkpoint()?;
        hasher.update(chunk);
    }
    context.checkpoint()?;
    Ok(to_hex(hasher.finalize().as_slice()))
}

fn same_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
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

fn add_metadata(hasher: &mut Sha256, metadata: &fs::Metadata) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        for value in [
            metadata.dev(),
            metadata.ino(),
            u64::from(metadata.mode()),
            metadata.len(),
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
        ] {
            hasher.update(value.to_le_bytes());
        }
    }
    #[cfg(not(unix))]
    {
        hasher.update(metadata.len().to_le_bytes());
        if let Ok(modified) = metadata.modified() {
            if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                hasher.update(duration.as_nanos().to_le_bytes());
            }
        }
        hasher.update([u8::from(metadata.permissions().readonly())]);
    }
}

fn file_mode(metadata: &fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn set_mode(file: &File, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (file, mode);
        Ok(())
    }
}

pub(crate) fn hash(content: &[u8]) -> String {
    to_hex(Sha256::digest(content).as_slice())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn create_temp(parent: &Path, file_name: &std::ffi::OsStr) -> Result<(PathBuf, File), AppletError> {
    let prefix = sibling_prefix(file_name);
    reclaim_abandoned_temps(parent, &prefix);
    for _ in 0..100 {
        let sequence = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = prefix.clone();
        temp_name.push(format!(".{}.{}.tmp", std::process::id(), sequence));
        let temp_path = parent.join(temp_name);
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error, format!("creating {}", temp_path.display()))),
        }
    }
    Err(failed(
        "io_error",
        "could not allocate a unique temporary file name",
    ))
}

fn sync_directory(path: &Path) -> Result<(), AppletError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| io_error(error, format!("syncing directory {}", path.display())))?;
    }
    #[cfg(windows)]
    {
        // Ask the OS to persist a just-published rename's directory entry.
        // Success means the flush call succeeded, not that the entry provably
        // survives power loss; see `platform::windows::sync_directory`.
        crate::platform::windows::sync_directory(path)
            .map_err(|error| io_error(error, format!("syncing directory {}", path.display())))?;
    }
    #[cfg(not(any(unix, windows)))]
    let _ = path;
    Ok(())
}

/// Remove sibling temporary files for this output whose creating process no
/// longer exists. `TempCleanup` cannot run after a hard interruption
/// (SIGKILL or power loss), so the next write for the same output reclaims
/// those orphans here, mirroring `transaction::reclaim_abandoned_stages`.
/// Anything that cannot be proven abandoned is kept: names that do not parse
/// as `<prefix>.<pid>.<seq>.tmp`, temporaries owned by this or another live
/// process (via the shared `transaction::process_alive` helper), links,
/// non-files, and (on Unix) files owned by someone else. Removal is
/// best-effort through the same `TempCleanup` the ordinary failure path uses.
fn reclaim_abandoned_temps(parent: &Path, prefix: &std::ffi::OsStr) {
    let prefix = format!("{}.", prefix.to_string_lossy());
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(suffix) = file_name
            .strip_prefix(prefix.as_str())
            .and_then(|suffix| suffix.strip_suffix(".tmp"))
        else {
            continue;
        };
        let Some((pid, sequence)) = suffix.split_once('.') else {
            continue;
        };
        let (Ok(pid), Ok(_)) = (pid.parse::<u32>(), sequence.parse::<u64>()) else {
            continue;
        };
        if pid == std::process::id() || super::transaction::process_alive(pid) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != crate::platform::unix_ffi::effective_uid() {
                continue;
            }
        }
        drop(TempCleanup(Some(path)));
    }
}

// Normal error returns and unwinding clean up the temporary file. Forced
// process termination can leave a hidden `.execute-applet.*.tmp` file behind;
// the next write for the same output reclaims it once its creating process
// is gone (see `reclaim_abandoned_temps`).
struct TempCleanup(Option<PathBuf>);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applets::AppletErrorKind;

    fn write_args() -> Map<String, Value> {
        serde_json::json!({
            "path": "out.txt",
            "expected_content_hash": "missing",
            "symlink_policy": "reject",
            "permissions_policy": "preserve",
            "content": "written",
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn write_atomic_result_path_names_the_file_it_wrote() {
        let directory = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        let result = write_atomic(&write_args(), b"", directory.path(), &context).unwrap();
        let expected = directory.path().join("out.txt");
        assert_eq!(result["path"], expected.to_str().unwrap());
        let returned = Path::new(result["path"].as_str().unwrap());
        assert_eq!(fs::read(returned).unwrap(), b"written");
    }

    #[test]
    fn orphaned_atomic_write_temporary_is_reclaimed_by_the_next_write() {
        fn dead_pid() -> u32 {
            #[cfg(windows)]
            let mut child = std::process::Command::new("cmd")
                .args(["/C", "exit", "0"])
                .spawn()
                .unwrap();
            #[cfg(not(windows))]
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        }

        let directory = tempfile::tempdir().unwrap();
        let prefix = sibling_prefix(std::ffi::OsStr::new("out.txt"));
        let prefix = prefix.to_string_lossy();
        let dead = dead_pid();

        // Simulate a hard interruption before the rename: a temporary for
        // this output left behind by a process that no longer exists.
        let orphaned = directory.path().join(format!("{prefix}.{dead}.0.tmp"));
        fs::write(&orphaned, b"orphaned").unwrap();

        // A temporary belonging to this live process, one for a different
        // output, and an unparsable name must all survive the reclaim.
        let active = directory
            .path()
            .join(format!("{prefix}.{}.999.tmp", std::process::id()));
        fs::write(&active, b"active").unwrap();
        let other_prefix = sibling_prefix(std::ffi::OsStr::new("other.txt"));
        let other = directory
            .path()
            .join(format!("{}.{dead}.0.tmp", other_prefix.to_string_lossy()));
        fs::write(&other, b"other").unwrap();
        let unparsable = directory.path().join(format!("{prefix}.nope.tmp"));
        fs::write(&unparsable, b"unparsable").unwrap();

        let context = ExecutionContext::unrestricted();
        let result = write_atomic(&write_args(), b"", directory.path(), &context).unwrap();
        assert_eq!(result["changed"], true);
        assert!(!orphaned.exists(), "orphaned temporary must be reclaimed");
        assert!(active.exists(), "live process temporary must be kept");
        assert!(other.exists(), "another output's temporary must be kept");
        assert!(unparsable.exists(), "unparsable name must be kept");
        assert_eq!(
            fs::read(directory.path().join("out.txt")).unwrap(),
            b"written"
        );
    }

    /// A relative path in a non-UTF-8 working directory resolves to a path
    /// whose lossy rendering names a sibling directory. The applet must
    /// refuse before mutating instead of returning that sibling's path.
    #[cfg(unix)]
    #[test]
    fn non_utf8_cwd_fails_before_mutation_so_result_cannot_name_sibling() {
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().unwrap();
        let actual = directory
            .path()
            .join(OsString::from_vec(b"bad-\xff".to_vec()));
        let lossy_name = actual.file_name().unwrap().to_string_lossy().into_owned();
        let sibling = directory.path().join(&lossy_name);
        assert_ne!(actual, sibling);
        fs::create_dir(&actual).unwrap();
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("out.txt"), b"sibling").unwrap();
        let context = ExecutionContext::unrestricted();

        let error = write_atomic(&write_args(), b"", &actual, &context).unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert!(error.message.contains("cannot be returned losslessly"));
        assert!(!actual.join("out.txt").exists());
        assert_eq!(fs::read(sibling.join("out.txt")).unwrap(), b"sibling");

        fs::write(actual.join("victim.txt"), b"actual").unwrap();
        fs::write(sibling.join("victim.txt"), b"sibling-victim").unwrap();
        let snapshot = inspect(&actual.join("victim.txt"), &context)
            .unwrap()
            .unwrap()
            .snapshot;
        let args = serde_json::json!({
            "path": "victim.txt",
            "expected_snapshot": snapshot,
        })
        .as_object()
        .unwrap()
        .clone();
        let error = unlink_checked(&args, &actual, &context).unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert!(error.message.contains("cannot be returned losslessly"));
        assert_eq!(fs::read(actual.join("victim.txt")).unwrap(), b"actual");
        assert_eq!(
            fs::read(sibling.join("victim.txt")).unwrap(),
            b"sibling-victim"
        );
    }

    fn lock_for(target: &Path) -> PathBuf {
        default_lock_path(target).unwrap()
    }

    /// Holding the default lock of one spelling must block the other spelling.
    fn assert_exclusive_locks(first: &Path, second: &Path) {
        let context = ExecutionContext::unrestricted();
        let held = acquire_path_lock(&lock_for(first), Duration::ZERO, &context).unwrap();
        let error = acquire_path_lock(&lock_for(second), Duration::ZERO, &context)
            .err()
            .expect("the second spelling must wait on the lock the first holds");
        assert_eq!(error.code, "lock_timeout");
        drop(held);
    }

    #[test]
    fn default_lock_is_shared_by_parent_directory_spellings() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("real.txt"), b"real").unwrap();
        fs::create_dir(directory.path().join("sub")).unwrap();
        let spelled = directory.path().join("sub").join("..").join("real.txt");
        assert_eq!(
            lock_for(&directory.path().join("real.txt")),
            lock_for(&spelled)
        );
        // A target that does not exist yet still resolves its parent.
        let missing = directory.path().join("sub").join("..").join("new.txt");
        assert_eq!(
            lock_for(&directory.path().join("new.txt")),
            lock_for(&missing)
        );
    }

    #[cfg(unix)]
    #[test]
    fn default_lock_is_shared_by_symlink_alias_and_excludes_the_real_path() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real.txt");
        let alias = directory.path().join("alias.txt");
        fs::write(&real, b"real").unwrap();
        std::os::unix::fs::symlink("real.txt", &alias).unwrap();
        assert_eq!(lock_for(&real), lock_for(&alias));
        assert_exclusive_locks(&real, &alias);
    }

    /// NTFS names differ only in case for the same file. Long names take the
    /// hashed lock prefix, so both the readable and the hashed forms are covered.
    #[cfg(windows)]
    #[test]
    fn default_lock_folds_windows_case_aliases_including_long_names() {
        let directory = tempfile::tempdir().unwrap();
        let upper = format!("{}X", "a".repeat(190));
        let lower = format!("{}x", "a".repeat(190));
        fs::write(directory.path().join(&upper), b"real").unwrap();
        assert_eq!(
            lock_for(&directory.path().join(&upper)),
            lock_for(&directory.path().join(&lower))
        );
        assert_exclusive_locks(
            &directory.path().join(&upper),
            &directory.path().join(&lower),
        );
        assert_eq!(
            lock_for(&directory.path().join("Report.TXT")),
            lock_for(&directory.path().join("report.txt"))
        );
    }

    /// A real-path writer and a symlink-alias writer that both expect the same
    /// content hash must not both succeed. Both start while the lock for the
    /// real file is held; the outcome is decided by serialization.
    #[cfg(unix)]
    #[test]
    fn alias_and_real_path_writers_with_same_expected_hash_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real.txt");
        let alias = directory.path().join("alias.txt");
        fs::write(&real, b"start").unwrap();
        std::os::unix::fs::symlink("real.txt", &alias).unwrap();
        let expected = hash(b"start");
        let cwd = directory.path().to_path_buf();
        let held = {
            let context = ExecutionContext::unrestricted();
            acquire_path_lock(&lock_for(&real), Duration::from_secs(5), &context).unwrap()
        };
        let spawn_writer = |path: PathBuf, content: &'static str| {
            let expected = expected.clone();
            let cwd = cwd.clone();
            std::thread::spawn(move || {
                let args = serde_json::json!({
                    "path": path.to_str().unwrap(),
                    "expected_content_hash": expected,
                    "symlink_policy": "preserve",
                    "permissions_policy": "preserve",
                    "content": content,
                })
                .as_object()
                .unwrap()
                .clone();
                let context = ExecutionContext::unrestricted();
                write_atomic(&args, b"", &cwd, &context)
            })
        };
        let through_alias = spawn_writer(alias.clone(), "via-alias");
        let through_real = spawn_writer(real.clone(), "via-real");
        std::thread::sleep(Duration::from_millis(100));
        drop(held);
        let results = [through_alias.join().unwrap(), through_real.join().unwrap()];
        let successes = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(successes, 1, "exactly one conflicting write may succeed");
        assert!(results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(|error| error.code == "conflict"));
    }

    fn path_args(path: &Path, extra: Value) -> Map<String, Value> {
        let mut args = serde_json::json!({ "path": path.to_str().unwrap() })
            .as_object()
            .unwrap()
            .clone();
        if let Value::Object(extra) = extra {
            args.extend(extra);
        }
        args
    }

    #[test]
    fn stat_reports_a_file_and_a_directory_with_typed_fields() {
        let directory = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        let file = directory.path().join("file.txt");
        fs::write(&file, b"hello").unwrap();
        let record = stat(&path_args(&file, Value::Null), directory.path(), &context).unwrap();
        assert_eq!(record["type"], "file");
        assert_eq!(record["size"], 5);
        assert!(record["mtime_ns"].as_i64().is_some(), "{record}");
        assert_eq!(record["mode"].is_u64(), cfg!(unix), "{record}");
        assert_eq!(record["read_only"].is_boolean(), cfg!(windows), "{record}");

        let subdirectory = directory.path().join("sub");
        fs::create_dir(&subdirectory).unwrap();
        let record = stat(
            &path_args(&subdirectory, Value::Null),
            directory.path(),
            &context,
        )
        .unwrap();
        assert_eq!(record["type"], "directory");
    }

    #[test]
    fn stat_of_an_absent_path_is_a_typed_result_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        let missing = directory.path().join("missing");
        let record = stat(
            &path_args(&missing, Value::Null),
            directory.path(),
            &context,
        )
        .unwrap();
        assert_eq!(record["type"], "absent");
        assert!(record["size"].is_null());
        assert!(record["mtime_ns"].is_null());

        let file = directory.path().join("file.txt");
        fs::write(&file, b"").unwrap();
        let under_file = file.join("child");
        let record = stat(
            &path_args(&under_file, Value::Null),
            directory.path(),
            &context,
        )
        .unwrap();
        assert_eq!(record["type"], "absent");
    }

    #[test]
    fn stat_rejects_a_missing_path_argument() {
        let context = ExecutionContext::unrestricted();
        assert!(stat(&Map::new(), Path::new("/"), &context).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stat_does_not_follow_a_trailing_symlink_unless_asked() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        let target = directory.path().join("target.txt");
        fs::write(&target, b"abc").unwrap();
        let link = directory.path().join("link");
        symlink(&target, &link).unwrap();

        let record = stat(&path_args(&link, Value::Null), directory.path(), &context).unwrap();
        assert_eq!(record["type"], "symlink");
        let record = stat(
            &path_args(&link, serde_json::json!({ "follow": true })),
            directory.path(),
            &context,
        )
        .unwrap();
        assert_eq!(record["type"], "file");
        assert_eq!(record["size"], 3);

        let dangling = directory.path().join("dangling");
        symlink(directory.path().join("nowhere"), &dangling).unwrap();
        let record = stat(
            &path_args(&dangling, Value::Null),
            directory.path(),
            &context,
        )
        .unwrap();
        assert_eq!(record["type"], "symlink");
        let record = stat(
            &path_args(&dangling, serde_json::json!({ "follow": true })),
            directory.path(),
            &context,
        )
        .unwrap();
        assert_eq!(record["type"], "absent");
    }

    #[test]
    fn realpath_resolves_an_existing_file_and_refuses_an_absent_one() {
        let directory = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        let file = directory.path().join("file.txt");
        fs::write(&file, b"").unwrap();
        let result = realpath(&path_args(&file, Value::Null), directory.path(), &context).unwrap();
        let expected = strip_verbatim(fs::canonicalize(&file).unwrap());
        assert_eq!(result["realpath"], expected.to_str().unwrap());

        let missing = directory.path().join("missing");
        let error = realpath(
            &path_args(&missing, Value::Null),
            directory.path(),
            &context,
        )
        .unwrap_err();
        assert_eq!(error.code, "not_found");
        assert_eq!(error.kind, AppletErrorKind::Absence);
    }

    #[cfg(unix)]
    #[test]
    fn realpath_resolves_symlinks_and_refuses_a_dangling_link() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let context = ExecutionContext::unrestricted();
        let real_directory = directory.path().join("real");
        fs::create_dir(&real_directory).unwrap();
        let file = real_directory.join("file.txt");
        fs::write(&file, b"").unwrap();
        let link = directory.path().join("link");
        symlink(&real_directory, &link).unwrap();

        let result = realpath(
            &path_args(&link.join("file.txt"), Value::Null),
            directory.path(),
            &context,
        )
        .unwrap();
        let expected = fs::canonicalize(&file).unwrap();
        assert_eq!(result["realpath"], expected.to_str().unwrap());

        let dangling = directory.path().join("dangling");
        symlink(directory.path().join("nowhere"), &dangling).unwrap();
        assert!(realpath(
            &path_args(&dangling, Value::Null),
            directory.path(),
            &context
        )
        .is_err());
    }
}
