//! Observation of finite runs for run records and replay.
//!
//! An observer sees each applet group a finite run executes, in order, with its
//! captured stdout, stderr and status. A recorder keeps those results. A replayer
//! returns recorded results instead of running the group, and suppresses file
//! writes. Observers are process-global so worker threads share one view, and
//! nothing is observed unless one is installed with [`install`].

use super::*;
use crate::agent::what_if_key::{hmac_sha256, load_existing, load_or_create, KEY_BYTES};
use crate::contracts::Outcome;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

/// An applet group about to run, or one that has just finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRequest {
    /// Source line of the group's first call.
    pub line: usize,
    /// Manifest name of each call in pipeline order.
    pub applets: Vec<String>,
    /// Executable name of each call, from its manifest.
    pub binaries: Vec<String>,
    /// Resolved argument vector of each call, when it can be computed.
    pub argv: Vec<Vec<String>>,
    /// Keyed digest of the group's input, as `<key check>:<digest>` in hex. Only
    /// the digest is kept, never the input. `None` when the input is not held in
    /// memory (stdin or a file handle) or no per-user key is available.
    pub input_digest: Option<String>,
}

/// Which per-user key, if any, keys the input digest of each group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKeyPolicy {
    /// No digest is computed.
    Unkeyed,
    /// Recording: create the per-user key when it is missing.
    CreateIfMissing,
    /// Replay: use the existing key only, so replay never creates one.
    Existing,
}

/// Domain separators for the input digest, so a key check and a digest never share
/// a message.
const KEY_CHECK_DOMAIN: &[u8] = b"execute input key check v1";
const INPUT_DIGEST_DOMAIN: &[u8] = b"execute group input v1\0";

static CREATED_KEY: OnceLock<Option<[u8; KEY_BYTES]>> = OnceLock::new();
static EXISTING_KEY: OnceLock<Option<[u8; KEY_BYTES]>> = OnceLock::new();

/// The per-user key for a policy. A key that cannot be read or created is `None`,
/// so the digest is omitted rather than failing the group.
fn input_key(policy: InputKeyPolicy) -> Option<[u8; KEY_BYTES]> {
    match policy {
        InputKeyPolicy::Unkeyed => None,
        InputKeyPolicy::CreateIfMissing => *CREATED_KEY.get_or_init(|| load_or_create().ok()),
        InputKeyPolicy::Existing => *EXISTING_KEY.get_or_init(|| load_existing().ok()),
    }
}

/// The keyed digest of a group's input, or `None` when the input is not in memory.
fn input_digest(key: &[u8; KEY_BYTES], input: &Input) -> Option<String> {
    let bytes: &[u8] = match input {
        Input::Empty => &[],
        Input::Bytes(bytes) => bytes,
        Input::File(_) | Input::Stdin => return None,
    };
    let check = hmac_sha256(key, KEY_CHECK_DOMAIN);
    let mut message = INPUT_DIGEST_DOMAIN.to_vec();
    message.extend_from_slice(bytes);
    let digest = hmac_sha256(key, &message);
    Some(format!("{}:{}", hex_text(&check[..8]), hex_text(&digest)))
}

fn hex_text(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Captured result of one applet group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The declared outcome of a single-call group, when the status is known.
    pub outcome: Option<Outcome>,
    /// Whether the group ended as a typed failure (`CommandFailed`), which stops
    /// the program unless it is caught.
    pub failed: bool,
    /// Sensitivity labels the runtime tracks for the workflow when the group ran.
    /// Persisting this output is subject to the data-flow persistence policy.
    pub sensitivity: crate::security::SensitivitySet,
    /// Whether an applet in the group receives declared secret environment, whose
    /// output is never persisted.
    pub secret_environment: bool,
}

pub trait Observer: Send + Sync {
    /// Return recorded results to use in place of running this group. `None` runs it.
    fn replay(&self, request: &StepRequest) -> Result<Option<StepResult>>;

    /// Called once a group has run for real, with its wall time.
    fn finished(&self, request: &StepRequest, wall: Duration, result: &StepResult) -> Result<()>;

    /// Whether file writes (`save`, `append`, redirections) at this line are skipped.
    fn suppresses_writes(&self, line: usize) -> bool;

    /// Whether streamed stdout must be captured so the observer sees it. The bytes
    /// are still written to the terminal after the group completes.
    fn captures_streamed_stdout(&self) -> bool {
        true
    }

    /// Whether streamed output should be written to the process stdout. An
    /// observer may instead capture it for a framed protocol response.
    fn writes_streamed_stdout(&self) -> bool {
        true
    }

    /// Whether native child streams that would normally be inherited by the
    /// caller must be captured instead. Framed protocols use this to keep raw
    /// child bytes from escaping their response channel.
    fn captures_inherited_stdio(&self) -> bool {
        false
    }

