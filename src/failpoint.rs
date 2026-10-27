//! Test-only fault injection for checkpoint durability tests. Configure one failure with
//! `EXECUTE_FAILPOINT=NAME:N:ACTION`, where ACTION is `error`, `abort`, `lost`, or
//! `partial:BYTES`. `lost` models a filesystem that silently drops a write.
//! The older `NAME:N` spelling remains an alias for `error`.

use std::fs::File;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

#[derive(Clone, Copy)]
enum Action {
    Error,
    Abort,
    Lost,
    Partial(usize),
}

struct Plan {
    point: String,
    nth: usize,
    action: Action,
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
        Some(Action::Abort | Action::Partial(_)) => std::process::abort(),
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
        None => file.write_all(bytes),
    }
}
