//! Issue #725: a record or replay observer must not reach an unrelated run that
//! overlaps it in the same process. The observed run is the one started on the
//! installing thread; a plain run on another thread must execute its own command,
//! must not be offered to the observer, and must perform its own write step.

use flow::{
    manifest::Registry,
    parser,
    runtime::{
        self,
        observe::{self, Observer, StepRequest, StepResult},
        RunOptions,
    },
    security::SensitivitySet,
};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread,
    time::Duration,
};

/// The observer slot is process-wide, so the two tests take turns installing it.
/// This serialises only the test bodies; the runs under test do not share a lock.
static INSTALL_TURN: Mutex<()> = Mutex::new(());

fn install_turn() -> MutexGuard<'static, ()> {
    INSTALL_TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn options(cwd: &Path) -> RunOptions {
    RunOptions {
        command_timeout: Some(Duration::from_secs(30)),
        ..RunOptions::new(cwd.to_owned())
    }
}

fn recorded_stdout(stdout: &[u8]) -> StepResult {
    StepResult {
        status: Some(0),
        signal: None,
        stages: None,
        failed_stage: None,
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
        outcome: None,
        failed: false,
        sensitivity: SensitivitySet::empty(),
        secret_environment: false,
    }
}

fn mentions(request: &StepRequest, word: &str) -> bool {
    request
        .argv
        .iter()
        .flatten()
        .any(|argument| argument == word)
}

/// Answers every group it is offered with a canned result and suppresses writes.
#[derive(Default)]
struct Replaying {
    offered: Mutex<Vec<StepRequest>>,
}

impl Observer for Replaying {
    fn replay(&self, request: &StepRequest) -> anyhow::Result<Option<StepResult>> {
        self.offered.lock().unwrap().push(request.clone());
        Ok(Some(recorded_stdout(b"replayed")))
    }

    fn finished(
        &self,
        _request: &StepRequest,
        _wall: Duration,
        _result: &StepResult,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn suppresses_writes(&self, _line: usize) -> bool {
        true
    }
}

/// Keeps every group that actually ran, and runs everything for real.
#[derive(Default)]
struct Recording {
    finished: Mutex<Vec<StepRequest>>,
}

impl Observer for Recording {
    fn replay(&self, _request: &StepRequest) -> anyhow::Result<Option<StepResult>> {
        Ok(None)
    }

    fn finished(
        &self,
        request: &StepRequest,
        _wall: Duration,
        _result: &StepResult,
    ) -> anyhow::Result<()> {
        self.finished.lock().unwrap().push(request.clone());
        Ok(())
    }

    fn suppresses_writes(&self, _line: usize) -> bool {
        false
    }
}

#[test]
fn a_replay_observer_does_not_replay_an_unrelated_run_on_another_thread() {
    let _turn = install_turn();
    let cwd = tempfile::tempdir().unwrap();
    let observer = Arc::new(Replaying::default());
    let _guard = observe::install(observer.clone()).unwrap();

    // The plain run overlaps the installed replayer: it starts on another thread
    // and finishes before the observed run starts.
    let plain_cwd = cwd.path().to_owned();
    let plain = thread::spawn(move || {
        let program = parser::parse(
            "run \"printf\" with arguments [\"plain\"]\nthen save to file \"plain.txt\"\n",
        )
        .unwrap();
        runtime::run(&program, &Registry::default(), &options(&plain_cwd)).unwrap()
    })
    .join()
    .unwrap();

    assert_eq!(
        plain.output, b"plain",
        "the plain run must execute its command"
    );
    assert_eq!(
        fs::read(cwd.path().join("plain.txt")).unwrap(),
        b"plain",
        "the plain run's write step must not be suppressed by the replayer"
    );
    {
        let offered = observer.offered.lock().unwrap();
        assert!(
            offered.iter().all(|request| !mentions(request, "plain")),
            "the replayer was offered the plain run's group: {offered:?}"
        );
    }

    // The observed run still sees its own groups through the installed replayer.
    let observed = parser::parse("run \"printf\" with arguments [\"observed\"]\n").unwrap();
    let result = runtime::run(&observed, &Registry::default(), &options(cwd.path())).unwrap();
    assert_eq!(result.output, b"replayed");
}

#[test]
fn a_record_observer_does_not_record_an_unrelated_run_on_another_thread() {
    let _turn = install_turn();
    let cwd = tempfile::tempdir().unwrap();
    let observer = Arc::new(Recording::default());
    let _guard = observe::install(observer.clone()).unwrap();

    let plain_cwd = cwd.path().to_owned();
    let plain = thread::spawn(move || {
        let program = parser::parse("run \"printf\" with arguments [\"plain\"]\n").unwrap();
        runtime::run(&program, &Registry::default(), &options(&plain_cwd)).unwrap()
    })
    .join()
    .unwrap();
    assert_eq!(
        plain.output, b"plain",
        "the plain run must execute its command"
    );

    let observed = parser::parse("run \"printf\" with arguments [\"observed\"]\n").unwrap();
    let result = runtime::run(&observed, &Registry::default(), &options(cwd.path())).unwrap();
    assert_eq!(result.output, b"observed");

    let finished = observer.finished.lock().unwrap();
    assert!(
        finished.iter().all(|request| !mentions(request, "plain")),
        "the recorder kept the plain run's group: {finished:?}"
    );
    assert!(
        finished.iter().any(|request| mentions(request, "observed")),
        "the recorder must keep the observed run's group: {finished:?}"
    );
}
