//! Small OS boundary shared by the runtime, checkpoints and control transport.
use anyhow::Result;
use std::fs::{Metadata, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

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
/// Signal number of the latest interrupt this process saw; 0 when none. This is
/// only a fallback channel for entry points that own no run (the CLI's error
/// reporter and the graph cancellation handler). Per-run truth lives on each
/// `Signals`' own slot, so overlapping runs and runs without signal handlers
/// can neither inherit nor erase each other's exit codes.
static CANCEL_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

pub(crate) fn record_cancel_signal(signal: i32) {
    CANCEL_SIGNAL.store(signal, std::sync::atomic::Ordering::SeqCst);
}

/// Clear the recorded interrupt at the start of a fresh run. The signal number
/// is process-global, so without this a SIGINT that cancelled one in-process
/// run (tests, daemon, library reuse) would still be visible to the next run's
/// CLI fallback report (see issue #525). Escalation never reads this fallback:
/// per-run truth lives on each `Signals`' own slot, and the process-wide
/// fallback only keeps the last signal so the CLI entry point can report it
/// after its run is gone. Tests use this too, serialized through
/// `CANCEL_SIGNAL_TEST_LOCK`.
pub(crate) fn clear_cancel_signal() {
    CANCEL_SIGNAL.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Live signal-owning runs in this process. A run becomes an owner when its
/// `platform::signals` handler set is installed and stops being one when its
/// `Signals` is dropped. Runs without handlers (nested, service, job, worker)
/// never own. Used only to guard the process-wide fallback: a second
/// overlapping register must not erase the first run's pending CLI report
/// (issue #525, B1), while a sequential run must still drop stale state.
static SIGNAL_OWNERS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn live_signal_owners() -> usize {
    SIGNAL_OWNERS.load(Ordering::SeqCst)
}

pub(crate) fn note_signal_owner_installed() {
    SIGNAL_OWNERS.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn note_signal_owner_released() {
    // Defensive saturation: `Drop` pairs with a successful install, but a
    // duplicated or stray release must never underflow into a huge count that
    // would pin the fallback uncleared forever.
    let mut current = SIGNAL_OWNERS.load(Ordering::SeqCst);
    loop {
        if current == 0 {
            return;
        }
        match SIGNAL_OWNERS.compare_exchange_weak(
            current,
            current.saturating_sub(1),
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

/// Clear the fallback for a new signal-owning run, but only when no other
/// owner is live. Returns the cleared value when this call cleared, so a
/// failed install can restore it (B2). A live owner keeps its pending CLI
/// report; a sequential run drops the previous run's stale signal.
pub(crate) fn clear_for_new_owner() -> Option<i32> {
    let previous = CANCEL_SIGNAL.load(Ordering::SeqCst);
    if previous == 0 {
        return None;
    }
    if live_signal_owners() != 0 {
        return None;
    }
    clear_cancel_signal();
    Some(previous)
}

/// Restore the fallback after a failed handler install, and only then: when
/// the fallback is still cleared, so a concurrent handler's fresh record is
/// never overwritten with stale state (B2).
pub(crate) fn restore_after_failed_install(cleared: Option<i32>) {
    if let Some(previous) = cleared {
        let _ = CANCEL_SIGNAL.compare_exchange(0, previous, Ordering::SeqCst, Ordering::SeqCst);
    }
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

    #[test]
    fn new_owner_clear_skips_a_live_owner_and_clears_sequential_stale() {
        // B1: an overlapping register must not erase the live run's pending
        // CLI fallback report; a sequential run must still drop stale state.
        // Deterministic: no real signal delivery, only the fallback word and
        // the owner count, serialized through the fallback lock.
        let _serial = CANCEL_SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous_fallback = CANCEL_SIGNAL.load(std::sync::atomic::Ordering::SeqCst);
        let previous_owners = SIGNAL_OWNERS.load(Ordering::SeqCst);
        // Start from a known state: no live owners in this serialized test.
        SIGNAL_OWNERS.store(0, Ordering::SeqCst);
        CANCEL_SIGNAL.store(0, Ordering::SeqCst);
        // Sequential stale: a previous run's SIGTERM is dropped for the next run.
        record_cancel_signal(15);
        let cleared = clear_for_new_owner();
        assert_eq!(cleared, Some(15));
        assert_eq!(cancel_exit_code(), None);
        // Live owner: the first run records, the second register skips clearing.
        record_cancel_signal(2);
        note_signal_owner_installed();
        assert_eq!(live_signal_owners(), 1);
        let skipped = clear_for_new_owner();
        assert_eq!(skipped, None);
        assert_eq!(cancel_exit_code(), Some(130));
        note_signal_owner_released();
        assert_eq!(live_signal_owners(), 0);
        // After the owner drains, the next sequential run clears again.
        let cleared_again = clear_for_new_owner();
        assert_eq!(cleared_again, Some(2));
        assert_eq!(cancel_exit_code(), None);
        // A stray release never underflows into a huge pinned count.
        note_signal_owner_released();
        assert_eq!(live_signal_owners(), 0);
        // Restore entry state so a panicking assertion cannot leak fallback
        // or owner state past the lock.
        SIGNAL_OWNERS.store(previous_owners, Ordering::SeqCst);
        if previous_fallback == 0 {
            clear_cancel_signal();
        } else {
            record_cancel_signal(previous_fallback);
        }
    }

    #[test]
    fn failed_install_restores_only_a_cleared_fallback() {
        // B2: a failed handler install after a clear must not destroy the
        // fallback the run never started with; a fresh concurrent record must
        // never be overwritten with stale state. Deterministic helper-level
        // proof: no real `signal_hook` failure is injected.
        let _serial = CANCEL_SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous_fallback = CANCEL_SIGNAL.load(std::sync::atomic::Ordering::SeqCst);
        let previous_owners = SIGNAL_OWNERS.load(Ordering::SeqCst);
        SIGNAL_OWNERS.store(0, Ordering::SeqCst);
        CANCEL_SIGNAL.store(0, Ordering::SeqCst);
        // Cleared then failed: the stale value comes back.
        record_cancel_signal(15);
        let cleared = clear_for_new_owner();
        assert_eq!(cleared, Some(15));
        assert_eq!(cancel_exit_code(), None);
        restore_after_failed_install(cleared);
        assert_eq!(cancel_exit_code(), Some(143));
        // Cleared but a concurrent record landed first: keep the fresh one.
        let cleared_again = clear_for_new_owner();
        assert_eq!(cleared_again, Some(15));
        record_cancel_signal(2);
        restore_after_failed_install(cleared_again);
        assert_eq!(cancel_exit_code(), Some(130));
        // Skipped clear (live owner): restore is a no-op.
        note_signal_owner_installed();
        record_cancel_signal(15);
        restore_after_failed_install(None);
        assert_eq!(cancel_exit_code(), Some(143));
        note_signal_owner_released();
        SIGNAL_OWNERS.store(previous_owners, Ordering::SeqCst);
        if previous_fallback == 0 {
            clear_cancel_signal();
        } else {
            record_cancel_signal(previous_fallback);
        }
    }
}
