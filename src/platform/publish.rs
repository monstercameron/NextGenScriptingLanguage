//! The Windows atomic-publish contract: one rename plus a parent-directory
//! flush, shared verbatim by `crate::platform` and the standalone Windows test
//! harness (which includes it by path, so this file must stay free of
//! crate-internal references: all doc links resolve within this file).
//!
//! Both the POSIX-semantics fast path and the write-through fallback end with
//! a flush of the published target's parent directory. A flush failure after a
//! successful rename returns [`ReplaceError`] with `published == true`; the
//! target is published and the caller must take the published-but-unsynced
//! path instead of reporting a failed publish. Volumes without flush support
//! fail closed as errors; there is no silent degraded mode. Recording the
//! flush path is not proof the entry survives power loss: that is only shown
//! by the hard-reset lab.
#[cfg(any(windows, test))]
use std::path::Path;

/// Failure of `replace_file`, classified by whether the target was replaced.
///
/// A post-rename durability flush can fail after the rename succeeded. That
/// must not be reported as "could not publish": the new bytes are published
/// and resume or retry may observe them, so callers map `published == true`
/// to the existing published-but-unsynced / `uncertain_effect` path instead
/// of unlinking the (now-gone) temporary and re-running the publish.
///
/// A flush failure on a volume without directory-flush support (FAT/exFAT,
/// some network redirectors) is still `published == true`: previously working
/// publishes on such volumes now surface as errors on purpose (fail-closed),
/// directing the operator to a local NTFS directory, rather than degrading
/// silently to an unflushed entry.
#[derive(Debug)]
pub(crate) struct ReplaceError {
    /// The rename succeeded; only the follow-up durability flush failed.
    pub(crate) published: bool,
    /// The underlying OS error.
    pub(crate) error: std::io::Error,
}

impl ReplaceError {
    pub(crate) fn not_published(error: std::io::Error) -> Self {
        Self {
            published: false,
            error,
        }
    }

    #[cfg(any(windows, test))]
    pub(crate) fn published_unsynced(error: std::io::Error) -> Self {
        Self {
            published: true,
            error,
        }
    }
}

impl std::fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.published {
            write!(
                f,
                "published but its directory entry may not be on disk: {}",
                self.error
            )
        } else {
            write!(f, "{}", self.error)
        }
    }
}

impl std::error::Error for ReplaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// How a Windows atomic publish reached stable storage. Tests record this to
/// assert the requested path was taken without calling Win32. Recording the
/// path is not proof the entry survives power loss: that is only shown by the
/// hard-reset lab.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowsPublishDurability {
    /// POSIX-semantics rename followed by a flush of the published target's
    /// parent directory.
    PosixRenameFlushed,
    /// `MoveFileExW` with `MOVEFILE_WRITE_THROUGH`, followed by a flush of the
    /// published target's parent directory. Write-through is documented for
    /// copy-and-delete moves; for a same-volume rename the persistence is
    /// filesystem dependent and unproven without the hard-reset lab.
    MoveFileWriteThrough,
}

/// The Win32 operations behind [`windows_publish_with`]. Production code uses
/// the native backend in `platform::windows`; tests substitute a recorder so
/// the ordering contract is checked on every platform.
#[cfg(any(windows, test))]
pub(crate) trait WindowsPublishBackend {
    /// Rename `source` over `target` with POSIX semantics. `Ok(false)` means
    /// the volume or OS version lacks support and the caller must fall back.
    /// An `Err` also falls through so the fallback reports the final error.
    fn posix_rename(&self, source: &Path, target: &Path) -> std::io::Result<bool>;
    /// Replace `target` with `source` and ask the OS not to return until the
    /// move is on disk.
    fn write_through_move(&self, source: &Path, target: &Path) -> std::io::Result<()>;
    /// Flush the parent directory of the published `target` so the OS is asked
    /// to persist the rename entry. Success means the flush call succeeded,
    /// not that the entry provably survives power loss.
    fn flush_published(&self, target: &Path) -> std::io::Result<()>;
}

/// Replace `target` with `source` and ask the OS to persist the move: both the
/// POSIX fast path and the write-through fallback end with a parent-directory
/// flush. A flush failure after a successful rename returns
/// [`ReplaceError`] with `published == true`; the target is published and the
/// caller must take the published-but-unsynced path instead of reporting a
/// failed publish.
#[cfg(any(windows, test))]
pub(crate) fn windows_publish_with<B: WindowsPublishBackend>(
    backend: &B,
    source: &Path,
    target: &Path,
) -> std::result::Result<WindowsPublishDurability, ReplaceError> {
    if matches!(backend.posix_rename(source, target), Ok(true)) {
        backend
            .flush_published(target)
            .map_err(ReplaceError::published_unsynced)?;
        return Ok(WindowsPublishDurability::PosixRenameFlushed);
    }
    backend
        .write_through_move(source, target)
        .map_err(ReplaceError::not_published)?;
    backend
        .flush_published(target)
        .map_err(ReplaceError::published_unsynced)?;
    Ok(WindowsPublishDurability::MoveFileWriteThrough)
}
