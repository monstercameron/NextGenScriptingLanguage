//! `hand over to` replaces the script with one native program, and `launch detached`
//! starts one that the script does not own. These tests cover the grammar and the
//! placement and mode rules, a detached launch that returns before its child
//! finishes, and exit status through a hand-over on each platform.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[path = "support/private_answers.rs"]
mod private_answers;

fn execute(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(directory)
        // Handover is refused under Actions hardening, so never inherit an
        // ambient CI marker: these tests pin the non-hardened contract.
        .env_remove("GITHUB_ACTIONS")
        .output()
        .unwrap()
}

fn write_program(directory: &Path, text: &str) -> std::path::PathBuf {
    let program = directory.join("handoff.program");
    fs::write(&program, text).unwrap();
    program
}

fn run_program(text: &str) -> Output {
    let directory = tempdir().unwrap();
    let program = write_program(directory.path(), text);
    execute(&["run", program.to_str().unwrap()], directory.path())
}

/// A native program that prints one word and exits with the given status.
fn print_and_exit(word: &str, status: u8) -> String {
    if cfg!(windows) {
        format!(
            "\"C:/Windows/System32/cmd.exe\" with arguments [\"/c\", \"echo {word} & exit /b {status}\"]"
        )
    } else {
        format!("\"/bin/sh\" with arguments [\"-c\", \"printf {word}; exit {status}\"]")
    }
}

#[test]
fn hand_over_must_be_the_last_statement() {
    let program = format!(
        "hand over to {}\nrun text.trim_final_lf with text \"after\"\n",
        print_and_exit("first", 0)
    );
    let output = run_program(&program);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must be the last statement"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn hand_over_and_detached_launch_require_a_quoted_native_program() {
    for text in [
        "hand over to text.trim_final_lf with text \"x\"\n",
        "launch detached text.trim_final_lf with text \"x\"\n",
    ] {
        let output = run_program(text);
        assert_ne!(output.status.code(), Some(0), "program accepted: {text}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("quoted native program path"),
            "program {text:?} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn hand_over_refuses_result_bindings_and_discarded_stderr() {
    let program = format!(
        "hand over to {} and discard stderr\n",
        print_and_exit("x", 0)
    );
    let output = run_program(&program);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not bind results"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn hand_over_and_detached_launch_refuse_checkpointed_runs() {
    let directory = tempdir().unwrap();
    let checkpoint = directory.path().join("checkpoint");
    let program = write_program(
        directory.path(),
        &format!("launch detached {}\n", print_and_exit("x", 0)),
    );
    let output = execute(
        &[
            "run",
            program.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
        directory.path(),
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not supported under --checkpoint"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !checkpoint.exists(),
        "a refused run must not create a checkpoint"
    );
}

#[test]
fn detached_launch_returns_while_the_child_keeps_running() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("detached.txt");
    // The child writes its marker after a short wait, so the script must not be
    // waiting for it. The script continues and finishes first. PowerShell
    // is the Windows sleeper: a `cmd /c ping` payload fails at once on hosted
    // runners ("The system cannot find the path specified"), and the
    // `& echo done` fallback would mask that by writing the marker immediately
    // instead of after a wait.
    let child = if cfg!(windows) {
        format!(
            "\"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", {}]",
            serde_json::to_string("Start-Sleep -Seconds 2; 'done'").unwrap()
        )
    } else {
        "\"/bin/sh\" with arguments [\"-c\", \"sleep 0.5; printf done\"]".to_owned()
    };
    let program = write_program(
        directory.path(),
        &format!(
            "launch detached {child} and output to \"detached.txt\"\nrun text.trim_final_lf with text \"after\"\n"
        ),
    );
    let started = Instant::now();
    let output = execute(&["run", program.to_str().unwrap()], directory.path());
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "after");
    assert!(started.elapsed() < Duration::from_secs(10));

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if fs::read_to_string(&marker).is_ok_and(|text| text.contains("done")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the detached child never wrote its output"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn hand_over_replaces_the_script_and_passes_its_exit_status() {
    // The handed-over program is this test binary's `execute`, running an inner
    // program that prints one word and exits with status 7. Forward slashes keep
    // the path free of escapes in the script's string literal.
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("inner.program"),
        "run text.trim_final_lf with text \"handed\"\nexit with status 7\n",
    )
    .unwrap();
    let execute_path = env!("CARGO_BIN_EXE_execute").replace('\\', "/");
    let program = write_program(
        directory.path(),
        &format!(
            "hand over to \"{execute_path}\" with arguments [\"run\", \"inner.program\"]
"
        ),
    );
    let output = execute(&["run", program.to_str().unwrap()], directory.path());
    assert_eq!(
        output.status.code(),
        Some(7),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("handed"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn hand_over_enforces_the_workflow_deadline() {
    // Unix replaces the interpreter with `execve`, so no waiter remains to
    // enforce a deadline after the hand-over. This exercises the Windows wait:
    // PowerShell sleeps five seconds, the 500 ms workflow deadline fires
    // first, and the job object ends the child. A `cmd /c ping` sleeper does
    // not work here: on hosted runners it fails at once with "The system
    // cannot find the path specified", exiting before the deadline can fire.
    if !cfg!(windows) {
        return;
    }
    let directory = tempdir().unwrap();
    let program = write_program(
        directory.path(),
        &format!(
            "hand over to \"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", {}]\n",
            serde_json::to_string("Start-Sleep -Seconds 5").unwrap()
        ),
    );
    let output = execute(
        &[
            "run",
            program.to_str().unwrap(),
            "--total-timeout-ms",
            "500",
        ],
        directory.path(),
    );
    assert_ne!(
        output.status.code(),
        Some(0),
        "a hand-over past its deadline must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deadline"),
        "a deadline failure must say so, stderr: {stderr}"
    );
}

/// Read a PID published by a fixture script. PowerShell `Out-File` appends a
/// newline, so the first whitespace-separated token is parsed and zero is
/// refused; anything else means the fixture has not published yet.
fn read_pid_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()?
        .split_whitespace()
        .next()?
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid != 0)
}

