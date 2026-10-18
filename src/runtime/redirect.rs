//! Output targets of a run: redirect and append files, their authorization,
//! the hardened open, and the durable write.

use super::{write_with_deadline, Signals, FORCE_STDERR_CAPTURE};
use crate::manifest::Registry;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::path::Path;

/// Scopes forced stderr capture to one redirected native command.
pub(super) struct ForceStderrCapture(bool);
impl ForceStderrCapture {
    pub(super) fn enter(on: bool) -> Self {
        Self(FORCE_STDERR_CAPTURE.with(|flag| flag.replace(on)))
    }
}
impl Drop for ForceStderrCapture {
    fn drop(&mut self) {
        FORCE_STDERR_CAPTURE.with(|flag| flag.set(self.0));
    }
}

/// A `Redirect` step waiting for the command that follows it.
pub(super) struct PendingRedirect {
    pub(super) target: crate::ast::RedirectTarget,
    /// Opened, authorized and re-checked when the redirect step runs, before the
    /// command starts. Writes go through this handle, not a later path lookup.
    pub(super) file: Option<File>,
}

impl PendingRedirect {
    pub(super) fn captures_stderr(&self) -> bool {
        !matches!(self.target, crate::ast::RedirectTarget::StdoutToStderr)
    }

    /// Write the command's stderr through the handle opened before it ran. A
    /// merged or stdout target leaves the file alone.
    pub(super) fn write_stderr(&mut self, stderr: &[u8]) -> Result<()> {
        use std::io::Write as _;
        match (self.target, self.file.as_mut()) {
            (
                crate::ast::RedirectTarget::StderrFile | crate::ast::RedirectTarget::StderrAppend,
                Some(file),
            ) => {
                file.write_all(stderr)
                    .context("could not write redirected stderr")?;
                file.sync_all()
                    .context("could not sync redirected stderr file")
            }
            _ => Ok(()),
        }
    }

    /// Route a completed command's output and return the stdout that remains in
    /// the pipeline. Stdout to stderr leaves nothing for the pipeline to read.
    pub(super) fn route(
        &mut self,
        stdout: Vec<u8>,
        stderr: &[u8],
        signals: &Signals,
    ) -> Result<Vec<u8>> {
        match self.target {
            crate::ast::RedirectTarget::StderrMerged => {
                let mut merged = stdout;
                merged.extend_from_slice(stderr);
                Ok(merged)
            }
            crate::ast::RedirectTarget::StdoutToStderr => {
                write_with_deadline(2, &stdout, &signals.cancelled, signals.deadline)
                    .context("could not write redirected stdout to stderr")?;
                Ok(Vec::new())
            }
            _ => {
                self.write_stderr(stderr)?;
                Ok(stdout)
            }
        }
    }
}

/// Volume, file index and link count of an open Windows file.
#[cfg(windows)]
fn windows_file_info(
    file: &File,
) -> Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    // SAFETY: BY_HANDLE_FILE_INFORMATION is a plain C struct of integers and FILETIME values, for
    // which the all-zero bit pattern is valid. The call below fills it in on success.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle belongs to `file`, which outlives this call, and
    // `info` is a valid, writable structure of the size the API expects.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading file identity");
    }
    Ok(info)
}

/// Check that an opened file is the file the authorized, resolved path names. A link
/// swapped in between the authorization and the open must not change what the grant
/// covered. Unix compares device and inode; Windows has no equivalent check here yet
/// (see the threat model).
pub(super) fn verify_opened_identity(file: &File, resolved: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        let by_path = std::fs::metadata(resolved)
            .with_context(|| format!("checking {}", resolved.display()))?;
        if opened.dev() != by_path.dev() || opened.ino() != by_path.ino() {
            bail!(
                "file changed while it was being opened: {}",
                resolved.display()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, resolved);
    }
    Ok(())
}

/// Authorize and open one output target of a redirect or a detached launch. Every
/// output file takes this path: the write grant and the persistence policy are
/// checked before the hardened open, so no output target has its own rules.
pub(super) fn open_redirect_target(
    destination: &Path,
    append: bool,
    registry: &Registry,
    signals: &Signals,
    cwd: &Path,
) -> Result<File> {
    crate::capabilities::authorize_host_path(
        registry,
        destination,
        crate::capabilities::HostAccess::Write,
        cwd,
    )?;
    crate::security::authorize_persistence_set(
        registry,
        "workflow.redirect",
        crate::security::PersistenceKind::Save,
        &signals.sensitivity_floor(),
    )?;
    open_output_file(destination, append, registry, cwd)
}

/// Open a redirect or append target before any command runs, and keep the handle
/// that later writes use. The final component must not be a link, and the file
/// the handle now refers to must lie inside the write grant. A stderr target is
/// truncated only after that check, so a refused path keeps its old contents.
pub(super) fn open_output_file(
    destination: &Path,
    append: bool,
    registry: &Registry,
    cwd: &Path,
) -> Result<File> {
    // New files are owner-only (0600 on Unix).
    let mut options = crate::platform::private_options();
    options.write(true).create(true);
    if append {
        options.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Refuse a symlink in the final component instead of following it.
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open a reparse point itself, then reject it from the handle below.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(destination)
        .with_context(|| format!("opening {} for output", destination.display()))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!(
                "refusing to write through a link or reparse point: {}",
                destination.display()
            );
        }
    }
    // Re-check the resolved path, so a link in an intermediate directory cannot
    // move the write outside the grant.
    let canonical = std::fs::canonicalize(destination)
        .with_context(|| format!("resolving {}", destination.display()))?;
    crate::capabilities::authorize_host_path(
        registry,
        &canonical,
        crate::capabilities::HostAccess::Write,
        cwd,
    )?;
    // The grant applies to the file that was opened, not to a path looked up
    // again later: its identity must match the authorized path.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        let by_path = std::fs::metadata(&canonical)
            .with_context(|| format!("checking {}", canonical.display()))?;
        if opened.dev() != by_path.dev() || opened.ino() != by_path.ino() {
            bail!(
                "redirect target changed while it was being opened: {}",
                destination.display()
            );
        }
        if opened.nlink() > 1 {
            bail!(
                "refusing to write a hard-linked file: {}",
                destination.display()
            );
        }
    }
    #[cfg(windows)]
    {
        // Parity with the Unix identity check: the opened handle must be the same
        // file as a read-attributes-only open of the authorized path.
        let opened = windows_file_info(&file)?;
        if opened.nNumberOfLinks > 1 {
            bail!(
                "refusing to write a hard-linked file: {}",
                destination.display()
            );
        }
        let probe = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_READ_ATTRIBUTES: u32 = 0x0080;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            std::fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(&canonical)
                .with_context(|| format!("checking {}", canonical.display()))?
        };
        let authorized = windows_file_info(&probe)?;
        if (
            opened.dwVolumeSerialNumber,
            opened.nFileIndexHigh,
            opened.nFileIndexLow,
        ) != (
            authorized.dwVolumeSerialNumber,
            authorized.nFileIndexHigh,
            authorized.nFileIndexLow,
        ) {
            bail!(
                "redirect target changed while it was being opened: {}",
                destination.display()
            );
        }
    }
    if !append {
        file.set_len(0)
            .with_context(|| format!("truncating {}", destination.display()))?;
    }
    Ok(file)
}

/// Append bytes through a handle opened by `open_output_file`, and make them durable.
pub(super) fn append_to_open_file(file: &mut File, destination: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    file.write_all(bytes)
        .with_context(|| format!("appending to {}", destination.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", destination.display()))
}