    /// Bytes the run wrote to the terminal as streamed output, in order.
    fn terminal_output(&self, _bytes: &[u8]) {}

    /// The final implicit program output and the line whose executed statement
    /// made it eligible. Replayers can associate it with the current block.
    fn program_output(&self, _bytes: &[u8], _line: Option<usize>) {}

    /// Whether streamed output of this line is withheld from the terminal, because
    /// an earlier run already showed it.
    fn suppresses_terminal_output(&self, _line: usize) -> bool {
        false
    }

    /// Which per-user key keys the input digest of each group. Observers that do
    /// not compare input leave this unkeyed.
    fn input_key_policy(&self) -> InputKeyPolicy {
        InputKeyPolicy::Unkeyed
    }
}

/// Whether streamed output at this line must not be written again.
pub(super) fn suppresses_terminal_output(line: usize) -> bool {
    current().is_some_and(|observer| observer.suppresses_terminal_output(line))
}

/// Report bytes the run is writing to the terminal as streamed output.
pub(super) fn note_terminal_output(bytes: &[u8]) {
    if let Some(observer) = current() {
        observer.terminal_output(bytes);
    }
}

/// Report the final implicit program output to an active observer.
pub(super) fn note_program_output(bytes: &[u8], line: Option<usize>) {
    if let Some(observer) = current() {
        observer.program_output(bytes, line);
    }
}

/// Whether streamed output should reach the process stdout.
pub(super) fn writes_streamed_stdout() -> bool {
    current().is_none_or(|observer| observer.writes_streamed_stdout())
}

/// Whether an observer requires inherited native streams to be captured.
pub(super) fn captures_inherited_stdio() -> bool {
    current().is_some_and(|observer| observer.captures_inherited_stdio())
}

static OBSERVER: Mutex<Option<Arc<dyn Observer>>> = Mutex::new(None);

/// Keeps an observer installed. Dropping it removes the observer.
pub struct ObserverGuard;

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        *OBSERVER.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Install the process-wide observer. Only one may be active at a time.
pub fn install(observer: Arc<dyn Observer>) -> Result<ObserverGuard> {
    let mut slot = OBSERVER.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_some() {
        bail!("another run observer is already installed");
    }
    *slot = Some(observer);
    Ok(ObserverGuard)
}

/// Whether a run observer (recording or replay) is installed.
pub(super) fn is_installed() -> bool {
    OBSERVER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
}

