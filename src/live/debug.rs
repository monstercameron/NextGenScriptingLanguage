//! Breakpoints and statement stepping for a live run. The runtime calls the
//! [`LiveHook`] before each top-level statement; the hook blocks while the run is
//! paused and returns once a `continue` or `step` control releases it.

#[cfg(test)]
use super::state::FloodBudget;
use super::state::{plain, withhold_secret_bindings, Shared};
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
        let bindings = bindings
            .iter()
            .map(|(name, value)| (name.clone(), plain(value.clone())))
            .collect();
        let bindings = withhold_secret_bindings(&bindings, &state.secret_names());
        state.debug.paused = Some(Paused {
            id: state.debug.pauses,
            line,
            bindings,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::state::{Data, Document, Plan, State};
    use std::collections::{BTreeMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::{mpsc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn paused_bindings_withhold_secret_prompt_values() {
        let source = "accept token as text from secret prompt \"Token?\"\n";
        let program = crate::parser::parse(source).unwrap();
        let state = State {
            job_session: std::sync::Arc::new(crate::runtime::live::JobSession::new()),
            redactor: std::sync::Arc::default(),
            registry: Default::default(),
            doc: Document {
                source: source.to_owned(),
                cwd: PathBuf::new(),
                bindings: BTreeMap::from([("token".to_owned(), json!("CANARY-LIVE-TOKEN"))]),
                input: Data::default(),
                cursor: 0,
                inbox: VecDeque::new(),
                user: Default::default(),
                ui: Json::Null,
            },
            exit: None,
            debug: Control {
                breakpoints: BTreeSet::from([1]),
                ..Control::default()
            },
            plan: Plan {
                program,
                stream: None,
                body_start: 0,
                screen: None,
            },
            revision: 0,
            running: true,
            in_flight: false,
            status: "running".into(),
            uncertain: false,
            error: None,
            active: None,
            step_budget: None,
            cycle: false,
            runs: 0,
            nodes: 0,
            launches: 0,
            cache_hits: 0,
            last_output: Data::default(),
            active_cancel: Arc::new(AtomicBool::new(false)),
            producer_cancel: Arc::new(AtomicBool::new(false)),
            producer_done: false,
            producer_error: None,
            producer_epoch: 0,
            events: VecDeque::new(),
            ui_epoch: 0,
            ui_dirty: true,
            ui_frame: String::new(),
            ui_applied_revision: 0,
            ui_errors: Json::Null,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            wake: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
            registry: Default::default(),
            max_bytes: 1024,
            initial_cwd: PathBuf::new(),
            initial_bindings: BTreeMap::new(),
            initial_input: Data::default(),
            headless: true,
            run_mode: false,
            flood: Mutex::new(FloodBudget::new()),
        });
        let (sender, receiver) = mpsc::channel();
        let release = Arc::clone(&shared);
        let observer = thread::spawn(move || loop {
            let mut state = release.state.lock().unwrap();
            if state.debug.paused.is_some() {
                sender.send(state.debug.json()).unwrap();
                state.debug.release = true;
                release.wake.notify_all();
                return;
            }
            drop(state);
            thread::sleep(Duration::from_millis(1));
        });
        let hook = LiveHook {
            shared: Arc::clone(&shared),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        hook.before_statement(
            1,
            &BTreeMap::from([(
                "token".to_owned(),
                Value::String("CANARY-LIVE-TOKEN".to_owned()),
            )]),
        )
        .unwrap();
        observer.join().unwrap();
        let debug = receiver.recv().unwrap();
        assert_eq!(
            debug["paused"]["bindings"]["token"],
            json!({"withheld":"secret"})
        );
        assert!(!debug.to_string().contains("CANARY-LIVE-TOKEN"));
    }
}
