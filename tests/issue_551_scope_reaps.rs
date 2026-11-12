//! Named jobs must be cancelled and reaped by the scope that owns them, on every
//! platform: a try body's job when the body fails (before the catch block runs,
//! with or without a finally block), a job owned by an enclosing scope when an
//! inner try fails, and a service's setup job when the service is cancelled.
//! Each job records its own process id and then sleeps for a minute, so only
//! cancellation can end it before the test does.

use flow::{manifest::Registry, parser, runtime};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

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

/// One `run` statement with its arguments quoted for the language.
fn run_stage(command: &str, arguments: &[String]) -> String {
    let arguments = arguments
        .iter()
        .map(|argument| quote(argument))
        .collect::<Vec<_>>()
        .join(", ");
    format!("run {} with arguments [{arguments}]", quote(command))
}

/// The long-running job stage: writes its own pid to `pid_file`, then sleeps.
fn job_stage(pid_file: &Path) -> String {
    if cfg!(windows) {
        let script = format!(
            "Set-Content -LiteralPath '{}' -Value $PID; Start-Sleep -Seconds 60",
            pid_file.display()
        );
        run_stage(
            "powershell",
            &["-NoProfile".into(), "-Command".into(), script],
        )
    } else {
        run_stage(
            "sh",
            &[
                "-c".into(),
                "echo $$ > \"$1\"; exec sleep 60".into(),
                "job".into(),
                pid_file.to_string_lossy().into_owned(),
            ],
        )
    }
}

/// Waits, for a bounded time, until the job has written a non-empty pid file.
fn wait_stage(pid_file: &Path) -> String {
    if cfg!(windows) {
        let script = format!(
            "$p = '{}'; for ($i = 0; $i -lt 500; $i++) {{ if ((Test-Path -LiteralPath $p) -and ((Get-Item -LiteralPath $p).Length -gt 0)) {{ exit 0 }}; Start-Sleep -Milliseconds 20 }}; exit 1",
            pid_file.display()
        );
        run_stage(
            "powershell",
            &["-NoProfile".into(), "-Command".into(), script],
        )
    } else {
        run_stage(
            "sh",
            &[
                "-c".into(),
                "i=0; while [ ! -s \"$1\" ] && [ $i -lt 500 ]; do sleep 0.02; i=$((i + 1)); done; [ -s \"$1\" ]".into(),
                "wait".into(),
                pid_file.to_string_lossy().into_owned(),
            ],
        )
    }
}

/// Creates `marker` only if the process named in `pid_file` is still alive. It
/// always succeeds, so the probe itself never fails the program.
fn probe_stage(pid_file: &Path, marker: &Path) -> String {
    if cfg!(windows) {
        let script = format!(
            "$p = [int](Get-Content -LiteralPath '{}'); if (Get-Process -Id $p -ErrorAction SilentlyContinue) {{ Set-Content -LiteralPath '{}' -Value alive }}; exit 0",
            pid_file.display(),
            marker.display()
        );
        run_stage(
            "powershell",
            &["-NoProfile".into(), "-Command".into(), script],
        )
    } else {
        run_stage(
            "sh",
            &[
                "-c".into(),
                "if kill -0 \"$(cat \"$1\")\" 2>/dev/null; then touch \"$2\"; fi; exit 0".into(),
                "probe".into(),
                pid_file.to_string_lossy().into_owned(),
                marker.to_string_lossy().into_owned(),
            ],
        )
    }
}

/// A stage that fails at once.
fn failing_stage() -> String {
    if cfg!(windows) {
        run_stage("cmd", &["/C".into(), "exit 1".into()])
    } else {
        run_stage("sh", &["-c".into(), "exit 1".into()])
    }
}

/// A stage that succeeds at once.
fn succeeding_stage() -> String {
    if cfg!(windows) {
        run_stage("cmd", &["/C".into(), "exit 0".into()])
    } else {
        run_stage("sh", &["-c".into(), "exit 0".into()])
    }
}

/// A stage that prints one line, so a service run emits output.
fn tick_stage() -> String {
    if cfg!(windows) {
        run_stage("cmd", &["/C".into(), "echo tick".into()])
    } else {
        run_stage("sh", &["-c".into(), "echo tick".into()])
    }
}

