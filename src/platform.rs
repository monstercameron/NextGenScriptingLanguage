//! Small OS boundary shared by the runtime, checkpoints and control transport.
use anyhow::Result;
use std::fs::{Metadata, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{atomic::AtomicBool, Arc};

pub(crate) mod sandbox;
#[cfg(unix)]
pub(crate) mod unix_ffi;
#[cfg(windows)]
pub(crate) mod windows;

/// Append native arguments without changing their argv boundaries. Windows' default
/// `Command::args` quoting only quotes spaces and tabs, but some Windows runtimes
/// (notably MSYS) also split on line breaks and other ASCII whitespace.
pub(crate) fn add_arguments<I, S>(command: &mut Command, arguments: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    #[cfg(windows)]
    windows::add_arguments(command, arguments);
    #[cfg(not(windows))]
    command.args(arguments);
}

pub(crate) fn executable_path(path: PathBuf) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        Ok(path)
    }
    #[cfg(windows)]
    {
        let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd") {
            anyhow::bail!("batch files require an explicit cmd.exe invocation; native calls never insert a shell");
        }
        if extension.is_empty() {
            Ok(path.with_extension("exe"))
        } else {
            Ok(path)
        }
    }
}
pub(crate) fn executable(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        metadata.is_file()
    }
}
pub(crate) fn private_options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
/// The Windows atomic-publish contract (rename plus parent-directory flush)
/// lives in [`publish`] so the standalone Windows test harness includes the
/// same source instead of duplicating it. Every platform's [`replace_file`]
/// reports failures through `publish::ReplaceError`.
mod publish;

pub(crate) fn replace_file(
    source: &Path,
    target: &Path,
) -> std::result::Result<(), publish::ReplaceError> {
    #[cfg(unix)]
    {
        std::fs::rename(source, target).map_err(publish::ReplaceError::not_published)
    }
    #[cfg(windows)]
    {
        windows::replace_file(source, target)
    }
    #[cfg(not(any(unix, windows)))]
    {
        // No durability flush exists on other targets: a plain rename with
        // failures reported as not-published (the temporary is still intact).
        std::fs::rename(source, target).map_err(publish::ReplaceError::not_published)
    }
}

/// Signal number of the latest interrupt that cancelled a run; 0 when none.
static CANCEL_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

pub(crate) fn record_cancel_signal(signal: i32) {
    CANCEL_SIGNAL.store(signal, std::sync::atomic::Ordering::SeqCst);
}

/// Exit status for a run cancelled by an interrupt: 128 plus the signal number.
pub(crate) fn cancel_exit_code() -> Option<i32> {
    match CANCEL_SIGNAL.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        signal => Some(128 + signal),
    }
}

/// Interrupt drain states shared by a run and its signal handlers. The first
/// interrupt of a run with an `on interrupt` body requests a drain: no new work is
/// admitted and the program unwinds at the next statement boundary. A second
/// interrupt cancels as before.
pub(crate) const DRAIN_IDLE: u8 = 0;
pub(crate) const DRAIN_REQUESTED: u8 = 1;
pub(crate) const DRAIN_STARTED: u8 = 2;
pub(crate) const DRAIN_CANCELLED: u8 = 3;

