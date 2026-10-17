//! The working directory of a run. A `UseFolder` step opens the directory once and keeps
//! that handle. The path the directory was authorized under is taken from the handle, and
//! launched children enter the directory through the handle (`fchdir` before `exec`), so a
//! link swapped in after the check cannot move them. On Unix the handle is the authority;
//! on other platforms the directory is a path, as before.
//!
//! The type dereferences to `Path`, so code that only reads the directory is unchanged.

use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(not(target_arch = "wasm32"))]
use crate::manifest::Registry;
#[cfg(not(target_arch = "wasm32"))]
use anyhow::{bail, Result};

#[derive(Debug)]
pub struct WorkingDirectory {
    path: PathBuf,
    #[cfg(unix)]
    handle: Option<std::os::fd::OwnedFd>,
}

impl WorkingDirectory {
    /// A directory named by path only. Children use `current_dir` with this path, which is
    /// the behaviour before handles existed. Used for fixed directories such as a listener's
    /// or a worker's.
    pub fn plain(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            #[cfg(unix)]
            handle: None,
        }
    }

    /// Open a directory and keep the handle. The path is recorded as given; callers that
    /// authorize the directory use `resolved_path` and check the identity.
    pub fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(path)?;
            Ok(Self {
                path: path.to_path_buf(),
                handle: Some(file.into()),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                path: path.to_path_buf(),
            })
        }
    }

    /// The path of the opened directory as the operating system reports it. Authorization
    /// must use this path, not the path that was opened, so that the check covers the
    /// directory the handle refers to.
    pub fn resolved_path(&self) -> io::Result<PathBuf> {
        #[cfg(unix)]
        {
            if let Some(handle) = &self.handle {
                use std::os::fd::AsRawFd;
                if let Ok(path) = path_of_descriptor(handle.as_raw_fd()) {
                    return Ok(path);
                }
            }
        }
        std::fs::canonicalize(&self.path)
    }

    /// Whether the handle and the given path name the same directory. Only Unix can check
    /// this; elsewhere the answer is `true` and the check is not performed.
    pub fn same_directory_as(&self, path: &Path) -> io::Result<bool> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Some(handle) = &self.handle {
                let opened = std::fs::File::from(handle.try_clone()?).metadata()?;
                let by_path = std::fs::metadata(path)?;
                return Ok(opened.dev() == by_path.dev() && opened.ino() == by_path.ino());
            }
        }
        let _ = path;
        Ok(true)
    }

    /// Record the path the directory was authorized under. Call it only after the
    /// resolved path has been authorized and the handle checked against it, so that
    /// relative paths joined onto `path()` name the authorized directory.
    pub fn authorized(mut self, resolved: PathBuf) -> Self {
        self.path = resolved;
        self
    }

    /// The directory's path, for diagnostics and for code that joins relative paths.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Make a launched child start in this directory. With a handle, the child enters the
    /// directory through the handle before `exec`; without one, the path is used.
    pub fn apply(&self, command: &mut Command) {
        #[cfg(unix)]
        {
            if let Some(handle) = &self.handle {
                use std::os::fd::AsRawFd;
                use std::os::unix::process::CommandExt;
                let fd = handle.as_raw_fd();
                // SAFETY: fchdir is async-signal-safe and is the only call in the hook. The
                // descriptor is owned by `self`, which outlives the spawn it is used for.
                unsafe {
                    command.pre_exec(move || {
                        if libc::fchdir(fd) == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                return;
            }
        }
        command.current_dir(&self.path);
    }

    /// A second handle to the same directory, for a child that outlives this value.
    pub fn duplicate(&self) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let handle = match &self.handle {
                Some(handle) => Some(handle.try_clone()?),
                None => None,
            };
            Ok(Self {
                path: self.path.clone(),
                handle,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                path: self.path.clone(),
            })
        }
    }
}

impl Deref for WorkingDirectory {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

#[cfg(target_os = "linux")]
fn path_of_descriptor(fd: std::os::fd::RawFd) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
}

#[cfg(target_os = "macos")]
fn path_of_descriptor(fd: std::os::fd::RawFd) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = [0u8; 1024];
    // SAFETY: F_GETPATH writes at most MAXPATHLEN (1024) bytes into the buffer.
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buffer.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..end])))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn path_of_descriptor(_fd: std::os::fd::RawFd) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform cannot name an open directory",
    ))
}

/// Open a directory for the run and authorize the path it resolves to. The returned
/// handle records that resolved path, so relative paths joined onto it name the
/// authorized directory even if the requested path is swapped later.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn enter_directory(
    registry: &Registry,
    requested: &Path,
    cwd: &Path,
) -> Result<crate::working_dir::WorkingDirectory> {
    let opened = crate::working_dir::WorkingDirectory::open(requested)?;
    let resolved = opened.resolved_path()?;
    crate::capabilities::authorize_host_path(registry, &resolved, false, cwd)?;
    if !opened.same_directory_as(&resolved)? {
        bail!(
            "folder changed while it was being opened: {}",
            resolved.display()
        );
    }
    Ok(opened.authorized(resolved))
}
