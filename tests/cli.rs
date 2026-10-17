#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn fixture() -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    (directory, manifest_dir)
}

fn invoke(
    directory: &Path,
    manifests: &Path,
    command: &str,
    script: &str,
    extra: &[&str],
) -> Output {
    let script_path = directory.join("workflow.program");
    fs::write(&script_path, script).unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_execute"));
    process.current_dir(directory);
    if command != "default" {
        process.arg(command).arg(&script_path);
    } else {
        process.arg(&script_path);
    }
    process.arg("--manifest-dir").arg(manifests);
    process.args(extra);
    process.output().unwrap()
}

#[test]
fn quoted_message_with_shell_syntax_is_printed_literally() {
    let (directory, manifests) = fixture();
    let message = "$(touch should-not-exist); `echo unsafe` | * $HOME";
    let script = format!("run printf with message {message:?}\n");
    let result = invoke(directory.path(), &manifests, "default", &script, &[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, format!("{message}\n").as_bytes());
    assert!(!directory.path().join("should-not-exist").exists());
}

#[test]
fn reads_file_pipes_to_uppercase_and_counts_three_lines() {
    let (directory, manifests) = fixture();
    fs::write(directory.path().join("input.txt"), "one\ntwo\nthree\n").unwrap();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "read file \"input.txt\"\npipe to uppercase\npipe to count lines\n",
        &[],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "3");
}

#[test]
fn cd_updates_only_workflow_directory_for_later_pwd_and_ls() {
    let (directory, manifests) = fixture();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let canonical_nested = fs::canonicalize(&nested).unwrap();
    fs::write(nested.join("visible.txt"), "owned fixture\n").unwrap();
    fs::write(nested.join(".hidden-fixture"), "owned hidden fixture\n").unwrap();
    let caller_directory = std::env::current_dir().unwrap();

    let pwd = invoke(
        directory.path(),
        &manifests,
        "default",
        "run cd with path \"nested\"\nthen pwd\n",
        &[],
    );
    assert!(
        pwd.status.success(),
        "{}",
        String::from_utf8_lossy(&pwd.stderr)
    );
    let output = String::from_utf8_lossy(&pwd.stdout);
    assert!(
        output
            .lines()
            .any(|line| line == canonical_nested.to_string_lossy()),
        "pwd did not report workflow directory {canonical_nested:?}: {output:?}"
    );

    let listing = invoke(
        directory.path(),
        &manifests,
        "default",
        "run cd with path \"nested\"\nthen ls with flags [\"-a\"] and path \".\"\n",
        &[],
    );
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let output = String::from_utf8_lossy(&listing.stdout);
    assert!(
        output.contains(".hidden-fixture"),
        "ls omitted hidden fixture: {output:?}"
    );
    assert!(
        output.contains("visible.txt"),
        "ls omitted visible fixture: {output:?}"
    );
    assert_eq!(std::env::current_dir().unwrap(), caller_directory);
}

#[test]
fn cd_cannot_pipe_to_or_from_an_applet_and_validation_prevents_prior_save() {
    let (directory, manifests) = fixture();
    fs::create_dir(directory.path().join("nested")).unwrap();

    for (script, forbidden_output) in [
        (
            "run printf with message \"must not save\"\nthen save to file \"before-invalid-pipe.txt\"\nthen cd with path \"nested\"\npipe to pwd\n",
            "before-invalid-pipe.txt",
        ),
        (
            "run printf with message \"must not save\"\nthen save to file \"before-invalid-pipe.txt\"\nthen pwd\npipe to cd with path \"nested\"\n",
            "before-invalid-pipe.txt",
        ),
    ] {
        let result = invoke(directory.path(), &manifests, "default", script, &[]);
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .to_lowercase()
                .contains("pipe"),
            "expected pipe validation error, got: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            !directory.path().join(forbidden_output).exists(),
            "workflow executed a save before detecting invalid cd pipe"
        );
    }
}

#[test]
fn cd_to_missing_directory_stops_before_a_later_save() {
    let (directory, manifests) = fixture();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "run cd with path \"missing-directory\"\nthen printf with message \"should not save\"\nthen save to file \"after-missing-cd.txt\"\n",
        &[],
    );
    assert!(!result.status.success());
    assert!(
        !directory.path().join("after-missing-cd.txt").exists(),
        "workflow continued after cd failed"
    );
}

#[test]
fn large_file_streams_through_a_native_pipe_group_with_backpressure() {
    let (directory, manifests) = fixture();
    let input: Vec<u8> = (0..1024 * 1024)
        .map(|index| match index % 4 {
            0 => b'a',
            1 => b'b',
            2 => b'c',
            _ => b'\n',
        })
        .collect();
    fs::write(directory.path().join("large.txt"), &input).unwrap();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "read file \"large.txt\"\npipe to uppercase\npipe to copy\n",
        &[],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, input.to_ascii_uppercase());
}