/// Fail-closed liveness probe: the `tasklist` invocation itself must run,
/// otherwise the test fails instead of reading an empty listing as "gone". A
/// PID counts as running only when a CSV field equals it exactly, so a PID
/// that is a substring of another process's PID never matches. A non-zero
/// `tasklist` exit that still reports "no tasks" (some SKUs/locales) counts
/// as gone; any other failure retries twice and then fails the test.
fn probe_pid(pid: u32) -> Option<String> {
    let want = pid.to_string();
    for attempt in 0..3 {
        let probe = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("the tasklist liveness probe must run");
        let stdout = String::from_utf8_lossy(&probe.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&probe.stderr).into_owned();
        if probe.status.success() {
            for line in stdout.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with("INFO:") {
                    continue;
                }
                let fields: Vec<&str> = line
                    .split(',')
                    .map(|field| field.trim().trim_matches('"'))
                    .collect();
                if fields.iter().any(|field| *field == want) {
                    return fields.first().map(|image| image.to_string());
                }
            }
            return None;
        }
        let combined = format!("{stdout}\n{stderr}");
        if combined.contains("INFO:") || combined.to_uppercase().contains("NO TASKS") {
            return None;
        }
        if attempt + 1 < 3 {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        panic!("the tasklist probe for PID {pid} failed: {}", stderr.trim());
    }
    unreachable!("the tasklist probe retry loop must return or panic");
}

/// Our fixture sleepers are `powershell.exe`: the image corroboration keeps a
/// recycled PID from reading as "our process still running" and, more
/// importantly, keeps cleanup from killing it.
fn our_pid_is_running(pid: u32) -> bool {
    probe_pid(pid).is_some_and(|image| image.eq_ignore_ascii_case("powershell.exe"))
}

/// Conditional cleanup: only `taskkill` a PID that still probes as one of our
/// `powershell.exe` sleepers. An already-gone PID (possibly recycled to an
/// unrelated process) is left alone.
fn kill_pid_if_running(pid: u32) {
    if our_pid_is_running(pid) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
}

