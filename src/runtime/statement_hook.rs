//! A per-statement observer for finite runs, used by the live debugger.
//!
//! The statement loop of a top-level run calls [`before_statement`] before each
//! statement it executes, including every iteration of a loop body. With no hook
//! installed on the thread, the call is one relaxed atomic load.

use crate::ast::Value;
use anyhow::Result;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Observes each statement a top-level finite run is about to execute. The hook
/// may block the run, for example to pause at a breakpoint.
pub trait StatementHook: Send + Sync {
    /// Called before the statement at `line` runs, with the bindings it will see.
    /// Returning an error ends the run.
    fn before_statement(&self, line: usize, bindings: &BTreeMap<String, Value>) -> Result<()>;
}

thread_local! {
    static HOOK: RefCell<Option<Arc<dyn StatementHook>>> = const { RefCell::new(None) };
}

/// Installs in progress across all threads. Zero means no statement hook exists,
/// so the statement loop skips the thread-local lookup entirely.
static INSTALLED: AtomicUsize = AtomicUsize::new(0);

/// Run `run` on this thread with `hook` observing its top-level statements. The
/// previous hook, if any, is restored afterwards.
#[cfg(feature = "live")]
pub fn with_statement_hook<T>(hook: Arc<dyn StatementHook>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Arc<dyn StatementHook>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            HOOK.with(|slot| *slot.borrow_mut() = self.0.take());
            INSTALLED.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let previous = HOOK.with(|slot| slot.borrow_mut().replace(hook));
    INSTALLED.fetch_add(1, Ordering::Relaxed);
    let _restore = Restore(previous);
    run()
}

/// Notify this thread's hook, if one is installed, before a statement runs.
pub(crate) fn before_statement(line: usize, bindings: &BTreeMap<String, Value>) -> Result<()> {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return Ok(());
    }
    let hook = HOOK.with(|slot| slot.borrow().clone());
    match hook {
        Some(hook) => hook.before_statement(line, bindings),
        None => Ok(()),
    }
}