#[test]
fn cached_step_is_reported_on_second_run_and_saved_output_is_stable() {
    let (directory, manifests) = fixture();
    let script = "run printf with message \"cache me\"\nthen uppercase and cache\nthen save to file \"result.txt\"\n";
    let cache_dir = directory.path().join("cache");
    let first = invoke(
        directory.path(),
        &manifests,
        "default",
        script,
        &["--trace", "--cache-dir", cache_dir.to_str().unwrap()],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(!String::from_utf8_lossy(&first.stderr).contains("cache_hit"));
    let saved_once = fs::read(directory.path().join("result.txt")).unwrap();
    assert_eq!(saved_once, b"CACHE ME\n");

    let second = invoke(
        directory.path(),
        &manifests,
        "default",
        script,
        &["--trace", "--cache-dir", cache_dir.to_str().unwrap()],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("cache_hit"),
        "expected cache hit event, got: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        fs::read(directory.path().join("result.txt")).unwrap(),
        saved_once
    );
}

#[test]
fn validation_rejects_cache_inside_a_pipe_group() {
    let (directory, manifests) = fixture();
    let result = invoke(
        directory.path(),
        &manifests,
        "check",
        "run printf with message \"cache me\"\npipe to uppercase and cache\n",
        &[],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr)
        .to_lowercase()
        .contains("cache"));
}

#[test]
fn changed_input_misses_the_sequential_stage_cache() {
    let (directory, manifests) = fixture();
    let cache_dir = directory.path().join("cache");
    let script = "run printf with message \"first value\"\nthen uppercase and cache\n";
    let first = invoke(
        directory.path(),
        &manifests,
        "default",
        script,
        &["--trace", "--cache-dir", cache_dir.to_str().unwrap()],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let changed_script = "run printf with message \"changed value\"\nthen uppercase and cache\n";
    let changed = invoke(
        directory.path(),
        &manifests,
        "default",
        changed_script,
        &["--trace", "--cache-dir", cache_dir.to_str().unwrap()],
    );
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert_eq!(changed.stdout, b"CHANGED VALUE\n");
    assert!(
        !String::from_utf8_lossy(&changed.stderr).contains("cache_hit"),
        "changed input unexpectedly hit cache: {}",
        String::from_utf8_lossy(&changed.stderr)
    );
}

#[test]
fn terminal_named_output_can_feed_a_later_parameter() {
    let (directory, manifests) = fixture();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "run printf with message \"named value\" as greeting\nthen printf with message greeting\n",
        &[],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"named value\n\n");
}

#[test]
fn check_rejects_missing_required_parameter_before_execution() {
    let (directory, manifests) = fixture();
    let result = invoke(directory.path(), &manifests, "check", "run printf\n", &[]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("message"));
    assert!(!directory.path().join("should-not-exist").exists());
}

#[test]
fn validation_rejects_undefined_variables() {
    let (directory, manifests) = fixture();
    let result = invoke(
        directory.path(),
        &manifests,
        "check",
        "run printf with message absent_value\n",
        &[],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("absent_value"));
}

#[test]
fn check_and_explain_do_not_run_or_save_workflow_steps() {
    let (directory, manifests) = fixture();
    let script = "run printf with message \"must stay a plan\"\nthen save to file \"should-not-exist.txt\"\n";
    let check = invoke(directory.path(), &manifests, "check", script, &[]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(!directory.path().join("should-not-exist.txt").exists());

    let explain = invoke(directory.path(), &manifests, "explain", script, &[]);
    assert!(
        explain.status.success(),
        "{}",
        String::from_utf8_lossy(&explain.stderr)
    );
    assert!(!explain.stdout.is_empty());
    assert!(!directory.path().join("should-not-exist.txt").exists());
}

#[test]
fn whole_program_validation_prevents_an_earlier_save() {
    let (directory, manifests) = fixture();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "run printf with message \"valid first step\"\nthen save to file \"early.txt\"\nthen printf with message missing_result\n",
        &[],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("missing_result"));
    assert!(!directory.path().join("early.txt").exists());
}

#[test]
fn nonzero_stage_stops_the_pipeline() {
    let (directory, manifests) = fixture();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "run fail\npipe to printf with message \"must not run\"\n",
        &[],
    );
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
}

#[test]
fn upstream_pipe_failure_prevents_a_later_save() {
    let (directory, manifests) = fixture();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "run fail\npipe to uppercase\nthen save to file \"failed-output.txt\"\n",
        &[],
    );
    assert!(!result.status.success());
    assert!(!directory.path().join("failed-output.txt").exists());
}

#[test]
fn output_limit_fails_without_emitting_partial_stdout() {
    let (directory, manifests) = fixture();
    let script = "run printf with message \"0123456789012345678901234567890123456789\"\nthen save to \"out.txt\"\n";
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        script,
        &["--max-output-bytes", "16"],
    );
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
}

#[test]
fn oversized_native_pipe_group_does_not_save_partial_output() {
    let (directory, manifests) = fixture();
    let input = vec![b'a'; 512 * 1024];
    fs::write(directory.path().join("large.txt"), input).unwrap();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "read file \"large.txt\"\npipe to uppercase\npipe to copy\nthen save to file \"overflow.txt\"\n",
        &["--max-output-bytes", "1024"],
    );
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(!directory.path().join("overflow.txt").exists());
}

#[test]
fn help_and_unknown_options_are_handled() {
    let help = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: execute"));

    let (directory, manifests) = fixture();
    let unknown = invoke(
        directory.path(),
        &manifests,
        "check",
        "run printf with message \"ok\"\n",
        &["--bogus"],
    );
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("--bogus"));
}

