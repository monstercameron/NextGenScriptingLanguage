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
pub(crate) fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(source, target)
    }
    #[cfg(windows)]
    {
        windows::replace_file(source, target)
    }
}
/// Signal number of the latest interrupt that cancelled a run; 0 when none.
static CANCEL_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

pub(crate) fn record_cancel_signal(signal: i32) {
    CANCEL_SIGNAL.store(signal, std::sync::atomic::Ordering::SeqCst);
}

/// Clear the recorded interrupt signal at the start of a new run. Without this,
/// a later run in the same process would inherit the previous run's `128+N`
/// exit status even when it was never interrupted.
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

/// Serializes tests that touch the process-wide `CANCEL_SIGNAL` so a recorded
/// probe signal cannot leak into a concurrent test's exit-status assertion.
#[cfg(test)]
pub(crate) static CANCEL_SIGNAL_TEST_LOCK: std::sync::Mutex<()> =
    std::sync::Mutex::new(());

#[cfg(test)]
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
        let _serial = CANCEL_SIGNAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    cleanup: Arc<AtomicBool>,
) -> Result<Vec<signal_hook::SigId>> {
    #[cfg(unix)]
    {
        let mut ids = Vec::new();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGHUP] {
            let flag = cancel.clone();
            let drain = drain.clone();
            let wake = wake.clone();
            let cleanup = cleanup.clone();
            // SAFETY: the action performs atomic operations and a nonblocking
            // one-byte write through the cancellation wake, all signal-safe.
            // A repeat interrupt also marks the shared cleanup flag so a
            // `finally`, `on exit` or `on interrupt` body aborts at once
            // instead of running out its cleanup budget.
            let registered = unsafe {
                signal_hook::low_level::register(signal, move || {
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
        windows::signals(cancel, drain, cleanup)?;
        Ok(Vec::new())
    }
}