/// Records an interrupt against a drain state. True when it requested the drain;
/// false when a drain was already under way and the interrupt must cancel.
pub(crate) fn request_drain(state: &std::sync::atomic::AtomicU8) -> bool {
    state
        .compare_exchange(
            DRAIN_IDLE,
            DRAIN_REQUESTED,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

pub(crate) fn signals(
    cancel: Arc<AtomicBool>,
    drain: Option<Arc<std::sync::atomic::AtomicU8>>,
    wake: Option<Arc<crate::tasks::CancellationWake>>,
) -> Result<Vec<signal_hook::SigId>> {
    #[cfg(unix)]
    {
        let mut ids = Vec::new();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGHUP] {
            let flag = cancel.clone();
            let drain = drain.clone();
            let wake = wake.clone();
            // SAFETY: the action performs atomic operations and a nonblocking
            // one-byte write through the cancellation wake, all signal-safe.
            let registered = unsafe {
                signal_hook::low_level::register(signal, move || {
                    record_cancel_signal(signal);
                    if let Some(drain) = &drain {
                        if request_drain(drain) {
                            return;
                        }
                        drain.store(DRAIN_CANCELLED, std::sync::atomic::Ordering::SeqCst);
                    }
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    if let Some(wake) = &wake {
                        wake.notify_signal();
                    }
                })
            };
            match registered {
                Ok(id) => ids.push(id),
                Err(error) => {
                    for id in ids {
                        signal_hook::low_level::unregister(id);
                    }
                    return Err(error.into());
                }
            }
        }
        Ok(ids)
    }
    #[cfg(windows)]
    {
        let _ = wake;
        windows::signals(cancel, drain)?;
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::publish::*;
    use super::*;
    use std::cell::RefCell;

    /// Records the backend calls behind one publish for ordering assertions.
    struct RecordingBackend {
        posix: std::io::Result<bool>,
        move_fails: bool,
        flush_fails: bool,
        calls: RefCell<Vec<&'static str>>,
    }

    impl RecordingBackend {
        fn with_posix(posix: std::io::Result<bool>) -> Self {
            Self {
                posix,
                move_fails: false,
                flush_fails: false,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl WindowsPublishBackend for RecordingBackend {
        fn posix_rename(&self, _source: &Path, _target: &Path) -> std::io::Result<bool> {
            self.calls.borrow_mut().push("posix_rename");
            match &self.posix {
                Ok(supported) => Ok(*supported),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected rename failure",
                )),
            }
        }

        fn write_through_move(&self, _source: &Path, _target: &Path) -> std::io::Result<()> {
            self.calls.borrow_mut().push("write_through_move");
            if self.move_fails {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected move failure",
                ));
            }
            Ok(())
        }

        fn flush_published(&self, _target: &Path) -> std::io::Result<()> {
            self.calls.borrow_mut().push("flush_published");
            if self.flush_fails {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected flush failure",
                ));
            }
            Ok(())
        }
    }

    fn publish_with(
        backend: &RecordingBackend,
    ) -> (
        std::result::Result<WindowsPublishDurability, ReplaceError>,
        Vec<&'static str>,
    ) {
        let outcome =
            windows_publish_with(backend, Path::new("source.tmp"), Path::new("target.json"));
        (outcome, backend.calls.borrow().clone())
    }

    #[test]
    fn posix_rename_path_flushes_before_returning() {
        let backend = RecordingBackend::with_posix(Ok(true));
        let (outcome, calls) = publish_with(&backend);
        assert_eq!(
            outcome.unwrap(),
            WindowsPublishDurability::PosixRenameFlushed
        );
        assert_eq!(calls, vec!["posix_rename", "flush_published"]);
    }

    #[test]
    fn unsupported_volume_falls_back_to_write_through_move_then_flushes() {
        // The fallback cannot skip the parent flush: non-POSIX volumes need the
        // entry flush as much as the fast path does.
        let backend = RecordingBackend::with_posix(Ok(false));
        let (outcome, calls) = publish_with(&backend);
        assert_eq!(
            outcome.unwrap(),
            WindowsPublishDurability::MoveFileWriteThrough
        );
        assert_eq!(
            calls,
            vec!["posix_rename", "write_through_move", "flush_published"]
        );
    }

    #[test]
    fn posix_error_falls_through_to_write_through_move() {
        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let backend = RecordingBackend::with_posix(Err(denied));
        let (outcome, calls) = publish_with(&backend);
        assert_eq!(
            outcome.unwrap(),
            WindowsPublishDurability::MoveFileWriteThrough
        );
        assert_eq!(
            calls,
            vec!["posix_rename", "write_through_move", "flush_published"]
        );
    }

    #[test]
    fn failed_move_is_not_published() {
        let backend = RecordingBackend {
            posix: Ok(false),
            move_fails: true,
            flush_fails: false,
            calls: RefCell::new(Vec::new()),
        };
        let (outcome, calls) = publish_with(&backend);
        let error = outcome.unwrap_err();
        assert!(!error.published);
        assert_eq!(calls, vec!["posix_rename", "write_through_move"]);
    }

    #[test]
    fn flush_failure_after_posix_rename_is_published_but_unsynced() {
        // The bytes are published; only the entry flush failed. Callers must
        // take the published-but-unsynced path instead of reporting that the
        // publish failed and retrying it.
        let backend = RecordingBackend {
            posix: Ok(true),
            move_fails: false,
            flush_fails: true,
            calls: RefCell::new(Vec::new()),
        };
        let (outcome, calls) = publish_with(&backend);
        let error = outcome.unwrap_err();
        assert!(error.published);
        assert_eq!(calls, vec!["posix_rename", "flush_published"]);
    }

    #[test]
    fn flush_failure_after_fallback_move_is_published_but_unsynced() {
        let backend = RecordingBackend {
            posix: Ok(false),
            move_fails: false,
            flush_fails: true,
            calls: RefCell::new(Vec::new()),
        };
        let (outcome, calls) = publish_with(&backend);
        let error = outcome.unwrap_err();
        assert!(error.published);
        assert_eq!(
            calls,
            vec!["posix_rename", "write_through_move", "flush_published"]
        );
    }
}