#[test]
fn direct_program_invocation_runs_by_default_and_explicit_run_is_supported() {
    let (directory, manifests) = fixture();
    let direct = invoke(
        directory.path(),
        &manifests,
        "default",
        "run printf with message \"direct execution\"\n",
        &[],
    );
    assert!(
        direct.status.success(),
        "{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    assert_eq!(direct.stdout, b"direct execution\n");

    let explicit = invoke(
        directory.path(),
        &manifests,
        "run",
        "run printf with message \"explicit execution\"\n",
        &[],
    );
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    assert_eq!(explicit.stdout, b"explicit execution\n");
}

#[test]
fn every_trigger_stops_after_the_requested_run_count() {
    let (directory, manifests) = fixture();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "every 10 milliseconds\n  run printf with message \"tick\"\n",
        &["--max-runs", "3"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"tick\ntick\ntick\n");
}

#[test]
fn service_limits_must_be_positive_and_are_run_only_options() {
    let (directory, manifests) = fixture();
    let script = "every 10 milliseconds\n  run printf with message \"tick\"\n";
    for option in ["--max-runs", "--max-record-bytes"] {
        let result = invoke(
            directory.path(),
            &manifests,
            "default",
            script,
            &[option, "0"],
        );
        assert!(!result.status.success(), "{option} accepted zero");
        assert!(String::from_utf8_lossy(&result.stderr).contains("greater than zero"));
    }

    let read_only = invoke(
        directory.path(),
        &manifests,
        "check",
        script,
        &["--max-runs", "3"],
    );
    assert!(!read_only.status.success());
}

#[test]
fn trigger_limits_are_rejected_when_the_workflow_cannot_use_them() {
    let (directory, manifests) = fixture();
    let finite = "run printf with message \"complete workflow\"\n";
    let finite_max_runs = invoke(
        directory.path(),
        &manifests,
        "default",
        finite,
        &["--max-runs", "1"],
    );
    assert!(!finite_max_runs.status.success());
    assert!(String::from_utf8_lossy(&finite_max_runs.stderr).contains("triggered programs"));

    let finite_max_record_bytes = invoke(
        directory.path(),
        &manifests,
        "default",
        finite,
        &["--max-record-bytes", "1"],
    );
    assert!(!finite_max_record_bytes.status.success());
    assert!(String::from_utf8_lossy(&finite_max_record_bytes.stderr).contains("listener programs"));

    let scheduled_max_record_bytes = invoke(
        directory.path(),
        &manifests,
        "default",
        "every 10 milliseconds\n  run printf with message \"tick\"\n",
        &["--max-runs", "1", "--max-record-bytes", "1"],
    );
    assert!(!scheduled_max_record_bytes.status.success());
    assert!(
        String::from_utf8_lossy(&scheduled_max_record_bytes.stderr).contains("listener programs")
    );
}

#[test]
fn applet_listener_processes_two_source_records() {
    let (directory, manifests) = fixture();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "listen to printf with message \"first\\nsecond\"\nwhenever a line arrives as record\n  run printf with message record\n",
        &["--max-runs", "2"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"first\n\nsecond\n\n");
}

#[test]
fn cd_prelude_sets_working_directory_for_listener_handlers() {
    let (directory, manifests) = fixture();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let expected_directory = fs::canonicalize(&nested).unwrap();
    let caller_directory = std::env::current_dir().unwrap();
    let result = invoke(
        directory.path(),
        &manifests,
        "default",
        "run cd with path \"nested\"\nlisten to printf with message \"record\"\nwhenever a line arrives as record\n  run pwd\n",
        &["--max-runs", "1"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        format!("{}\n", expected_directory.display()).as_bytes()
    );
    assert_eq!(std::env::current_dir().unwrap(), caller_directory);
}

#[cfg(unix)]
mod process_group_tests {
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    struct RunnerCleanup {
        runner: Option<Child>,
        group_id_files: Vec<PathBuf>,
        active: bool,
    }

    impl RunnerCleanup {
        fn kill_all(&mut self) {
            if !self.active {
                return;
            }
            for group_id_file in &self.group_id_files {
                if let Some(group_id) = read_pid(group_id_file) {
                    // SAFETY: kill takes only integers and touches no memory. `group_id` is the
                    // pid the applet shell wrote with `echo $$` (see start_runner). The runner
                    // starts applets with setpgid(0, 0) in src/runtime/unix_process.rs, so
                    // -group_id names that group. read_pid does not reject 0 or negative
                    // values; a 0 here would signal the test's own process group.
                    unsafe {
                        libc::kill(-(group_id as libc::pid_t), libc::SIGKILL);
                    }
                }
            }
            if let Some(runner) = &mut self.runner {
                if runner.try_wait().ok().flatten().is_none() {
                    let _ = runner.kill();
                    let _ = runner.wait();
                }
            }
            self.active = false;
        }
    }

    impl Drop for RunnerCleanup {
        fn drop(&mut self) {
            self.kill_all();
        }
    }

    fn read_pid(path: &Path) -> Option<i32> {
        fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    fn wait_for_pid(path: &Path, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pid) = read_pid(path) {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn process_exists(pid: i32) -> bool {
        // SAFETY: kill with signal 0 sends nothing; it only checks whether `pid` exists and
        // takes no pointers. `pid` is read from a pid file that this test's own shell script
        // wrote with `echo $$`, so it is an integer naming one process.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn wait_for_group_exit(group_id: i32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: signal 0 to a negative pid probes the process group `group_id` and
            // delivers nothing; the call only takes integers. `group_id` comes from a file the
            // test's script wrote with `echo $$`, the applet shell that leads its own group.
            // read_pid does not check that the value is positive.
            let result = unsafe { libc::kill(-(group_id as libc::pid_t), 0) };
            if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_runner(runner: &mut Child, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = runner.try_wait().expect("polling flow process") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn start_runner(wait_for_child: bool, scheduled: bool) -> (TempDir, RunnerCleanup, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let control = directory.path().join("control");
        let manifests = directory.path().join("manifests");
        fs::create_dir_all(&control).unwrap();
        fs::create_dir_all(&manifests).unwrap();

        let wrapper = directory.path().join("sleeper.sh");
        let group_id_file = control.join("group.pid");
        let sleeper_pid_file = control.join("sleeper.pid");
        // In the parent-exit case, keep the wrapper alive briefly after the
        // sleeper PID is recorded so the test can observe the live child before
        // the wrapper exits and leaves stdout open in its descendant.
        let wait_command = if wait_for_child {
            "wait"
        } else {
            "sleep 0.2\nexit 0"
        };
        let wrapper_source = format!(
            "#!/bin/sh\necho $$ > '{}'\nsleep 30 &\necho $! > '{}'\n{}\n",
            group_id_file.display(),
            sleeper_pid_file.display(),
            wait_command,
        );
        fs::write(&wrapper, wrapper_source).unwrap();
        let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&wrapper, permissions).unwrap();

        let manifest = serde_json::json!({
            "name": "sleeper",
            "binary": wrapper,
            "arguments": []
        });
        fs::write(
            manifests.join("sleeper.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let workflow = directory.path().join("workflow.program");
        let workflow_source = if scheduled {
            "every 10 milliseconds\n  run sleeper\n"
        } else {
            "run sleeper\n"
        };
        fs::write(&workflow, workflow_source).unwrap();

        let runner = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .arg(&workflow)
            .arg("--manifest-dir")
            .arg(&manifests)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let cleanup = RunnerCleanup {
            runner: Some(runner),
            group_id_files: vec![group_id_file],
            active: true,
        };
        (directory, cleanup, sleeper_pid_file)
    }

    fn write_executable(path: &Path, source: &str) {
        fs::write(path, source).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn write_manifest(manifests: &Path, filename: &str, manifest: serde_json::Value) {
        fs::write(
            manifests.join(filename),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn marker_applet(directory: &TempDir) -> (PathBuf, PathBuf) {
        let manifests = directory.path().join("marker-manifests");
        fs::create_dir_all(&manifests).unwrap();
        let marker = directory.path().join("producer-started");
        let wrapper = directory.path().join("marker-source.sh");
        write_executable(
            &wrapper,
            &format!(
                "#!/bin/sh\necho launched > '{}'\nprintf 'event\\n'\n",
                marker.display()
            ),
        );
        write_manifest(
            &manifests,
            "marker.json",
            serde_json::json!({
                "name": "marker source",
                "binary": wrapper,
                "parameters": { "token": { "type": "string", "required": true } },
                "arguments": []
            }),
        );
        (manifests, marker)
    }

    fn start_listener_service() -> (TempDir, RunnerCleanup, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let control = directory.path().join("control");
        let manifests = directory.path().join("manifests");
        fs::create_dir_all(&control).unwrap();
        fs::create_dir_all(&manifests).unwrap();

        let source_group_file = control.join("source-group.pid");
        let source_wrapper = directory.path().join("source.sh");
        write_executable(
            &source_wrapper,
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nprintf 'event\\n'\nsleep 30\n",
                source_group_file.display()
            ),
        );
        write_manifest(
            &manifests,
            "source.json",
            serde_json::json!({
                "name": "source",
                "binary": source_wrapper,
                "parameters": { "token": { "type": "string", "required": true } },
                "arguments": []
            }),
        );

        let body_group_file = control.join("body-group.pid");
        let sleeper_pid_file = control.join("body-sleeper.pid");
        let body_wrapper = directory.path().join("body.sh");
        write_executable(
            &body_wrapper,
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nsleep 30 &\necho $! > '{}'\nwait\n",
                body_group_file.display(),
                sleeper_pid_file.display()
            ),
        );
        write_manifest(
            &manifests,
            "sleeper.json",
            serde_json::json!({
                "name": "sleeper",
                "binary": body_wrapper,
                "arguments": []
            }),
        );

        let workflow = directory.path().join("listener.program");
        fs::write(
            &workflow,
            "listen to source with token \"launch\"\nwhenever a line arrives as record\n  run sleeper\n",
        )
        .unwrap();
        let runner = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .arg(&workflow)
            .arg("--manifest-dir")
            .arg(&manifests)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let cleanup = RunnerCleanup {
            runner: Some(runner),
            group_id_files: vec![source_group_file.clone(), body_group_file.clone()],
            active: true,
        };
        (directory, cleanup, source_group_file, sleeper_pid_file)
    }

    #[test]
    fn stdin_listener_emits_two_records_before_stdin_closes() {
        use std::sync::mpsc;

        let (directory, mut cleanup) = start_stdin_listener();
        let expected = b"alpha\n\nbeta\n\n".to_vec();
        let expected_len = expected.len();
        let mut stdout = cleanup.runner.as_mut().unwrap().stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut received = vec![0; expected_len];
            let outcome = stdout.read_exact(&mut received).map(|()| received);
            let _ = sender.send(outcome);
        });
        let stdin = cleanup.runner.as_mut().unwrap().stdin.as_mut().unwrap();
        stdin.write_all(b"alpha\nbeta\n").unwrap();
        stdin.flush().unwrap();
        let output = receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("listener did not emit while stdin remained open")
            .expect("reading listener output");
        assert_eq!(output, expected);
        assert!(
            cleanup
                .runner
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none(),
            "listener exited before stdin closed"
        );

        drop(cleanup.runner.as_mut().unwrap().stdin.take());
        let status = wait_runner(cleanup.runner.as_mut().unwrap(), Duration::from_secs(3));
        assert!(status.is_some(), "listener did not stop after stdin EOF");
        assert!(status.unwrap().success());
        cleanup.active = false;
        drop(directory);
    }

    #[test]
    fn stdin_listener_emits_a_final_record_without_a_newline() {
        let (_directory, mut cleanup) = start_stdin_listener();
        cleanup
            .runner
            .as_mut()
            .unwrap()
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"final-record")
            .unwrap();
        drop(cleanup.runner.as_mut().unwrap().stdin.take());

        let status = wait_runner(cleanup.runner.as_mut().unwrap(), Duration::from_secs(3));
        assert!(status.is_some(), "listener did not stop after stdin EOF");
        assert!(status.unwrap().success());
        let mut output = Vec::new();
        cleanup
            .runner
            .as_mut()
            .unwrap()
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut output)
            .unwrap();
        assert_eq!(output, b"final-record\n");
        cleanup.active = false;
    }

    #[test]
    fn listener_cache_key_tracks_the_current_event_binding() {
        let directory = tempfile::tempdir().unwrap();
        let manifests = directory.path().join("manifests");
        fs::create_dir(&manifests).unwrap();
        write_manifest(
            &manifests,
            "printf.json",
            serde_json::json!({
                "name": "printf",
                "binary": "printf",
                "cacheable": true,
                "parameters": {
                    "message": { "type": "string", "required": true }
                },
                "arguments": [
                    { "literal": "%s\\n" },
                    { "parameter": "message" }
                ]
            }),
        );
        let workflow = directory.path().join("listener.program");
        fs::write(
            &workflow,
            "listen to stdin\nwhenever a line arrives as record\n  run printf with message record and cache\n",
        )
        .unwrap();
        let cache = directory.path().join("cache");
        let trace_path = directory.path().join("trace.log");
        let child = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .arg(&workflow)
            .arg("--manifest-dir")
            .arg(&manifests)
            .arg("--cache-dir")
            .arg(cache)
            .arg("--trace")
            .arg("--max-runs")
            .arg("3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(fs::File::create(&trace_path).unwrap()))
            .spawn()
            .unwrap();
        let mut cleanup = RunnerCleanup {
            runner: Some(child),
            group_id_files: Vec::new(),
            active: true,
        };
        cleanup
            .runner
            .as_mut()
            .unwrap()
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"alpha\nbeta\nalpha\n")
            .unwrap();
        drop(cleanup.runner.as_mut().unwrap().stdin.take());
        let status = wait_runner(cleanup.runner.as_mut().unwrap(), Duration::from_secs(3));
        assert!(
            status.is_some(),
            "cached listener did not stop after max runs"
        );
        assert!(status.unwrap().success());
        let mut output = Vec::new();
        cleanup
            .runner
            .as_mut()
            .unwrap()
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut output)
            .unwrap();
        let trace = fs::read(&trace_path).unwrap();
        assert_eq!(output, b"alpha\n\nbeta\n\nalpha\n\n");
        assert_eq!(
            String::from_utf8_lossy(&trace).matches("cache_hit").count(),
            1,
            "expected only the repeated alpha event to hit cache: {}",
            String::from_utf8_lossy(&trace)
        );
        cleanup.active = false;
    }

    fn start_stdin_listener() -> (TempDir, RunnerCleanup) {
        let directory = tempfile::tempdir().unwrap();
        let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let workflow = directory.path().join("stdin.program");
        fs::write(
            &workflow,
            "listen to stdin\nwhenever a line arrives as record\n  run printf with message record\n",
        )
        .unwrap();
        let runner = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .arg(&workflow)
            .arg("--manifest-dir")
            .arg(manifests)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let cleanup = RunnerCleanup {
            runner: Some(runner),
            group_id_files: Vec::new(),
            active: true,
        };
        (directory, cleanup)
    }

    #[test]
    fn stdin_listener_rejects_an_oversized_record() {
        let directory = tempfile::tempdir().unwrap();
        let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let workflow = directory.path().join("oversized.program");
        fs::write(
            &workflow,
            "listen to stdin\nwhenever a line arrives as record\n  run printf with message record\n",
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .arg(&workflow)
            .arg("--manifest-dir")
            .arg(manifests)
            .arg("--max-record-bytes")
            .arg("4")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut cleanup = RunnerCleanup {
            runner: Some(child),
            group_id_files: Vec::new(),
            active: true,
        };
        cleanup
            .runner
            .as_mut()
            .unwrap()
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"oversized\n")
            .unwrap();
        drop(cleanup.runner.as_mut().unwrap().stdin.take());
        let status = wait_runner(cleanup.runner.as_mut().unwrap(), Duration::from_secs(3));
        assert!(
            status.is_some(),
            "oversized record did not stop the listener"
        );
        assert!(!status.unwrap().success());
        let stdout = cleanup.runner.as_mut().unwrap().stdout.take().unwrap();
        let stderr = cleanup.runner.as_mut().unwrap().stderr.take().unwrap();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        stdout.take(4096).read_to_end(&mut output).unwrap();
        stderr.take(4096).read_to_end(&mut errors).unwrap();
        assert!(output.is_empty());
        assert!(String::from_utf8_lossy(&errors)
            .to_lowercase()
            .contains("record"));
        cleanup.active = false;
    }

    #[test]
    fn malformed_cron_check_does_not_start_an_applet() {
        let directory = tempfile::tempdir().unwrap();
        let (manifests, marker) = marker_applet(&directory);
        let result = super::invoke(
            directory.path(),
            &manifests,
            "check",
            "on schedule \"not a cron expression\"\n  run marker source with token \"check\"\n",
            &[],
        );
        assert!(!result.status.success());
        assert!(!marker.exists(), "check started a producer applet");
    }

    #[test]
    fn invalid_listener_body_prevents_source_applet_launch() {
        let directory = tempfile::tempdir().unwrap();
        let (manifests, marker) = marker_applet(&directory);
        let result = super::invoke(
            directory.path(),
            &manifests,
            "default",
            "listen to marker source with token \"launch\"\nwhenever a line arrives as record\n  run marker source with token undefined_record\n",
            &[],
        );
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("undefined_record"),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            !marker.exists(),
            "invalid listener body launched its source applet"
        );
    }

    #[test]
    fn sigterm_cancels_listener_and_kills_source_and_body_groups() {
        let (_directory, mut cleanup, source_group_file, sleeper_pid_file) =
            start_listener_service();
        let source_group = wait_for_pid(&source_group_file, Duration::from_secs(3));
        let sleeper_pid = wait_for_pid(&sleeper_pid_file, Duration::from_secs(3));
        let body_group = wait_for_pid(&cleanup.group_id_files[1], Duration::from_secs(1));
        assert!(
            process_exists(sleeper_pid),
            "listener body sleeper was not active"
        );

        let runner_pid = cleanup.runner.as_ref().unwrap().id() as libc::pid_t;
        // SAFETY: kill takes only integers. `runner_pid` is the id of the runner spawned in
        // start_listener_service (line 885) and nothing has waited on it yet, so the pid still
        // names that child and has not been reused for another process.
        assert_eq!(unsafe { libc::kill(runner_pid, libc::SIGTERM) }, 0);
        let status = wait_runner(cleanup.runner.as_mut().unwrap(), Duration::from_secs(5));
        assert!(
            status.is_some(),
            "listener runner did not exit after SIGTERM"
        );
        assert!(!status.unwrap().success());
        assert!(wait_for_group_exit(source_group, Duration::from_secs(3)));
        assert!(wait_for_group_exit(body_group, Duration::from_secs(3)));
        cleanup.active = false;
    }

    #[test]
    fn sigterm_cancels_listener_when_service_stdout_pipe_is_full() {
        let directory = tempfile::tempdir().unwrap();
        let manifests = directory.path().join("manifests");
        let control = directory.path().join("control");
        fs::create_dir_all(&manifests).unwrap();
        fs::create_dir_all(&control).unwrap();
        let group_file = control.join("source-group.pid");
        let source = directory.path().join("source.sh");
        write_executable(
            &source,
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nprintf 'event\\n'\nsleep 30\n",
                group_file.display()
            ),
        );
        write_manifest(
            &manifests,
            "source.json",
            serde_json::json!({
                "name": "source",
                "binary": source,
                "parameters": { "token": { "type": "string", "required": true } },
                "arguments": []
            }),
        );
        fs::write(
            directory.path().join("large.txt"),
            vec![b'x'; 4 * 1024 * 1024],
        )
        .unwrap();
        let workflow = directory.path().join("listener.program");
        fs::write(
            &workflow,
            "listen to source with token \"launch\"\nwhenever a line arrives as record\n  read file \"large.txt\"\n",
        )
        .unwrap();
        let runner = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .arg(&workflow)
            .arg("--manifest-dir")
            .arg(&manifests)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut cleanup = RunnerCleanup {
            runner: Some(runner),
            group_id_files: vec![group_file.clone()],
            active: true,
        };
        let source_group = wait_for_pid(&group_file, Duration::from_secs(3));
        thread::sleep(Duration::from_millis(300));
        assert!(
            cleanup
                .runner
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none(),
            "listener unexpectedly exited before cancellation"
        );

        let runner_pid = cleanup.runner.as_ref().unwrap().id() as libc::pid_t;
        // SAFETY: kill takes only integers. `runner_pid` is the id of the runner spawned at
        // line 1246; the try_wait at lines 1255-1264 returned None, so the child has not
        // exited or been reaped and the pid still names it.
        assert_eq!(unsafe { libc::kill(runner_pid, libc::SIGTERM) }, 0);
        let status = wait_runner(cleanup.runner.as_mut().unwrap(), Duration::from_secs(3));
        assert!(
            status.is_some(),
            "runner did not exit after SIGTERM with full stdout pipe"
        );
        assert!(!status.unwrap().success());
        assert!(wait_for_group_exit(source_group, Duration::from_secs(3)));
        cleanup.active = false;
    }

    #[test]
    fn exits_promptly_and_kills_background_child_after_applet_parent_exits() {
        let (_directory, mut cleanup, sleeper_pid_file) = start_runner(false, false);
        let sleeper_pid = wait_for_pid(&sleeper_pid_file, Duration::from_secs(3));
        let group_id = wait_for_pid(&cleanup.group_id_files[0], Duration::from_secs(1));
        assert!(
            process_exists(sleeper_pid),
            "background sleeper was not active"
        );

        let started = Instant::now();
        let status = wait_runner(cleanup.runner.as_mut().unwrap(), Duration::from_secs(5));
        assert!(
            status.is_some(),
            "flow did not finish promptly after applet parent exit"
        );
        assert!(status.unwrap().success(), "flow reported an applet failure");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            wait_for_group_exit(group_id, Duration::from_secs(3)),
            "applet process group remained after runner exit"
        );
        cleanup.active = false;
    }

    #[test]
    fn sigterm_cancels_scheduled_service_and_terminates_its_applet_group() {
        let (_directory, mut cleanup, sleeper_pid_file) = start_runner(true, true);
        let sleeper_pid = wait_for_pid(&sleeper_pid_file, Duration::from_secs(3));
        let group_id = wait_for_pid(&cleanup.group_id_files[0], Duration::from_secs(1));
        assert!(
            process_exists(sleeper_pid),
            "background sleeper was not active"
        );

        let runner_pid = cleanup.runner.as_ref().unwrap().id() as libc::pid_t;
        // SAFETY: kill takes only integers. `runner_pid` is the id of the runner spawned by
        // start_runner (line 774), which has not been waited on, so the pid still names it.
        let signal_result = unsafe { libc::kill(runner_pid, libc::SIGTERM) };
        assert_eq!(signal_result, 0, "sending SIGTERM to flow runner");

        let status = wait_runner(cleanup.runner.as_mut().unwrap(), Duration::from_secs(5));
        assert!(status.is_some(), "flow did not exit after SIGTERM");
        assert!(
            !status.unwrap().success(),
            "cancelled workflow unexpectedly succeeded"
        );
        assert!(
            wait_for_group_exit(group_id, Duration::from_secs(3)),
            "applet process group remained after runner cancellation"
        );
        cleanup.active = false;
    }

    #[test]
    fn sigterm_cancels_scheduled_service_when_trace_stderr_pipe_is_full() {
        let directory = tempfile::tempdir().unwrap();
        let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        fs::write(directory.path().join("tiny.txt"), "tiny\n").unwrap();
        let workflow = directory.path().join("trace.program");
        fs::write(&workflow, "every 1 millisecond\n  read file \"tiny.txt\"\n").unwrap();
        let runner = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .arg("run")
            .arg(&workflow)
            .arg("--manifest-dir")
            .arg(manifests)
            .arg("--drain-timeout-ms")
            .arg("500")
            .arg("--trace")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut cleanup = RunnerCleanup {
            runner: Some(runner),
            group_id_files: Vec::new(),
            active: true,
        };
        thread::sleep(Duration::from_millis(500));
        assert!(
            cleanup
                .runner
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none(),
            "scheduled trace workflow exited before cancellation"
        );

        let runner_pid = cleanup.runner.as_ref().unwrap().id() as libc::pid_t;
        // SAFETY: kill takes only integers. `runner_pid` is the id of the runner spawned at
        // line 1350; the try_wait at lines 1358-1367 returned None, so the child has not
        // exited or been reaped and the pid still names it.
        assert_eq!(unsafe { libc::kill(runner_pid, libc::SIGTERM) }, 0);
        let status = wait_runner(cleanup.runner.as_mut().unwrap(), Duration::from_secs(3));
        assert!(
            status.is_some(),
            "runner did not exit after SIGTERM with full trace stderr pipe"
        );
        assert!(!status.unwrap().success());
        cleanup.active = false;
    }
}

