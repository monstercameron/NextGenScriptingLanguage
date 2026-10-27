//! Process handoff (`hand over to`) and detached launch (`launch detached`).
//!
//! Both take the `Command` that the capability command builder produces for the
//! native program, so the environment is cleared and rebuilt from the grant and
//! any required OS sandbox wraps the process exactly as for other native commands.
//!
//! A hand-over replaces the script with one program. On Unix this is `execve`
//! after flushing our output; the process keeps its pid and exit status. Windows
//! has no in-place replacement, so the child runs in a job object with
//! `KILL_ON_JOB_CLOSE` and inherited stdio, is waited on together with the
//! run's cancellation flag and workflow deadline, and its exit code becomes
//! the script's exit code.
//!
//! A detached launch starts a program that the script does not own. The child
//! gets its own session on Unix and `DETACHED_PROCESS` on Windows, and is never
//! assigned to a job object, so cancellation and exit of this script do not reach it.

use crate::ast::{Connection, Program, Step, StepKind, Value};
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Write;
use std::process::{Command, Stdio};

/// Detached launches allowed in one run. Each one is an owned child until it exits,
/// so the cap bounds the waiter threads and process-table entries a loop can create.
pub(crate) const MAX_DETACHED_LAUNCHES: usize = 64;

/// The process-level request a marker step left for the next native command.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ProcessRequest {
    HandOver,
    /// Detached launch with the optional output target, resolved like a redirect.
    Detach(Option<Value>),
}

