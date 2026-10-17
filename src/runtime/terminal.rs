//! Terminal handoff for `interactively`. The `Interactive` marker requests the
//! handoff for the next native command. That command inherits all three stdio
//! streams, and on Unix with a terminal on stdin it becomes the terminal's
//! foreground process group for its run.

use std::cell::Cell;

thread_local! {
    /// Set while the command being started takes the terminal.
    static REQUESTED: Cell<bool> = const { Cell::new(false) };
}

/// Holds the handoff request for one `Invoke` step and restores the previous
/// request when the step ends, on success or error.
pub(super) struct Scope {
    previous: bool,
}

impl Scope {
    pub(super) fn enter(requested: bool) -> Self {
        Self {
            previous: REQUESTED.with(|flag| flag.replace(requested)),
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        REQUESTED.with(|flag| flag.set(self.previous));
    }
}

/// Whether the native process about to start takes the terminal.
pub(super) fn requested() -> bool {
    REQUESTED.with(|flag| flag.get())
}