#[test]
fn help_and_version_cannot_replace_required_option_values() {
    let (directory, _) = fixture();
    for value in ["--help", "--version", "-h", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .args(["unused.program", "--manifest-dir", value])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--manifest-dir requires a directory")
        );
    }
}

#[test]
fn installed_symlink_resolves_manifest_bundle_without_the_build_checkout() {
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("bundle");
    fs::create_dir_all(bundle.join("bin")).unwrap();
    fs::create_dir(bundle.join("manifests")).unwrap();
    let binary = bundle.join("bin/execute");
    fs::copy(env!("CARGO_BIN_EXE_execute"), &binary).unwrap();
    fs::write(bundle.join("manifests/probe.json"),r#"{"name":"installation.probe","binary":"printf","arguments":[{"literal":"%s\n"},{"literal":"installed bundle"}]}"#).unwrap();
    let command = directory.path().join("execute");
    std::os::unix::fs::symlink(&binary, &command).unwrap();
    let script = directory.path().join("my.program");
    fs::write(&script, "run installation.probe\n").unwrap();
    let output = Command::new(&command)
        .arg(&script)
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"installed bundle\n");
}

fn local_file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut result = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(byte) {
            result.push(*byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}
#[test]
fn file_uri_runs_the_same_script_and_supports_localhost_and_percent_escapes() {
    let (directory, manifests) = fixture();
    let script = directory
        .path()
        .join("space 東京 + percent% # question?.program");
    fs::write(&script, "run printf with message \"URI works\"\n").unwrap();
    let uri = local_file_uri(&script);
    for argument in [
        script.to_string_lossy().into_owned(),
        uri.clone(),
        uri.replacen("file://", "file://localhost", 1),
        uri.replacen("file:", "FILE:", 1),
        uri.replacen("file://", "file:", 1),
    ] {
        for mode in ["default", "check", "explain"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
            if mode != "default" {
                command.arg(mode);
            }
            let output = command
                .arg(&argument)
                .arg("--manifest-dir")
                .arg(&manifests)
                .current_dir(directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{argument}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if mode == "default" {
                assert_eq!(output.stdout, b"URI works\n");
            }
        }
    }
}
#[test]
fn file_uri_preserves_literal_plus_and_non_utf8_unix_filename_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let (directory, manifests) = fixture();
    // APFS rejects non-UTF-8 names; Linux filesystems can represent them.
    let names = if cfg!(target_os = "linux") {
        vec![
            b"plus+name.program".to_vec(),
            b"nonutf8-\xff.program".to_vec(),
        ]
    } else {
        vec![b"plus+name.program".to_vec()]
    };
    for name in names {
        let script = directory.path().join(std::ffi::OsString::from_vec(name));
        fs::write(
            &script,
            "run printf with message \"filename bytes preserved\"\n",
        )
        .unwrap();
        let uri = local_file_uri(&script).replace("%2B", "+");
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(uri)
            .arg("--manifest-dir")
            .arg(&manifests)
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"filename bytes preserved\n");
    }
}
#[test]
fn invalid_or_remote_file_uris_fail_with_specific_diagnostics() {
    let directory = tempfile::tempdir().unwrap();
    for (uri, expected) in [
        ("file://remote.example/program", "remote host"),
        ("file:relative.program", "absolute path"),
        ("file:///tmp/bad%GG.program", "percent escape"),
        ("file:///tmp/bad%.program", "percent escape"),
        ("file:///tmp/bad%00.program", "NUL"),
        ("file:///tmp/a.program?query", "queries and fragments"),
        ("file:///tmp/a.program#fragment", "queries and fragments"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(uri)
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn invalid_utf8_cli_argument_is_a_normal_error_instead_of_a_panic() {
    use std::os::unix::ffi::OsStringExt;
    let (directory, _) = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(std::ffi::OsString::from_vec(
            b"missing-\xff.program".to_vec(),
        ))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("not valid UTF-8"), "{error}");
    assert!(!error.contains("panicked"), "{error}");
}

#[test]
fn percent_encoded_non_utf8_workflow_path_is_handled_without_panicking() {
    use std::os::unix::ffi::OsStringExt;
    let (directory, manifests) = fixture();
    let path = directory
        .path()
        .join(std::ffi::OsString::from_vec(b"raw-\xff.program".to_vec()));
    let written = fs::write(&path, "run printf with message \"literal bytes path\"\n");
    let uri = format!("file://{}/raw-%FF.program", directory.path().display());
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(uri)
        .arg("--manifest-dir")
        .arg(manifests)
        .output()
        .unwrap();
    if let Err(error) = written {
        assert_eq!(error.raw_os_error(), Some(libc::EILSEQ), "{error}");
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("could not read workflow"));
    } else {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"literal bytes path\n");
    }
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
}

