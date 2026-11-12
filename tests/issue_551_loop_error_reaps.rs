//! A named job started in a loop body must be cancelled and reaped when the
//! body fails, on every platform. The job records its own process id and then
//! sleeps for a minute, so only cancellation can end it before the test ends.

use flow::{manifest::Registry, parser, runtime};
use std::path::Path;
use std::time::Duration;

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(20)),
        total_timeout: Some(Duration::from_secs(40)),
        ..runtime::RunOptions::new(cwd.to_owned())
    }
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

/// The long-running job stage: writes its own pid to `pid_file`, then sleeps.
fn job_stage(pid_file: &Path) -> String {
    if cfg!(windows) {
        let script = format!(
            "Set-Content -LiteralPath '{}' -Value $PID; Start-Sleep -Seconds 60",
            pid_file.display()
        );
        format!(
            "run {} with arguments [\"-NoProfile\", \"-Command\", {}]",
            quote("powershell"),
            quote(&script)
        )
    } else {
        format!(
            "run {} with arguments [\"-c\", {}, \"job\", {}]",
            quote("sh"),
            quote("echo $$ > \"$1\"; exec sleep 60"),
            quote(&pid_file.to_string_lossy())
        )
    }
}

/// A failing stage that waits about three seconds, so the job has time to
/// record its pid before the failure cancels it.
fn failing_stage() -> String {
    if cfg!(windows) {
        format!(
            "run {} with arguments [\"/C\", {}]",
            quote("cmd"),
            quote("ping -n 4 127.0.0.1 >nul & exit 1")
        )
    } else {
        format!(
            "run {} with arguments [\"-c\", {}]",
            quote("sh"),
            quote("sleep 3; exit 1")
        )
    }
}

fn read_pid(pid_file: &Path) -> u32 {
    std::fs::read_to_string(pid_file)
        .unwrap_or_else(|error| panic!("the job never recorded its pid: {error}"))
        .trim()
        .parse()
        .expect("the recorded pid is numeric")
}

/// True while a process with this id exists. Windows lists it through
/// `tasklist`; Unix probes it with signal 0.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: the zero signal only checks whether a process with this numeric
    // id currently exists; nothing is delivered to it.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .expect("the tasklist liveness probe must run");
    String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
}

#[test]
fn loop_body_failure_cancels_and_reaps_its_job() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("loop-job.pid");
    let source = format!(
        "repeat 1 times\n  spawn as worker\n    {}\n  {}\n  await worker\n",
        job_stage(&pid_file),
        failing_stage(),
    );
    let program = parser::parse(&source).unwrap();

    let error = match runtime::run(&program, &Registry::default(), &options(directory.path())) {
        Ok(_) => panic!("the loop body fails after starting its job"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        !message.contains("unsupported"),
        "the failure must come from the body, not a refusal: {message}"
    );

    let pid = read_pid(&pid_file);
    assert!(
        !process_is_alive(pid),
        "the loop job (pid {pid}) survived its failing loop body"
    );
}

#[test]
fn loop_job_failure_inside_try_is_reaped_before_catch() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("caught-loop-job.pid");
    let source = format!(
        "try\n  repeat 1 times\n    spawn as worker\n      {}\n    {}\n    await worker\ncatch failure\n  run \"printf\" with arguments [\"caught\"]\n",
        job_stage(&pid_file),
        failing_stage(),
    );
    let program = parser::parse(&source).unwrap();

    let result = match runtime::run(&program, &Registry::default(), &options(directory.path())) {
        Ok(result) => result,
        Err(error) => panic!("the catch body should handle the loop failure: {error:#}"),
    };
    assert_eq!(result.output, b"caught");

    let pid = read_pid(&pid_file);
    assert!(
        !process_is_alive(pid),
        "the loop job (pid {pid}) survived into the catch body"
    );
}
