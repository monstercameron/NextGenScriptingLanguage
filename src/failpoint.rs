//! Test-only fault injection for checkpoint durability tests. Configure one failure with
//! `EXECUTE_FAILPOINT=NAME:N:ACTION`, where ACTION is `error`, `abort`, `lost`,
//! `partial:BYTES`, or `partial-wait:BYTES`. `lost` skips only the selected write call; it
//! does not model a filesystem dropping all data and metadata that have not reached a sync
//! boundary. `partial-wait` is a test harness handshake: after writing the prefix, it signals
//! `<EXECUTE_FAILPOINT_HOLD_DIR>/ready`, waits up to 30 seconds for `release`, then aborts.
//! The older `NAME:N` spelling remains an alias for `error`.

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Action {
    Error,
    Abort,
    Lost,
    Partial(usize),
    PartialWait(usize),
}

struct Plan {
    point: String,
    nth: usize,
    action: Action,
}

/// A writer that can inject an interruption into a streaming atomic write. The
/// selected prefix is flushed through the wrapped writer before the hard abort, so
/// `partial:N` represents bytes written to the file rather than bytes buffered in
/// `BufWriter`.
#[cfg(feature = "failpoints")]
pub(crate) struct Writer<W> {
    point: &'static str,
    inner: W,
    armed: bool,
}

#[cfg(feature = "failpoints")]
impl<W> Writer<W> {
    pub(crate) fn new(point: &'static str, inner: W) -> Self {
        Self {
            point,
            inner,
            armed: true,
        }
    }
}

#[cfg(feature = "failpoints")]
impl<W: Write> Write for Writer<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.armed {
            if let Some(action) = action(self.point) {
                self.armed = false;
                match action {
                    Action::Error => {
                        return Err(io::Error::other(format!(
                            "injected failure at {}",
                            self.point
                        )))
                    }
                    Action::Abort => std::process::abort(),
                    Action::Lost => return Ok(bytes.len()),
                    Action::Partial(count) => {
                        self.inner.write_all(&bytes[..count.min(bytes.len())])?;
                        self.inner.flush()?;
                        std::process::abort()
                    }
                    Action::PartialWait(count) => {
                        self.inner.write_all(&bytes[..count.min(bytes.len())])?;
                        self.inner.flush()?;
                        wait_for_release()?;
                        std::process::abort()
                    }
                }
            }
        }
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

static PLAN: OnceLock<Option<Plan>> = OnceLock::new();
static CALLS: AtomicUsize = AtomicUsize::new(0);

fn plan() -> Option<&'static Plan> {
    PLAN.get_or_init(|| {
        let value = std::env::var("EXECUTE_FAILPOINT").ok()?;
        let mut parts = value.splitn(3, ':');
        let point = parts.next()?;
        let nth = parts.next()?.parse().ok()?;
        let action = match parts.next().unwrap_or("error") {
            "error" => Action::Error,
            "abort" => Action::Abort,
            "lost" => Action::Lost,
            tail if tail.starts_with("partial-wait:") => {
                Action::PartialWait(tail.strip_prefix("partial-wait:")?.parse().ok()?)
            }
            tail if tail.starts_with("partial:") => {
                Action::Partial(tail.strip_prefix("partial:")?.parse().ok()?)
            }
            _ => return None,
        };
        Some(Plan {
            point: point.to_owned(),
            nth,
            action,
        })
    })
    .as_ref()
}

fn action(point: &str) -> Option<Action> {
    let configured = plan()?;
    if configured.point != point {
        return None;
    }
    (CALLS.fetch_add(1, Ordering::SeqCst) + 1 == configured.nth).then_some(configured.action)
}

/// Inject a fault at a non-write boundary. A partial-write action at such a boundary
/// still hard-aborts, because no file bytes are written at that boundary.
pub(crate) fn hit(point: &str) -> io::Result<()> {
    match action(point) {
        Some(Action::Error) => Err(io::Error::other(format!("injected failure at {point}"))),
        Some(Action::Abort | Action::Partial(_) | Action::PartialWait(_)) => std::process::abort(),
        Some(Action::Lost) => Ok(()),
        None => Ok(()),
    }
}

/// Write bytes with optional torn-write injection, then return the caller's normal result.
pub(crate) fn write_all(point: &str, file: &mut File, bytes: &[u8]) -> io::Result<()> {
    match action(point) {
        Some(Action::Error) => Err(io::Error::other(format!("injected failure at {point}"))),
        Some(Action::Abort) => std::process::abort(),
        Some(Action::Lost) => Ok(()),
        Some(Action::Partial(count)) => {
            let _ = file.write_all(&bytes[..count.min(bytes.len())]);
            std::process::abort()
        }
        Some(Action::PartialWait(count)) => {
            file.write_all(&bytes[..count.min(bytes.len())])?;
            wait_for_release()?;
            std::process::abort()
        }
        None => file.write_all(bytes),
    }
}

fn wait_for_release() -> io::Result<()> {
    let hold_dir = std::env::var_os("EXECUTE_FAILPOINT_HOLD_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("partial-wait requires EXECUTE_FAILPOINT_HOLD_DIR"))?;
    std::fs::create_dir_all(&hold_dir)?;
    std::fs::write(hold_dir.join("ready"), b"ready")?;
    let release = hold_dir.join("release");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for failpoint harness release",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
