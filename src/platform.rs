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

/// Signal number of the latest interrupt this process saw; 0 when none. This is
/// only a fallback channel for entry points that own no run (the CLI's error
/// reporter and the graph cancellation handler). Per-run truth lives on each
/// `Signals`' own slot, so overlapping runs and runs without signal handlers
/// can neither inherit nor erase each other's exit codes.
static CANCEL_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

pub(crate) fn record_cancel_signal(signal: i32) {
    CANCEL_SIGNAL.store(signal, std::sync::atomic::Ordering::SeqCst);
}

/// Clear the recorded interrupt signal. Only tests use this: production runs
/// read their own slot, and the process-wide fallback keeps the last signal
/// so the CLI entry point can report it after its run is gone.
#[cfg(test)]
pub(crate) fn clear_cancel_signal() {
    CANCEL_SIGNAL.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Exit status for one recorded interrupt signal: 128 plus the signal number
/// (130 for SIGINT, 143 for SIGTERM, 129 for SIGHUP, 131 for SIGQUIT).
pub(crate) fn exit_code_for_signal(signal: i32) -> i32 {
    128 + signal
}

/// Exit status for a run cancelled by an interrupt: 128 plus the signal number.
pub(crate) fn cancel_exit_code() -> Option<i32> {
    match CANCEL_SIGNAL.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        signal => Some(exit_code_for_signal(signal)),
    }
}

/// Serializes tests that touch the process-wide `CANCEL_SIGNAL` fallback so a
/// recorded probe signal cannot leak into a concurrent test's exit-status
/// assertion. Per-run slots need no serialization.
///
/// Invariant: a test that delivers a real OS signal (for example via
/// `libc::raise`) must run that delivery in a child test process re-invoked
/// through `current_exe` with `--exact`, never in the shared test process,
/// because real delivery is process-wide broadcast and would run every
/// concurrently installed handler. Tests that only write the fallback
/// synthetically hold this lock and clear the fallback through a `Drop` guard
/// so a panicking assertion cannot leak `Some(130)` past the lock.
#[cfg(test)]
pub(crate) static CANCEL_SIGNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// Install one signal-handler set for a single run: each handler records into
/// that run's own `signal_code` slot (plus the process-wide CLI fallback).
/// OS delivery is process-wide broadcast -- a signal addressed to this process
/// runs every installed owner's handler set, so overlapping owners each record
/// that same signal while runs without handlers stay clean.
pub(crate) fn signals(
    cancel: Arc<AtomicBool>,
    drain: Option<Arc<std::sync::atomic::AtomicU8>>,
    wake: Option<Arc<crate::tasks::CancellationWake>>,
    cleanup: Arc<AtomicBool>,
    signal_code: Arc<std::sync::atomic::AtomicI32>,
) -> Result<Vec<signal_hook::SigId>> {
    #[cfg(unix)]
    {
        let mut ids = Vec::new();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGHUP] {
            let flag = cancel.clone();
            let drain = drain.clone();
            let wake = wake.clone();
            let cleanup = cleanup.clone();
            let signal_code = signal_code.clone();
            // SAFETY: the action performs atomic operations and a nonblocking
            // one-byte write through the cancellation wake, all signal-safe.
            // A repeat interrupt also marks the shared cleanup flag so a
            // `finally`, `on exit` or `on interrupt` body aborts at once
            // instead of running out its cleanup budget.
            let registered = unsafe {
                signal_hook::low_level::register(signal, move || {
                    // The run's own slot is per-run truth; the process-wide
                    // fallback keeps the CLI entry point reporting after the
                    // run (and its slot) is gone.
                    signal_code.store(signal, std::sync::atomic::Ordering::SeqCst);
                    record_cancel_signal(signal);
                    if let Some(drain) = &drain {
                        if request_drain(drain) {
                            return;
                        }
                        drain.store(DRAIN_CANCELLED, std::sync::atomic::Ordering::SeqCst);
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        cleanup.store(true, std::sync::atomic::Ordering::Relaxed);
                        if let Some(wake) = &wake {
                            wake.notify_signal();
                        }
                        return;
                    }
                    if flag.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        cleanup.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
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
        windows::signals(cancel, drain, cleanup, signal_code)?;
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

mod cancel_signal_tests {
    use super::*;

    #[test]
    fn interrupt_signals_map_to_128_plus_signal() {
        assert_eq!(exit_code_for_signal(2), 130);
        assert_eq!(exit_code_for_signal(15), 143);
        assert_eq!(exit_code_for_signal(1), 129);
        assert_eq!(exit_code_for_signal(3), 131);
    }

    #[test]
    fn clearing_the_recorded_signal_removes_the_sticky_exit_code() {
        let _serial = CANCEL_SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = CANCEL_SIGNAL.load(std::sync::atomic::Ordering::SeqCst);
        record_cancel_signal(15);
        assert_eq!(cancel_exit_code(), Some(143));
        clear_cancel_signal();
        assert_eq!(cancel_exit_code(), None);
        if previous == 0 {
            clear_cancel_signal();
        } else {
            record_cancel_signal(previous);
        }
    }
}