fn pid_recorded(pid_file: &Path) -> bool {
    std::fs::read_to_string(pid_file).is_ok_and(|text| !text.trim().is_empty())
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

/// Polls for process exit for at most five seconds. Returns whether it exited.
fn wait_until_gone(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !process_is_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn try_body_job_is_reaped_before_catch_runs() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("try-job.pid");
    let alive_in_catch = directory.path().join("try-job-alive-in-catch");
    // The static check requires the body to resolve its job on normal exit, so
    // `cancel worker` follows the failing stage. A failure never reaches it: the
    // failure enters the catch block, which must reap the job first.
    let source = format!(
        "try\n  spawn as worker\n    {}\n  {}\n  {}\n  cancel worker\ncatch failure\n  {}\n  then encode \"caught\" as json\n",
        job_stage(&pid_file),
        wait_stage(&pid_file),
        failing_stage(),
        probe_stage(&pid_file, &alive_in_catch),
    );
    let program = parser::parse(&source).unwrap();

    let result = match runtime::run(&program, &Registry::default(), &options(directory.path())) {
        Ok(result) => result,
        Err(error) => panic!("the catch block should handle the body failure: {error:#}"),
    };
    assert_eq!(result.output, b"\"caught\"");
    assert!(
        !alive_in_catch.exists(),
        "the try body's job was still alive when the catch block ran"
    );

    let pid = read_pid(&pid_file);
    assert!(
        wait_until_gone(pid),
        "the try body's job (pid {pid}) survived the statement"
    );
}

#[test]
fn try_body_job_is_reaped_before_catch_and_finally_runs() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("finally-job.pid");
    let alive_in_catch = directory.path().join("finally-job-alive-in-catch");
    // As above: `cancel worker` only satisfies the static normal-exit check.
    let source = format!(
        "try\n  spawn as worker\n    {}\n  {}\n  {}\n  cancel worker\ncatch failure\n  {}\n  then encode \"caught\" as json\nfinally\n  {}\n",
        job_stage(&pid_file),
        wait_stage(&pid_file),
        failing_stage(),
        probe_stage(&pid_file, &alive_in_catch),
        succeeding_stage(),
    );
    let program = parser::parse(&source).unwrap();

    if let Err(error) = runtime::run(&program, &Registry::default(), &options(directory.path())) {
        panic!("the catch block should handle the body failure: {error:#}");
    }
    assert!(
        !alive_in_catch.exists(),
        "the try body's job was still alive when the catch block ran"
    );

    let pid = read_pid(&pid_file);
    assert!(
        wait_until_gone(pid),
        "the try body's job (pid {pid}) survived the statement with a finally block"
    );
}

#[test]
fn job_started_before_try_survives_the_try_failure_until_its_own_scope_ends() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("outer-job.pid");
    let alive_in_catch = directory.path().join("outer-job-alive-in-catch");
    let alive_after_try = directory.path().join("outer-job-alive-after-try");
    let source = format!(
        "spawn as outer_worker\n  {}\n{}\ntry\n  {}\ncatch failure\n  {}\n{}\ncancel outer_worker\n",
        job_stage(&pid_file),
        wait_stage(&pid_file),
        failing_stage(),
        probe_stage(&pid_file, &alive_in_catch),
        probe_stage(&pid_file, &alive_after_try),
    );
    let program = parser::parse(&source).unwrap();

    if let Err(error) = runtime::run(&program, &Registry::default(), &options(directory.path())) {
        panic!("the outer job must survive the caught try failure: {error:#}");
    }
    assert!(
        alive_in_catch.exists(),
        "the outer job was reaped by the try failure before the catch block ran"
    );
    assert!(
        alive_after_try.exists(),
        "the outer job was reaped by the try statement instead of its own scope"
    );

    let pid = read_pid(&pid_file);
    assert!(
        wait_until_gone(pid),
        "the outer job (pid {pid}) survived its own scope end"
    );
}

#[test]
fn service_cancellation_reaps_setup_owned_job() {
    use std::sync::atomic::AtomicBool;

    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("service-job.pid");
    let source = format!(
        "spawn as service_worker\n  {}\nevery 10 milliseconds\n  {}\n",
        job_stage(&pid_file),
        tick_stage(),
    );
    let program = parser::parse(&source).unwrap();
    let limits = runtime::ServiceLimits {
        max_runs: None,
        max_record_bytes: 1024 * 1024,
    };
    // The service stops through the same cancellation flag that SIGTERM sets.
    // The output sink runs after each handler and receives that flag, so it
    // raises the flag once the setup job has recorded its pid.
    let result = runtime::run_service_with_cancellation(
        &program,
        &Registry::default(),
        &runtime::RunOptions {
            total_timeout: Some(Duration::from_secs(30)),
            ..options(directory.path())
        },
        &limits,
        |_bytes: &[u8], cancelled: &AtomicBool| {
            if pid_recorded(&pid_file) {
                cancelled.store(true, Ordering::Release);
            }
            Ok(())
        },
    );
    let error = match result {
        Ok(_) => panic!("the cancelled service must stop with an error"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("cancelled"),
        "the service must stop by cancellation, not by another error: {message}"
    );

    let pid = read_pid(&pid_file);
    assert!(
        wait_until_gone(pid),
        "the service setup job (pid {pid}) survived service shutdown"
    );
}
