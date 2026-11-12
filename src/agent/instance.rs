//! `execute run FILE --single-instance LOCK`: refuse to run while another run holds
//! the same lock file. The lock is an exclusive OS file lock taken without
//! waiting, and it is released when the returned handle is dropped. The lock file
//! itself is created if absent and is never written.
//!
//! The named locks below serve programs rather than whole runs. A program names a
//! lock; the lock file is `<name>.lock` in the per-user lock directory, so a name
//! cannot point outside it. A held lock is released when its handle drops, and the
//! OS releases it when the process exits however it exits. The handles are not
//! inherited by spawned children, because the files are opened close-on-exec.

use anyhow::{bail, Context, Result};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// Take the lock, or fail at once if another instance holds it. Keep the returned
/// handle alive for the whole run.
pub fn acquire(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening single-instance lock {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!(
                "another instance holds the single-instance lock {}",
                path.display()
            )
        }
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("locking single-instance file {}", path.display()))
        }
    }
}

/// Suffix every lock file carries.
const LOCK_SUFFIX: &str = ".lock";

/// Longest accepted lock name in bytes: the file-name limit most filesystems impose, less
/// the suffix, so the lock file name itself fits.
const MAX_LOCK_NAME_BYTES: usize = 255 - LOCK_SUFFIX.len();

/// How often a waiting acquire retries the lock.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Names Windows reserves as devices, with or without an extension.
const WINDOWS_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Whether a named lock is exclusive or shared with other shared holders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    Exclusive,
    Shared,
}

impl LockMode {
    /// The name the language uses for this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            LockMode::Exclusive => "exclusive",
            LockMode::Shared => "shared",
        }
    }
}

/// A named lock this process holds. Dropping the handle releases the lock, so a
/// caller that scopes the handle releases the lock on every exit path.
#[derive(Debug)]
pub struct NamedLock {
    // Held only for its descriptor: closing it releases the lock.
    _file: File,
    name: String,
    mode: LockMode,
}

impl NamedLock {
    /// The name the lock was taken under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The mode the lock is held in.
    pub fn mode(&self) -> LockMode {
        self.mode
    }
}

/// The outcome of a waiting acquire.
#[derive(Debug)]
pub enum NamedLockWait {
    /// The lock is held; `waited` is how long the caller waited for it.
    Acquired { lock: NamedLock, waited: Duration },
    /// The lock stayed busy for the whole wait.
    TimedOut { waited: Duration },
}

/// The per-user directory named locks live in: `$XDG_RUNTIME_DIR` when it is an absolute
/// path, otherwise the temporary directory, under `execute-locks`. On Unix the effective
/// uid is appended, so users who share a temporary directory do not share lock files.
pub fn named_lock_directory() -> PathBuf {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    };
    #[cfg(unix)]
    {
        base.join(format!(
            "execute-locks-{}",
            crate::platform::unix_ffi::effective_uid()
        ))
    }
    #[cfg(not(unix))]
    {
        base.join("execute-locks")
    }
}

/// Check a lock name. A name is 1 to 250 bytes of ASCII letters, digits, `-`, `_` and
/// `.`; it starts with a letter or digit, does not end with `.`, and is not a Windows
/// device name. Such a name is a single file-name component, so it cannot leave the
/// lock directory.
pub fn validate_lock_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("a lock name must not be empty");
    }
    if name.len() > MAX_LOCK_NAME_BYTES {
        bail!("a lock name must be at most {MAX_LOCK_NAME_BYTES} bytes");
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        bail!("lock name {name:?} must start with a letter or digit");
    }
    if name.ends_with('.') {
        bail!("lock name {name:?} must not end with '.'");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("lock name {name:?} may contain only letters, digits, '-', '_' and '.'");
    }
    let stem = name.split('.').next().unwrap_or_default();
    if WINDOWS_DEVICE_NAMES
        .iter()
        .any(|device| stem.eq_ignore_ascii_case(device))
    {
        bail!("lock name {name:?} is a reserved device name");
    }
    Ok(())
}

