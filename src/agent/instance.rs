//! `execute run FILE --single-instance LOCK`: refuse to run while another run holds
//! the same lock file. The lock is an exclusive OS file lock taken without
//! waiting, and it is released when the returned handle is dropped. The lock file
//! itself is created if absent and is never written.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

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
