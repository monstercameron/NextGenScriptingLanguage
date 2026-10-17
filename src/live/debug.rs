//! Breakpoints and statement stepping for a live run. The runtime calls the
//! [`LiveHook`] before each top-level statement; the hook blocks while the run is
//! paused and returns once a `continue` or `step` control releases it.

use super::state::{plain, Shared};
use crate::ast::Value;
use crate::runtime::statement_hook::StatementHook;
use anyhow::{bail, Result};
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Debugger state kept in the live state, under the same lock as the run.
#[derive(Default)]
pub(crate) struct Control {
    /// Source lines to pause before.
    pub breakpoints: BTreeSet<usize>,
    /// Pause again before the next statement (set by `step`).
    pub stepping: bool,
    /// Set by `continue` or `step` to let a paused statement go on.
    pub release: bool,
    /// The statement the run is paused before.
    pub paused: Option<Paused>,
    /// Pauses so far. Each pause gets a new id, so a client can tell a new stop from a
    /// revision that changed while the same pause was still in place.
    pub pauses: u64,
}

#[derive(Clone)]
pub(crate) struct Paused {
    pub id: u64,
    pub line: usize,
    /// The bindings the statement will see, as JSON.
    pub bindings: BTreeMap<String, Json>,
}

impl Control {
    /// The `/runtime` fields that describe breakpoints.
    pub fn json(&self) -> Json {
        json!({
            "breakpoints": self.breakpoints.iter().collect::<Vec<_>>(),
            "paused": self.paused.as_ref().map(|paused| json!({
                "id": paused.id,
                "line": paused.line,
                "bindings": paused.bindings,
            })),
        })
    }
}

/// Parse the lines of a `breakpoint` control: a JSON array of positive integers.
/// An empty array clears every breakpoint.
pub(crate) fn lines(value: Option<&Json>) -> Result<BTreeSet<usize>> {
    let Some(Json::Array(items)) = value else {
        bail!("breakpoint requires an array of line numbers");
    };
    items
        .iter()
        .map(|item| {
            item.as_u64()
                .filter(|line| *line >= 1)
                .and_then(|line| usize::try_from(line).ok())
                .ok_or_else(|| anyhow::anyhow!("breakpoint lines must be positive integers"))
        })
        .collect()
}

/// Parse the comma-separated lines of `serve --breakpoints` (`run --break-at`).
pub(crate) fn lines_text(text: Option<&str>) -> Result<BTreeSet<usize>> {
    let Some(text) = text else {
        return Ok(BTreeSet::new());
    };
    text.split(',')
        .map(|line| {
            line.trim()
                .parse::<usize>()
                .ok()
                .filter(|line| *line >= 1)
                .ok_or_else(|| {
                    anyhow::anyhow!("breakpoint lines must be positive integers, got {line:?}")
                })
        })
        .collect()
}

/// The hook a live run installs on its worker thread.
pub(crate) struct LiveHook {
    pub shared: Arc<Shared>,
    /// The node's cancel flag, set by `cancel` and `stop`.
    pub cancel: Arc<AtomicBool>,
}

impl StatementHook for LiveHook {
    fn before_statement(&self, line: usize, bindings: &BTreeMap<String, Value>) -> Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        if !state.debug.breakpoints.contains(&line) && !state.debug.stepping {
            return Ok(());
        }
        state.debug.stepping = false;
        state.debug.release = false;
        state.debug.pauses += 1;
        state.debug.paused = Some(Paused {
            id: state.debug.pauses,
            line,
            bindings: bindings
                .iter()
                .map(|(name, value)| (name.clone(), plain(value.clone())))
                .collect(),
        });
        state.status = "breakpoint".into();
        state.changed("breakpoint");
        self.shared.wake.notify_all();
        loop {
            if state.debug.release {
                break;
            }
            if self.shared.stop.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed) {
                state.debug.paused = None;
                bail!("run cancelled at a breakpoint");
            }
            state = self
                .shared
                .wake
                .wait_timeout(state, Duration::from_millis(20))
                .unwrap()
                .0;
        }
        state.debug.release = false;
        state.debug.paused = None;
        state.status = "running".into();
        state.changed("breakpoint_released");
        self.shared.wake.notify_all();
        Ok(())
    }
}