/// The lock file for `name` inside `dir`, after the name is checked.
fn lock_file_path(dir: &Path, name: &str) -> Result<PathBuf> {
    validate_lock_name(name)?;
    Ok(dir.join(format!("{name}{LOCK_SUFFIX}")))
}

/// Create the lock directory if it is missing. On Unix an existing directory must be
/// owned by this user and not writable by others, since the lock files in it are
/// trusted to be ours.
fn ensure_lock_directory(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("creating lock directory {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(dir)
            .with_context(|| format!("inspecting lock directory {}", dir.display()))?;
        if metadata.uid() != crate::platform::unix_ffi::effective_uid()
            || metadata.mode() & 0o022 != 0
        {
            bail!(
                "lock directory {} must be owned by this user and not writable by others",
                dir.display()
            );
        }
    }
    Ok(())
}

/// Refuse a lock path that exists but is not a regular file, such as a symbolic link.
fn check_lock_path(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => bail!("lock file {} is not a regular file", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspecting lock file {}", path.display()))
        }
    }
}

/// Open the lock file for a lock that may be taken, creating it if absent.
fn create_lock_file(path: &Path) -> Result<File> {
    check_lock_path(path)?;
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock file {}", path.display()))
}

/// Open the lock file if it exists. A probe never creates a lock file.
fn existing_lock_file(path: &Path) -> Result<Option<File>> {
    if !check_lock_path(path)? {
        return Ok(None);
    }
    OpenOptions::new()
        .read(true)
        .open(path)
        .map(Some)
        .with_context(|| format!("opening lock file {}", path.display()))
}

/// One non-waiting attempt at `mode` on the file.
fn try_lock_mode(file: &File, mode: LockMode) -> Result<(), TryLockError> {
    match mode {
        LockMode::Exclusive => file.try_lock(),
        LockMode::Shared => file.try_lock_shared(),
    }
}

/// Try the named lock once. `Ok(None)` means another holder has it; the caller decides
/// whether to skip the work or wait. The lock file is created if absent.
pub fn try_named_lock(dir: &Path, name: &str, mode: LockMode) -> Result<Option<NamedLock>> {
    let path = lock_file_path(dir, name)?;
    ensure_lock_directory(dir)?;
    let file = create_lock_file(&path)?;
    match try_lock_mode(&file, mode) {
        Ok(()) => Ok(Some(NamedLock {
            _file: file,
            name: name.to_owned(),
            mode,
        })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("locking lock file {}", path.display()))
        }
    }
}

/// Take the named lock, retrying until it is free or `wait` has passed. A zero `wait`
/// makes exactly one attempt.
pub fn acquire_named_lock(
    dir: &Path,
    name: &str,
    mode: LockMode,
    wait: Duration,
) -> Result<NamedLockWait> {
    let path = lock_file_path(dir, name)?;
    ensure_lock_directory(dir)?;
    let file = create_lock_file(&path)?;
    let started = Instant::now();
    // A wait too long to represent never times out.
    let deadline = started.checked_add(wait);
    loop {
        match try_lock_mode(&file, mode) {
            Ok(()) => {
                return Ok(NamedLockWait::Acquired {
                    lock: NamedLock {
                        _file: file,
                        name: name.to_owned(),
                        mode,
                    },
                    waited: started.elapsed(),
                });
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("locking lock file {}", path.display()));
            }
        }
        let pause = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(NamedLockWait::TimedOut {
                        waited: started.elapsed(),
                    });
                }
                (deadline - now).min(WAIT_POLL_INTERVAL)
            }
            None => WAIT_POLL_INTERVAL,
        };
        thread::sleep(pause);
    }
}

/// Whether any process or handle holds the named lock. The probe takes the lock for an
/// instant to test it and releases it at once, so a try made in that instant sees the
/// lock busy. A missing lock file is not held, and the probe does not create one.
pub fn named_lock_held(dir: &Path, name: &str) -> Result<bool> {
    let path = lock_file_path(dir, name)?;
    let Some(file) = existing_lock_file(&path)? else {
        return Ok(false);
    };
    match file.try_lock() {
        Ok(()) => {
            file.unlock()
                .with_context(|| format!("releasing probe of lock file {}", path.display()))?;
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("probing lock file {}", path.display()))
        }
    }
}