fn current() -> Option<Arc<dyn Observer>> {
    OBSERVER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Whether file writes for this step must be skipped. Used by the write steps.
pub(super) fn suppresses_writes(line: usize) -> bool {
    current().is_some_and(|observer| observer.suppresses_writes(line))
}

/// Whether a streamed command must have its stdout captured.
pub(super) fn captures_streamed_stdout() -> bool {
    current().is_some_and(|observer| observer.captures_streamed_stdout())
}

fn request_for(
    calls: &[(&Invocation, &Manifest, usize)],
    bindings: &BTreeMap<String, Value>,
    input_digest: Option<String>,
) -> StepRequest {
    StepRequest {
        input_digest,
        line: calls.first().map_or(0, |(_, _, line)| *line),
        applets: calls
            .iter()
            .map(|(call, manifest, _)| manifest.name_for(call).to_owned())
            .collect(),
        binaries: calls
            .iter()
            .map(|(call, manifest, _)| manifest.binary_for(call).to_owned())
            .collect(),
        argv: calls
            .iter()
            .map(|(call, manifest, _)| {
                super::arguments_for_call(call, manifest, bindings).unwrap_or_default()
            })
            .collect(),
    }
}

/// Run one applet group, or substitute its recorded result when an observer is
/// installed. Every applet group of a finite run reaches this function.
pub(super) fn execute_call_group(group: GroupRequest<'_>) -> Result<GroupOutput> {
    let GroupRequest {
        registry,
        calls,
        input,
        prepared_first_call,
        bindings,
        cwd,
        options,
        launched,
        signals,
    } = group;
    let Some(observer) = current() else {
        return super::execute_call_group_unobserved(GroupRequest {
            registry,
            calls,
            input,
            prepared_first_call,
            bindings,
            cwd,
            options,
            launched,
            signals,
        });
    };
    let input_digest =
        input_key(observer.input_key_policy()).and_then(|key| input_digest(&key, &input));
    let request = request_for(calls, bindings, input_digest);
    if let Some(recorded) = observer.replay(&request)? {
        return replayed_output(calls, recorded);
    }
    let started = std::time::Instant::now();
    let output = match super::execute_call_group_unobserved(GroupRequest {
        registry,
        calls,
        input,
        prepared_first_call,
        bindings,
        cwd,
        options,
        launched,
        signals,
    }) {
        Ok(output) => output,
        Err(error) => {
            // A nonzero status is reported as a typed failure. The failed step is the
            // one a record most needs, so it is recorded before the error propagates.
            if let Some(failure) = error.downcast_ref::<super::CommandFailed>() {
                let result = StepResult {
                    status: failure.status.code(),
                    stdout: failure.stdout.clone().unwrap_or_default(),
                    stderr: failure
                        .stderr
                        .clone()
                        .unwrap_or_else(|| failure.diagnostic.clone().into_bytes()),
                    outcome: failure.outcome.clone(),
                    failed: true,
                    sensitivity: signals.sensitivity_floor(),
                    secret_environment: secret_environment(registry, calls),
                };
                observer.finished(&request, started.elapsed(), &result)?;
            }
            return Err(error);
        }
    };
    // The declared outcome is defined for a single call and a known status.
    let outcome = match (calls, output.status) {
        ([(_, manifest, _)], Some(status)) => Some(manifest.outcome_for(status)),
        _ => None,
    };
    let result = StepResult {
        status: output.status,
        stdout: output.bytes.clone(),
        stderr: output.diagnostic.clone(),
        outcome,
        failed: false,
        sensitivity: signals.sensitivity_floor(),
        secret_environment: secret_environment(registry, calls),
    };
    observer.finished(&request, started.elapsed(), &result)?;
    Ok(output)
}

/// Whether any applet in the group declares secret environment.
fn secret_environment(registry: &Registry, calls: &[(&Invocation, &Manifest, usize)]) -> bool {
    calls.iter().any(|(call, manifest, _)| {
        crate::security::applet_has_declared_secret_environment(registry, manifest.name_for(call))
    })
}

/// Build the group output for a recorded result. A recorded failure is returned as
/// the same typed error the live run produced, so the program's control flow
/// matches. Stage statuses for earlier stages of a pipeline are not recorded, so
/// they are reported as success.
fn replayed_output(
    calls: &[(&Invocation, &Manifest, usize)],
    recorded: StepResult,
) -> Result<GroupOutput> {
    let Some(status) = recorded.status else {
        bail!("recorded step has no exit status and cannot be replayed");
    };
    let last = calls.len().saturating_sub(1);
    let stages: Vec<execution_result::StageResult> = calls
        .iter()
        .enumerate()
        .map(|(index, (call, manifest, line))| {
            let stage_status = if index == last { status } else { 0 };
            let outcome = recorded
                .outcome
                .clone()
                .filter(|_| index == last)
                .unwrap_or_else(|| manifest.outcome_for(stage_status));
            let stderr = (index == last).then(|| recorded.stderr.clone());
            execution_result::StageResult::exited(
                *line,
                manifest.name_for(call),
                stage_status,
                outcome,
                stderr,
            )
        })
        .collect();
    if recorded.failed {
        let (line, operation) = calls
            .last()
            .map(|(call, manifest, line)| (*line, manifest.name_for(call).to_owned()))
            .unwrap_or((0, String::new()));
        return Err(CommandFailed {
            line,
            operation,
            status: exit_status(status),
            outcome: recorded.outcome.clone(),
            diagnostic: String::from_utf8_lossy(&recorded.stderr).into_owned(),
            stderr: Some(recorded.stderr),
            stdout: Some(recorded.stdout),
            stages,
        }
        .into());
    }
    Ok(GroupOutput {
        bytes: recorded.stdout,
        typed_json: None,
        status: Some(status),
        diagnostic: recorded.stderr,
        stdout_captured: true,
        stages,
    })
}

/// A process exit status with the given code, for a replayed failure.
fn exit_status(code: i32) -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw((code & 0xff) << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_input_changes_the_digest_and_unchanged_input_does_not() {
        let key = [7u8; KEY_BYTES];
        let old = input_digest(&key, &Input::Bytes(b"old".to_vec()));
        let new = input_digest(&key, &Input::Bytes(b"new".to_vec()));
        let again = input_digest(&key, &Input::Bytes(b"old".to_vec()));
        assert!(old.is_some());
        assert_ne!(old, new);
        assert_eq!(old, again);
    }

    #[test]
    fn digests_hide_the_input_and_depend_on_the_key() {
        let key = [7u8; KEY_BYTES];
        let other_key = [8u8; KEY_BYTES];
        let digest = input_digest(&key, &Input::Bytes(b"secret-value".to_vec())).unwrap();
        assert!(!digest.contains("secret"));
        assert_ne!(
            input_digest(&other_key, &Input::Bytes(b"secret-value".to_vec())),
            Some(digest)
        );
    }

    #[test]
    fn input_not_held_in_memory_has_no_digest() {
        let key = [7u8; KEY_BYTES];
        assert_eq!(input_digest(&key, &Input::Stdin), None);
        assert!(input_digest(&key, &Input::Empty).is_some());
    }
}
