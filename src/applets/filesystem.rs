//! Filesystem operations use a retained advisory lock file shared by
//! cooperating callers, hash-based expected-content checks and a metadata
//! recheck immediately before atomic replacement. This is not a portable
//! compare-and-swap against arbitrary writers and does not make multiple files
//! transactional. The parent directory is assumed to be trusted: portable
//! path-based rename still has a narrow race with non-cooperating writers.
//!
//! A preserved final symlink is resolved to its canonical target and opened
//! without following another final symlink; the link and target identity are
//! rechecked before replacement. Writes sync the temp file; Unix then syncs
//! the parent directory, while Windows asks the OS to persist the rename
//! entry during replacement and syncs the parent afterwards. A successful
//! flush means the OS was asked, not that the entry provably survives power
//! loss. Permission preservation covers Unix mode bits only; ownership,
//! ACLs, extended attributes and timestamps are not copied. File content is
//! capped at 16 MiB to keep reads and CAS checks bounded.

use super::{failed, invalid, io_error, required_string, AppletError};
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

pub(super) fn read(args: &Map<String, Value>, cwd: &Path) -> Result<Value, AppletError> {
    let path = resolve_path(required_string(args, "path")?, cwd);
    let state = inspect(&path)?.ok_or_else(|| {
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
) -> Result<Value, AppletError> {
    let path = resolve_path(required_string(args, "path")?, cwd);
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

    with_path_lock(&lock_path, lock_timeout, || {
        write_atomic_locked(
            &path,
            &content,
            expected_hash,
            symlink_policy,
            permission_policy,
        )
    })
}

/// Remove one final filesystem entry only when the snapshot still matches.
/// For a symlink this removes the link itself and never follows it for the
/// unlink operation. Snapshot reads still inspect the target so callers can
/// use `fs.snapshot` for either regular files or live symlinks.
pub(super) fn unlink_checked(args: &Map<String, Value>, cwd: &Path) -> Result<Value, AppletError> {
    let path = resolve_path(required_string(args, "path")?, cwd);
    let expected_snapshot = required_string(args, "expected_snapshot")?;
    let lock_path = lock_path(args, &path, cwd)?;
    validate_lock_path(&lock_path, &path)?;
    let lock_timeout = lock_timeout(args)?;

    with_path_lock(&lock_path, lock_timeout, || {
        let initial = inspect(&path)?.ok_or_else(|| {
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
        let current = inspect(&path)?.ok_or_else(|| {
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
        sync_directory(parent).map_err(|error| {
            failed(
                "uncertain_effect",
                format!("entry was unlinked but parent sync failed: {error}"),
            )
        })?;
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
            "path": path.to_string_lossy(),
            "removed": true,
            "symlink": symlink,
        }))
    })
}

pub(crate) struct PathLock {
    // The OS lock remains held until this file descriptor is dropped.
    _file: File,
}

pub(crate) fn acquire_path_lock(
    lock_path: &Path,
    timeout: Duration,
) -> Result<PathLock, AppletError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
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
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(io_error(error, format!("locking {}", lock_path.display())));
            }
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() >= timeout => {
                return Err(failed(
                    "lock_timeout",
                    format!("timed out waiting for lock {}", lock_path.display()),
                ));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                std::thread::sleep(
                    Duration::from_millis(10).min(timeout.saturating_sub(started.elapsed())),
                );
            }
        }
    }
    Ok(PathLock { _file: file })
}

pub(super) fn with_path_lock<T>(
    lock_path: &Path,
    timeout: Duration,
    action: impl FnOnce() -> Result<T, AppletError>,
) -> Result<T, AppletError> {
    // Keep this descriptor alive through the operation. The stable lock file
    // is intentionally retained: unlinking it could split concurrent lockers
    // across different inodes.
    let _guard = acquire_path_lock(lock_path, timeout)?;
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
    )
}

fn write_atomic_locked(
    path: &Path,
    content: &[u8],
    expected_hash: &str,
    symlink_policy: SymlinkPolicy,
    permission_policy: PermissionPolicy,
) -> Result<Value, AppletError> {
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
    let initial = match inspect(path) {
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
                "path": path.to_string_lossy(),
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
    temp_file.write_all(content).map_err(|error| {
        io_error(
            error,
            format!("writing temporary file {}", temp_path.display()),
        )
    })?;
    if let Some(mode) = mode {
        set_mode(&temp_file, mode).map_err(|error| {
            io_error(
                error,
                format!("setting permissions on {}", temp_path.display()),
            )
        })?;
    }
    temp_file.sync_all().map_err(|error| {
        io_error(
            error,
            format!("syncing temporary file {}", temp_path.display()),
        )
    })?;
    drop(temp_file);

    let current_broken_link = broken_symlink(path)?;
    let current = match inspect(path) {
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
    sync_directory(parent).map_err(|error| {
        failed(
            "uncertain_effect",
            format!(
                "replacement occurred but syncing its parent failed for {}: {error}",
                target.display()
            ),
        )
    })?;

    // Report the same path view that callers used to read the snapshot. In
    // particular, `preserve` must keep reporting the original symlink rather
    // than only the canonical target we atomically replaced.
    let written = inspect(path)
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
        "path": path.to_string_lossy(),
        "content_hash": written.content_hash,
        "snapshot": written.snapshot,
        "changed": true,
    }))
}

pub(crate) fn write_jsonc_update(
    args: &Map<String, Value>,
    cwd: &Path,
    updated: impl FnOnce(&str) -> Result<Option<String>, AppletError>,
) -> Result<Value, AppletError> {
    let path = resolve_path(required_string(args, "path")?, cwd);
    let expected_hash = required_string(args, "expected_content_hash")?;
    let symlink_policy = parse_symlink_policy(args)?;
    let permission_policy = parse_permission_policy(args)?;
    let lock_path = lock_path(args, &path, cwd)?;
    validate_lock_path(&lock_path, &path)?;
    let lock_timeout = lock_timeout(args)?;
    with_path_lock(&lock_path, lock_timeout, || {
        let initial = inspect(&path)?.ok_or_else(|| {
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
            let current = inspect(&path)?.ok_or_else(|| {
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
                "path": path.to_string_lossy(),
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
    let mut lock_name = sibling_prefix(target.file_name().unwrap_or_default());
    lock_name.push(".lock");
    Ok(target.with_file_name(lock_name))
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

fn inspect(path: &Path) -> Result<Option<FileState>, AppletError> {
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error, format!("checking {}", path.display()))),
    };
    let symlink = if link_metadata.file_type().is_symlink() {
        let target = fs::read_link(path)
            .map_err(|error| io_error(error, format!("reading symlink {}", path.display())))?;
        Some(target.to_string_lossy().into_owned())
    } else {
        None
    };
    let resolved_path = fs::canonicalize(path)
        .map_err(|error| io_error(error, format!("resolving {}", path.display())))?;
    let mut file = open_no_follow(&resolved_path)
        .map_err(|error| io_error(error, format!("opening {}", path.display())))?;
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
    (&mut file)
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut content)
        .map_err(|error| io_error(error, format!("reading {}", path.display())))?;
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

    let content_hash = hash(&content);
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

fn open_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(path)
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

// Normal error returns and unwinding clean up the temporary file. Forced
// process termination can leave a hidden `.execute-applet.*.tmp` file; trying
// to reap unknown old names would risk deleting another live invocation's
// temporary file.
struct TempCleanup(Option<PathBuf>);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}