#[test]
fn closed_stdout_is_a_normal_error_for_cli_and_live_responses() {
    use std::process::Stdio;
    let (directory, manifests) = fixture();
    let script = directory.path().join("workflow.program");
    fs::write(&script, "run pwd\n").unwrap();
    let screen = directory.path().join("screen.program");
    fs::write(&screen, "open a screen titled \"closed output\"\nplace app on the left\ndefine a program named app\n  show a text panel named status with text \"ready\"\n").unwrap();
    let script = script.to_str().unwrap();
    let screen = screen.to_str().unwrap();
    for arguments in [
        vec!["--version"],
        vec!["--help"],
        vec!["explain", script],
        vec![screen, "--headless"],
        vec!["live", "schema"],
        vec!["live", "read", "--socket", "does-not-exist.sock"],
    ] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command.current_dir(directory.path()).args(&arguments);
        if arguments[0] != "live" {
            command.arg("--manifest-dir").arg(&manifests);
        }
        let output = command.stdout(Stdio::from(writer)).output().unwrap();
        // A command that writes to stdout exits 141 quietly when the reader is gone.
        let expected = 141;
        assert_eq!(output.status.code(), Some(expected), "{arguments:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("panicked"),
            "{arguments:?}: {:?}",
            output.stderr
        );
    }
}