#[test]
fn hand_over_kill_ends_the_handed_over_child() {
    // Windows-only acceptance for the job object behind `hand over`: killing
    // the interpreter must end the handed-over child and the descendant it
    // started instead of orphaning them. Unix replaces the interpreter with
    // `execve`, so there is no waiter to kill and nothing to orphan.
    if !cfg!(windows) {
        return;
    }
    use std::process::Stdio;

    let directory = tempdir().unwrap();
    let pid_file = directory.path().join("handed-over.pid");
    let descendant_pid_file = directory.path().join("descendant.pid");
    let pid_forward = pid_file.to_str().unwrap().replace('\\', "/");
    let descendant_forward = descendant_pid_file.to_str().unwrap().replace('\\', "/");
    // The handed-over program records its own PID, starts a descendant
    // sleeper in the same job, records the descendant PID, and sleeps for a
    // minute, so the test can kill the interpreter and probe for both
    // directly. PowerShell is an inbox native fixture; `tasklist`/`taskkill`
    // below are inbox probes and bounded cleanup.
    let powershell = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
    let script = format!(
        "$PID | Out-File -FilePath '{pid_forward}' -Encoding ascii; \
         $desc = Start-Process -FilePath '{powershell}' \
         -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 60' -PassThru; \
         $desc.Id | Out-File -FilePath '{descendant_forward}' -Encoding ascii; \
         Start-Sleep -Seconds 60"
    );
    let program = write_program(
        directory.path(),
        &format!(
            "hand over to \"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", {}]\n",
            serde_json::to_string(&script).unwrap()
        ),
    );
    let mut interpreter = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", program.to_str().unwrap()])
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Wait for the handed-over child and its descendant to publish their PIDs.
    // The 20 second bound keeps a slow hosted runner from hanging the suite.
    // Both PIDs must be observed running before the kill: a gone-after probe
    // proves the job ended them only if they demonstrably existed first.
    let pid_by = Instant::now() + Duration::from_secs(20);
    let (child_pid, descendant_pid) = loop {
        let pair = (
            read_pid_file(&pid_file),
            read_pid_file(&descendant_pid_file),
        );
        if let (Some(child), Some(descendant)) = pair {
            if our_pid_is_running(child) && our_pid_is_running(descendant) {
                break (child, descendant);
            }
        }
        if interpreter.try_wait().unwrap().is_some() {
            let _ = interpreter.wait();
            panic!("the interpreter exited before the handed-over child started");
        }
        if Instant::now() >= pid_by {
            let _ = interpreter.kill();
            let _ = interpreter.wait();
            if let Some(pid) = read_pid_file(&pid_file) {
                kill_pid_if_running(pid);
            }
            if let Some(pid) = read_pid_file(&descendant_pid_file) {
                kill_pid_if_running(pid);
            }
            panic!("the handed-over child and its descendant never started");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        interpreter.try_wait().unwrap().is_none(),
        "the interpreter must still be waiting on the handed-over child"
    );

    // Killing the interpreter closes the job handle, and KILL_ON_JOB_CLOSE
    // must end the handed-over child and its descendant with it.
    let _ = interpreter.kill();
    let _ = interpreter.wait();

    // Descendant teardown after the job closes is asynchronous, so the
    // gone-assert polls instead of checking once. Image-corroborated probes
    // keep a recycled PID from reading as a survivor.
    let gone_by = Instant::now() + Duration::from_secs(10);
    loop {
        if !our_pid_is_running(child_pid) && !our_pid_is_running(descendant_pid) {
            break;
        }
        if Instant::now() >= gone_by {
            kill_pid_if_running(child_pid);
            kill_pid_if_running(descendant_pid);
            panic!(
                "handed-over child {child_pid} or descendant {descendant_pid} \
                 survived killing the interpreter"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Belt and braces: sweep our sleepers only; a gone PID (possibly
    // recycled) is left alone.
    kill_pid_if_running(child_pid);
    kill_pid_if_running(descendant_pid);
}

#[test]
fn detached_output_follows_the_write_policy_before_the_child_starts() {
    // A policy with no grants forbids the write, so the launch is refused before
    // any child starts and the output file is never created.
    let directory = tempdir().unwrap();
    let policy = directory.path().join("policy.json");
    fs::write(&policy, r#"{"version":1,"applets":{}}"#).unwrap();
    private_answers::make_private(&policy);
    let execute_path = env!("CARGO_BIN_EXE_execute").replace('\\', "/");
    let program = write_program(
        directory.path(),
        &format!(
            "launch detached \"{execute_path}\" with arguments [\"--version\"] and output to \"out.txt\"
"
        ),
    );
    let output = execute(
        &[
            "run",
            program.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
        directory.path(),
    );
    assert_ne!(
        output.status.code(),
        Some(0),
        "the write policy did not refuse the launch"
    );
    // A policy denies direct quoted commands outright, and a detached launch is always
    // a direct command, so the refusal comes before the output file is opened.
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("denies direct quoted commands"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !directory.path().join("out.txt").exists(),
        "a refused output must not be created"
    );
}

#[test]
fn hand_over_writes_earlier_output_before_the_program_starts() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("inner.program"),
        "run text.trim_final_lf with text \"inner\"\n",
    )
    .unwrap();
    let execute_path = env!("CARGO_BIN_EXE_execute").replace('\\', "/");
    let program = write_program(
        directory.path(),
        &format!(
            "run text.trim_final_lf with text \"before\"\nhand over to \"{execute_path}\" with arguments [\"run\", \"inner.program\"]\n"
        ),
    );
    let output = execute(&["run", program.to_str().unwrap()], directory.path());
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "beforeinner");
}

#[cfg(feature = "live")]
#[test]
fn run_control_refuses_hand_over_and_detached_launch() {
    let directory = tempdir().unwrap();
    for text in [
        "hand over to \"C:/Windows/System32/cmd.exe\" with arguments [\"/c\", \"exit\"]\n",
        "launch detached \"C:/Windows/System32/cmd.exe\" with arguments [\"/c\", \"exit\"]\n",
    ] {
        let program = write_program(directory.path(), text);
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["run", program.to_str().unwrap(), "--control"])
            .current_dir(directory.path())
            .env_remove("GITHUB_ACTIONS")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(0), "--control accepted: {text}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("live programs do not support"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn detached_launches_are_capped_per_run() {
    let directory = tempdir().unwrap();
    let items: Vec<String> = (0..65).map(|index| format!("\"n{index}\"")).collect();
    let execute_path = env!("CARGO_BIN_EXE_execute").replace('\\', "/");
    let program = write_program(
        directory.path(),
        &format!(
            "for each item in [{}]\n  launch detached \"{execute_path}\" with arguments [\"--version\"]\n",
            items.join(", ")
        ),
    );
    let output = execute(&["run", program.to_str().unwrap()], directory.path());
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("limited to 64 detached programs"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
