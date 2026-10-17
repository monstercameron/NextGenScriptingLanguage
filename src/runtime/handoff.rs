//! Process handoff (`hand over to`) and detached launch (`launch detached`).
//!
//! Both take the `Command` that the capability command builder produces for the
//! native program, so the environment is cleared and rebuilt from the grant and
//! any required OS sandbox wraps the process exactly as for other native commands.
//!
//! A hand-over replaces the script with one program. On Unix this is `execve`
//! after flushing our output; the process keeps its pid and exit status. Windows
//! has no in-place replacement, so the child is spawned with inherited stdio,
//! waited for, and its exit code becomes the script's exit code.
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
pub(crate) fn reject_in_live(program: &Program) -> Result<()> {
    if let Some(step) = program.steps.iter().find(|step| {
        matches!(
            step.kind,
            StepKind::HandOver | StepKind::LaunchDetached { .. }
        )
    }) {
        bail!(
            "line {}: live programs do not support hand over or detached launch",
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

#[cfg(windows)]
pub(crate) fn hand_over(mut command: Command) -> Result<i32> {
    flush_output()?;
    let mut child = command.spawn().context("starting the native program")?;
    let status = child.wait().context("waiting for the native program")?;
    // Windows has no signal-terminated status; a missing code is reported as failure.
    Ok(status.code().unwrap_or(1))
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
        let temp = tempfile::tempdir().unwrap();
        let mut command = shell("exit 7", "exit /b 7", temp.path());
        command.env_clear();
        assert_eq!(hand_over(command).unwrap(), 7);
    }

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
        let command = Command::new(OsString::from("execute-no-such-program-for-handoff"));
        assert!(hand_over(command).is_err());
    }
}
