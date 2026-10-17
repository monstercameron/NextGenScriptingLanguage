//! Test-only fault injection for the crash-consistency tests. The module exists only with
//! the `failpoints` feature, so release builds carry no hook and read no environment.
//! `EXECUTE_FAILPOINT=NAME:N` makes the Nth call at the named point fail once with an
//! injected I/O error. Each failpoint call is a checkpoint write: journal, log or blob.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// The configured failure: a point name and the call number that fails.
struct Plan {
    point: String,
    nth: usize,
}

static PLAN: OnceLock<Option<Plan>> = OnceLock::new();
static CALLS: AtomicUsize = AtomicUsize::new(0);

fn plan() -> Option<&'static Plan> {
    PLAN.get_or_init(|| {
        let value = std::env::var("EXECUTE_FAILPOINT").ok()?;
        let (point, nth) = value.rsplit_once(':')?;
        Some(Plan {
            point: point.to_owned(),
            nth: nth.parse().ok()?,
        })
    })
    .as_ref()
}

/// Called at each failure point. It fails only on the configured call of the configured
/// point, so a single run injects a single fault.
pub(crate) fn hit(point: &str) -> io::Result<()> {
    let Some(plan) = plan() else {
        return Ok(());
    };
    if plan.point != point {
        return Ok(());
    }
    let call = CALLS.fetch_add(1, Ordering::SeqCst) + 1;
    if call == plan.nth {
        return Err(io::Error::other(format!(
            "injected failure at {point} call {call}"
        )));
    }
    Ok(())
}