/// Static rules for process handoff, checked before the program has any effect.
/// The parser already limits each marker to one fresh native command; this adds
/// the mode and placement rules that need the whole program.
pub(crate) fn validate(program: &Program, checkpointed: bool) -> Result<()> {
    // Temporary scopes hold paths that a checkpoint cannot replay, so they are refused
    // under one, as hand-over and detached launch are.
    if checkpointed {
        if let Some(step) = program
            .steps
            .iter()
            .find(|step| matches!(step.kind, StepKind::TempScope { .. }))
        {
            bail!(
                "line {}: temporary scopes are not supported under --checkpoint",
                step.line
            );
        }
    }
    for (index, step) in program.steps.iter().enumerate() {
        let verb = match step.kind {
            StepKind::HandOver => "hand over to",
            StepKind::LaunchDetached { .. } => "launch detached",
            _ => continue,
        };
        if checkpointed {
            bail!(
                "line {}: `{verb}` is not supported under --checkpoint",
                step.line
            );
        }
        if program.trigger.is_some() {
            bail!(
                "line {}: `{verb}` is not supported in triggered, service, or listener programs",
                step.line
            );
        }
        match step.kind {
            // The command replaces the script, so nothing may follow it.
            StepKind::HandOver => {
                if index + 2 != program.steps.len() {
                    bail!(
                        "line {}: `hand over to` must be the last statement of the program; use `launch detached` to start a program and continue",
                        step.line
                    );
                }
            }
            // A detached command produces no pipeline data, so none may feed from it.
            _ => {
                if let Some(Step {
                    kind: StepKind::Invoke { connection, .. },
                    ..
                }) = program.steps.get(index + 2)
                {
                    if *connection != Connection::Start {
                        bail!(
                            "line {}: `launch detached` cannot feed a pipeline; the detached command produces no output",
                            step.line
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Flush what this process has already written, so the handed-over program
/// starts after our earlier output and nothing is lost in a buffer.
fn flush_output() -> Result<()> {
    std::io::stdout()
        .flush()
        .context("flushing stdout before handing over")?;
    std::io::stderr()
        .flush()
        .context("flushing stderr before handing over")
}

/// Stdio for a detached child: its stdout and stderr share the opened output
/// file, or go to null when there is none. The caller opened the file through the
/// redirect checks, so this only hands it to the child.
fn detached_stdio(output: Option<File>) -> Result<(Stdio, Stdio, Stdio)> {
    let Some(file) = output else {
        return Ok((Stdio::null(), Stdio::null(), Stdio::null()));
    };
    let stderr = file.try_clone().context("duplicating detached output")?;
    Ok((Stdio::null(), Stdio::from(file), Stdio::from(stderr)))
}

/// Refuse hand-over and detached launch anywhere in a program that runs live, where a
/// process cannot take over or outlive the live session. Checks the whole program,
/// including statements before a live cursor, so an edit cannot hide a marker there.
#[cfg(feature = "live")]
pub(crate) fn reject_in_live(program: &Program) -> Result<()> {
    if let Some(step) = program.steps.iter().find(|step| {
        matches!(
            step.kind,
            StepKind::HandOver | StepKind::LaunchDetached { .. } | StepKind::TempScope { .. }
        )
    }) {
        bail!(
            "line {}: live programs do not support hand over, detached launch, or temporary scopes",
            step.line
        );
    }
    Ok(())
}

/// Hand the script over to `command`. On Unix this returns only if `execve` fails.
/// On Windows it returns the child's exit code once the child has finished.
#[cfg(unix)]
pub(crate) fn hand_over(mut command: Command) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    flush_output()?;
    // Signal dispositions and the mask survive exec; the replacement program
    // must start with the default mask and no inherited blocked signals.
    // SAFETY: the closure only calls async-signal-safe libc functions.
    unsafe {
        command.pre_exec(|| {
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // std opens every descriptor it creates with O_CLOEXEC, so the checkpoint
    // lock and other runtime files do not survive into the new program.
    let error = command.exec();
    Err(error).context("handing over to the native program")
}

/// The cancellation and deadline check for a Windows hand-over wait. The
/// deadline error carries the timeout marker so the run records it as a
/// timeout, and the cancellation error carries the cancelled kind so the run
/// reports it as a cancellation rather than a plain failure.
#[cfg(windows)]
fn check_handoff_budget(signals: &super::Signals) -> Result<()> {
    if signals
        .deadline
        .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    {
        return Err(anyhow::anyhow!(
            "execution deadline exceeded; external effects may be incomplete"
        )
        .context(super::CommandTimedOut));
    }
    if signals.is_cancelled() {
        return Err(super::execution_result::ExecutionFailure::new(
            super::execution_result::ExecutionResult::runtime_failure(
                super::execution_result::ExecutionKind::Cancelled,
                "workflow cancelled by signal",
                super::execution_result::EffectCertainty::Uncertain,
            ),
        )
        .into());
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn hand_over(mut command: Command, signals: &super::Signals) -> Result<i32> {
    flush_output()?;
    // Refuse before spawning when the run is already over budget, instead of
    // starting a child that the wait loop would immediately have to kill.
    check_handoff_budget(signals)?;
    // The child joins a job that dies with this process: if the interpreter
    // is killed, the job handle closes and `KILL_ON_JOB_CLOSE` ends the child
    // instead of leaving it behind as an orphan. `Job::spawn` starts the
    // child suspended so no instruction runs before it belongs to the job.
    let (mut child, job) = crate::platform::windows::Job::spawn(&mut command)
        .context("starting the native program")?;
    let result = (|| -> Result<i32> {
        loop {
            check_handoff_budget(signals)?;
            if let Some(status) = child.try_wait().context("waiting for the native program")? {
                // Windows has no signal-terminated status; a missing code is reported as failure.
                return Ok(status.code().unwrap_or(1));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    })();
    // Ending the job kills a still-running child on cancellation or deadline;
    // when the child already exited this is skipped. Waiting reaps the child
    // so a cancelled hand-over never leaves a zombie behind.
    if result.is_err() {
        job.terminate();
        let _ = child.wait();
    }
    result
}

/// Start `command` without waiting for it. Returns the child's process id.
#[cfg(unix)]
pub(crate) fn launch_detached(mut command: Command, output: Option<File>) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    flush_output()?;
    let (stdin, stdout, stderr) = detached_stdio(output)?;
    command.stdin(stdin).stdout(stdout).stderr(stderr);
    // SAFETY: setsid is async-signal-safe. A new session means the child has no
    // controlling terminal and is not in this process's group, so a terminal
    // signal aimed at us does not reach it.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("launching the native program detached")?;
    let pid = child.id();
    // A waiter thread owns the child and reaps it when it exits, so finished detached
    // programs do not linger as zombies. The waiter never signals or cancels the child:
    // the script's exit does not reach it, and a waiter outliving the script just
    // reaps the child when it finishes.
    std::thread::Builder::new()
        .name("detached-child-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .context("starting the detached child waiter")?;
    Ok(pid)
}

#[cfg(windows)]
pub(crate) fn launch_detached(mut command: Command, output: Option<File>) -> Result<u32> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    flush_output()?;
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    // CREATE_BREAKAWAY_FROM_JOB escapes an outer job only when that job permits
    // breakaway (JOB_OBJECT_LIMIT_BREAKAWAY_OK). Where the job forbids it, spawn
    // refuses the flag and this retries without it, so the child stays in the outer
    // job and that job's close can end it. Output
    // files are reopened per attempt because a refused spawn may have consumed them.
    let mut spawn = |flags: u32| -> Result<std::process::Child> {
        let output = match &output {
            Some(file) => Some(file.try_clone().context("duplicating detached output")?),
            None => None,
        };
        let (stdin, stdout, stderr) = detached_stdio(output)?;
        command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .creation_flags(flags);
        Ok(command.spawn()?)
    };
    let child = match spawn(base | CREATE_BREAKAWAY_FROM_JOB) {
        Ok(child) => child,
        Err(_) => spawn(base).context("launching the native program detached")?,
    };
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs::File;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn shell(script_unix: &str, script_windows: &str, cwd: &Path) -> Command {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/c", script_windows]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", script_unix]);
            command
        };
        command.current_dir(cwd);
        command
    }

    #[test]
    fn detached_launch_returns_before_the_child_finishes_and_keeps_its_output() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("marker.txt");
        // The child waits briefly, so returning first is observable.
        let command = shell(
            "sleep 0.3; printf done > marker.txt",
            "ping -n 2 127.0.0.1 >nul & echo done> marker.txt",
            temp.path(),
        );
        let started = Instant::now();
        let log = File::create(temp.path().join("out.log")).unwrap();
        let pid = launch_detached(command, Some(log)).unwrap();
        assert!(pid > 0);
        assert!(
            !marker.exists(),
            "the launch must return before the child writes its marker"
        );
        assert!(started.elapsed() < Duration::from_secs(2));

        let deadline = Instant::now() + Duration::from_secs(20);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "detached child never finished");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(windows)]
    #[test]
    fn hand_over_returns_the_child_exit_code() {
        use std::sync::{atomic::AtomicBool, Arc};
        let temp = tempfile::tempdir().unwrap();
        let signals = crate::runtime::Signals::with_cancellation_and_deadline(
            Arc::new(AtomicBool::new(false)),
            false,
            None,
        )
        .unwrap();
        let mut command = shell("exit 7", "exit /b 7", temp.path());
        command.env_clear();
        assert_eq!(hand_over(command, &signals).unwrap(), 7);
    }

    #[cfg(windows)]
    #[test]
    fn hand_over_refuses_to_start_when_already_cancelled() {
        use std::sync::{atomic::AtomicBool, Arc};
        let cancelled = Arc::new(AtomicBool::new(true));
        let signals = crate::runtime::Signals::with_cancellation_and_deadline(
            cancelled,
            false,
            None,
        )
        .unwrap();
        // The command is never spawned: the pre-spawn budget check refuses it.
        let command = std::process::Command::new("execute-no-such-program-for-handoff");
        let error = hand_over(command, &signals).unwrap_err();
        let failure = error
            .downcast_ref::<crate::runtime::execution_result::ExecutionFailure>()
            .expect("cancellation must report the cancelled kind");
        assert_eq!(
            failure.result.kind,
            crate::runtime::execution_result::ExecutionKind::Cancelled
        );
    }

    #[cfg(windows)]
    #[test]
    fn hand_over_cancels_mid_wait_and_reaps_the_child() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let temp = tempfile::tempdir().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        // Cancel 200 ms into a ~30 second wait: the wait loop must observe the
        // flag, terminate the job, and reap the child instead of blocking.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            stop.store(true, Ordering::Release);
        });
        let signals = crate::runtime::Signals::with_cancellation_and_deadline(
            cancelled,
            false,
            None,
        )
        .unwrap();
        let mut command = shell(
            "sleep 30",
            "ping -n 30 127.0.0.1 >nul",
            temp.path(),
        );
        command.env_clear();
        let started = Instant::now();
        let error = hand_over(command, &signals).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "a cancelled hand-over must return promptly"
        );
        let failure = error
            .downcast_ref::<crate::runtime::execution_result::ExecutionFailure>()
            .expect("cancellation must report the cancelled kind");
        assert_eq!(
            failure.result.kind,
            crate::runtime::execution_result::ExecutionKind::Cancelled
        );
    }

    #[cfg(windows)]
    #[test]
    fn hand_over_reports_an_expired_deadline_without_starting_the_child() {
        use std::sync::{atomic::AtomicBool, Arc};
        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let signals = crate::runtime::Signals::with_cancellation_and_deadline(
            Arc::new(AtomicBool::new(false)),
            false,
            Some(past),
        )
        .unwrap();
        // The command is never spawned: the pre-spawn budget check refuses it.
        let command = std::process::Command::new("execute-no-such-program-for-handoff");
        let error = hand_over(command, &signals).unwrap_err();
        assert!(
            error.is::<super::super::CommandTimedOut>(),
            "expired deadline must report the timeout marker: {error:#}"
        );
    }

    #[cfg(feature = "live")]
    #[test]
    fn live_refusal_covers_markers_before_a_cursor() {
        let program =
            crate::parser::parse("launch detached \"x\"\nrun text.trim_final_lf with text \"a\"\n")
                .unwrap();
        assert!(reject_in_live(&program).is_err());
        let plain = crate::parser::parse("run text.trim_final_lf with text \"a\"\n").unwrap();
        assert!(reject_in_live(&plain).is_ok());
    }

    #[test]
    fn hand_over_reports_a_missing_program_without_running_anything() {
        #[cfg(unix)]
        {
            let command = Command::new(OsString::from("execute-no-such-program-for-handoff"));
            assert!(hand_over(command).is_err());
        }
        #[cfg(windows)]
        {
            use std::sync::{atomic::AtomicBool, Arc};
            let signals = crate::runtime::Signals::with_cancellation_and_deadline(
                Arc::new(AtomicBool::new(false)),
                false,
                None,
            )
            .unwrap();
            let command = Command::new(OsString::from("execute-no-such-program-for-handoff"));
            assert!(hand_over(command, &signals).is_err());
        }
    }
}
