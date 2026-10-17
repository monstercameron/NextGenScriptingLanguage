//! Small OS boundary shared by the runtime, checkpoints and control transport.
use anyhow::Result;
use std::fs::{Metadata, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, Arc};

pub(crate) mod sandbox;
#[cfg(windows)]
pub(crate) mod windows;

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
) -> Result<Vec<signal_hook::SigId>> {
    #[cfg(unix)]
    {
        let mut ids = Vec::new();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGHUP] {
            let flag = cancel.clone();
            let drain = drain.clone();
            // SAFETY: the action only performs atomic operations, which are
            // async-signal-safe.
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
        windows::signals(cancel, drain)?;
        Ok(Vec::new())
    }
}
