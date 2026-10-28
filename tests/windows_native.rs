#![cfg(windows)]
//! Real native executables, files, pipes, jobs and named-pipe state control.
#[cfg(feature = "live")]
use std::io::{BufRead, BufReader};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};
fn fixture() -> String {
    env!("CARGO_BIN_EXE_native-fixture").to_owned()
}
fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}
fn invoke(folder: &Path, source: &str, options: &[&str]) -> Output {
    let file = folder.join("job.program");
    fs::write(&file, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(file)
        .args(options)
        .current_dir(folder)
        .output()
        .unwrap()
}
fn ok(result: &Output) {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn native(args: &[&str]) -> String {
    format!(
        "run {} with arguments {}\n",
        quoted(&fixture()),
        serde_json::to_string(args).unwrap()
    )
}
fn find_msys_printf(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| {
        path.is_file()
            && path
                .parent()
                .is_some_and(|directory| directory.join("msys-2.0.dll").is_file())
    })
}
fn git_for_windows_printf() -> Option<PathBuf> {
    let candidates = ["ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|root| Path::new(&root).join("Git/usr/bin/printf.exe"))
        .chain(
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|directory| directory.join("printf.exe")),
        );
    find_msys_printf(candidates.collect::<Vec<_>>())
}

#[test]
fn native_argv_is_literal_unicode_and_round_trips_windows_quoting() {
    let folder = tempfile::tempdir().unwrap();
    let values = [
        "",
        "a b",
        "雪",
        "trailing\\",
        "quote\"slash\\",
        "%PATH%",
        "$(touch forbidden); & | >",
        "--help",
        "a\nb",
        "x\ny\n",
        "a\rb",
        "a\u{b}b",
        "a\u{c}b",
    ];
    let mut args = vec!["argv"];
    args.extend(values);
    let output = invoke(folder.path(), &native(&args), &[]);
    ok(&output);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!(values)
    );
    assert!(!folder.path().join("forbidden").exists());
}

#[test]
fn git_for_windows_printf_keeps_newline_inside_one_argument() {
    let git_printf = git_for_windows_printf();
    let Some(git_printf) = git_printf else {
        if std::env::var_os("CI").is_some() {
            panic!(
                "CI requires Git for Windows MSYS printf.exe with sibling msys-2.0.dll; see the Windows workflow fixture-verification step"
            );
        }
        eprintln!("skipping MSYS regression: Git for Windows printf.exe was not found outside CI");
        return;
    };

    let folder = tempfile::tempdir().unwrap();
    let source = format!(
        "run {} with arguments [{}]\n",
        quoted(git_printf.to_str().unwrap()),
        quoted("x\ny\n")
    );
    let output = invoke(folder.path(), &source, &[]);
    ok(&output);
    assert_eq!(output.stdout, b"x\ny\n");
}

#[test]
fn msys_printf_lookup_rejects_unrelated_printf_executables() {
    let folder = tempfile::tempdir().unwrap();
    let printf = folder.path().join("printf.exe");
    fs::write(&printf, b"not an executable").unwrap();
    assert_eq!(find_msys_printf([printf.clone()]), None);

    fs::write(folder.path().join("msys-2.0.dll"), b"MSYS runtime marker").unwrap();
    assert_eq!(find_msys_printf([printf.clone()]), Some(printf));
}
#[test]
fn direct_byte_pipelines_and_inherited_stdin_preserve_binary_data() {
    let folder = tempfile::tempdir().unwrap();
    let file = folder.path().join("binary.program");
    fs::write(
        &file,
        format!(
            "read from stdin\npipe to {0} with arguments [\"copy\"]\npipe to {0} with arguments [\"copy\"]\n",
            quoted(&fixture())
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let data: Vec<u8> = (0..256).cycle().take(131072).map(|n| n as u8).collect();
    let expected = data.clone();
    let mut sink = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || sink.write_all(&data));
    let result = child.wait_with_output().unwrap();
    ok(&result);
    writer.join().unwrap().unwrap();
    assert_eq!(result.stdout, expected);
}
#[test]
fn typed_process_inputs_json_conditions_reuse_and_file_replace_share_the_runtime() {
    let folder = tempfile::tempdir().unwrap();
    let exe = quoted(&fixture());
    // This synthetic Unicode fixture is public test data; declare that explicitly so
    // it can exercise environment-to-reusable-program-to-argv forwarding.
    let source=format!("accept message as text from public environment \"EXECUTE_WINDOWS_TEXT\"\naccept count as integer from argument 1\ndefine a program named echo\n  accept text as text\n  run {exe} with arguments [\"write\", text]\nif count is 3\n  run program echo with text message\notherwise\n  run {exe} with arguments [\"write\", \"wrong\"]\nthen save to \"result.txt\"\n");
    let path = folder.path().join("typed.program");
    fs::write(&path, source).unwrap();
    fs::write(folder.path().join("result.txt"), b"old").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(&path)
        .args(["--", "3"])
        .env("EXECUTE_WINDOWS_TEXT", "雪; literal")
        .current_dir(folder.path())
        .output()
        .unwrap();
    ok(&output);
    assert_eq!(
        fs::read(folder.path().join("result.txt")).unwrap(),
        "雪; literal".as_bytes()
    );
    let source=format!("{}then parse json as value\ntake integer at \"/n\" from value as count\nthen encode count as json\n",native(&["write","{\"n\":42}"]));
    let output = invoke(folder.path(), &source, &[]);
    ok(&output);
    assert_eq!(serde_json::from_slice::<i64>(&output.stdout).unwrap(), 42);
}
#[test]
fn real_native_statuses_propagate_or_can_be_handled() {
    let folder = tempfile::tempdir().unwrap();
    let failed = invoke(folder.path(), &native(&["exit", "42", "partial"]), &[]);
    assert_eq!(failed.status.code(), Some(42));
    // Unconsumed output streams as the command writes it, even when the command fails.
    assert_eq!(String::from_utf8_lossy(&failed.stdout), "partial");
    let source = format!(
        "{} and keep status as code\nthen encode code as json\n",
        native(&["exit", "42", ""]).trim_end()
    );
    let output = invoke(folder.path(), &source, &[]);
    ok(&output);
    assert_eq!(serde_json::from_slice::<i64>(&output.stdout).unwrap(), 42);
}

#[test]
fn producer_broken_pipe_after_early_reader_exit_is_pipeline_success() {
    let folder = tempfile::tempdir().unwrap();
    for status in ["120", "141", "1"] {
        let source = format!(
            "{}\npipe to {} with arguments [\"first-line\"]\n",
            native(&["flood-lines", status]).trim_end(),
            quoted(&fixture())
        );
        let result = invoke(folder.path(), &source, &[]);
        ok(&result);
        assert_eq!(result.stdout, b"first\n", "producer status {status}");
    }
}

#[test]
fn early_windows_reader_close_is_recorded_on_the_producer_stage() {
    let folder = tempfile::tempdir().unwrap();
    let source = format!(
        "{}\npipe to {} with arguments [\"first-line\"] and keep result as execution\nthen encode execution as json\n",
        native(&["flood-lines", "120"]).trim_end(),
        quoted(&fixture())
    );
    let result = invoke(folder.path(), &source, &[]);
    ok(&result);
    let execution: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(execution["stages"][0]["broken_pipe"], true);
}

#[test]
fn producer_failure_before_reader_closes_still_fails_pipeline() {
    let folder = tempfile::tempdir().unwrap();
    let source = format!(
        "{}\npipe to {} with arguments [\"delayed-copy\"]\n",
        native(&["exit", "42", "partial"]).trim_end(),
        quoted(&fixture())
    );
    let result = invoke(folder.path(), &source, &[]);
    assert_eq!(result.status.code(), Some(42));
    assert!(String::from_utf8_lossy(&result.stderr).contains("failed (exit code: 42)"));
}

#[test]
fn producer_python_120_before_reader_close_still_fails_pipeline() {
    let folder = tempfile::tempdir().unwrap();
    let source = format!(
        "{}\npipe to {} with arguments [\"delayed-copy\"]\n",
        native(&["exit", "120", "partial"]).trim_end(),
        quoted(&fixture())
    );
    let result = invoke(folder.path(), &source, &[]);
    assert_eq!(result.status.code(), Some(120));
    assert!(String::from_utf8_lossy(&result.stderr).contains("failed (exit code: 120)"));
}

#[test]
fn unrelated_failure_after_early_reader_close_still_fails_pipeline() {
    let folder = tempfile::tempdir().unwrap();
    let source = format!(
        "{}\npipe to {} with arguments [\"first-line\"]\n",
        native(&["flood-independent-failure"]).trim_end(),
        quoted(&fixture())
    );
    let result = invoke(folder.path(), &source, &[]);
    assert_eq!(result.status.code(), Some(42));
    assert!(String::from_utf8_lossy(&result.stderr).contains("failed (exit code: 42)"));
}

#[test]
fn native_stderr_discard_preserves_child_status_result_and_interpreter_diagnostics() {
    let folder = tempfile::tempdir().unwrap();
    let source = format!(
        "{} and discard stderr and keep status as code and keep result as execution\nthen encode execution as json\n",
        native(&["exit", "7", "kept"]).trim_end()
    );
    let handled = invoke(folder.path(), &source, &[]);
    ok(&handled);
    let result: serde_json::Value = serde_json::from_slice(&handled.stdout).unwrap();
    assert_eq!(result["status"], 7);
    assert_eq!(result["stdout_captured"], true);
    assert_eq!(result["stdout"], serde_json::json!(b"kept"));
    assert_eq!(result["stderr_captured"], false);
    assert_eq!(result["stderr"], serde_json::json!([]));
    assert!(!String::from_utf8_lossy(&handled.stderr).contains("fixture failed"));

    let unhandled = invoke(
        folder.path(),
        &format!(
            "{} and discard stderr\n",
            native(&["exit", "7", ""]).trim_end()
        ),
        &[],
    );
    assert_eq!(unhandled.status.code(), Some(7));
    let diagnostic = String::from_utf8_lossy(&unhandled.stderr);
    assert!(diagnostic.contains("failed (exit code: 7)"), "{diagnostic}");
    assert!(diagnostic.starts_with("execute: line 1:"), "{diagnostic}");
    assert!(!diagnostic.contains("fixture failed"), "{diagnostic}");
}

#[test]
fn repeat_foreach_and_while_use_native_windows_applets_and_file_paths() {
    let folder = tempfile::tempdir().unwrap();
    let exe = quoted(&fixture());
    let source = format!(
        "define a program named uppercase\n  repeat 2 times\n    then {exe} with arguments [\"upper\"]\nrun {exe} with arguments [\"write\", \"letters\"]\nrepeat 2 times\n  then program uppercase\nfor each outputpath in [\"one space.txt\", \"雪.txt\"]\n  then save to outputpath\nrun {exe} with arguments [\"exit\", \"0\", \"\"] and keep status as code\nwhile code is 0\n  run {exe} with arguments [\"exit\", \"1\", \"\"] and keep status as code\nthen encode code as json\n"
    );
    let output = invoke(folder.path(), &source, &[]);
    ok(&output);
    assert_eq!(serde_json::from_slice::<i64>(&output.stdout).unwrap(), 1);
    for path in ["one space.txt", "雪.txt"] {
        assert_eq!(fs::read(folder.path().join(path)).unwrap(), b"LETTERS");
    }
}
#[test]
fn output_limits_and_timeouts_kill_owned_descendants() {
    let folder = tempfile::tempdir().unwrap();
    let result = invoke(
        folder.path(),
        &format!(
            "{}then save to \"flood.bin\"
",
            native(&["flood"])
        ),
        &["--max-output-bytes", "1024"],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("exceeds the limit of 1024 bytes"));
    let marker = folder.path().join("escaped.txt");
    let result = invoke(
        folder.path(),
        &native(&["spawn", marker.to_str().unwrap()]),
        &["--command-timeout-ms", "150"],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("timed out"));
    std::thread::sleep(Duration::from_millis(850));
    assert!(!marker.exists(), "descendant escaped its native job");
}
#[test]
fn cache_and_completed_checkpoint_do_not_relaunch_effects() {
    let folder = tempfile::tempdir().unwrap();
    let counter = folder.path().join("counter");
    let source = native(&["increment", counter.to_str().unwrap()]);
    let first = invoke(folder.path(), &source, &["--checkpoint", "checkpoint"]);
    ok(&first);
    assert_eq!(first.stdout, b"1");
    let resumed = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "resume",
            "job.program",
            "--checkpoint",
            "checkpoint",
            "--trace",
        ])
        .current_dir(folder.path())
        .output()
        .unwrap();
    ok(&resumed);
    assert_eq!(fs::read_to_string(counter).unwrap(), "1");
    assert!(!String::from_utf8_lossy(&resumed.stderr).contains("\"event\":\"spawn\""));
    let manifests = folder.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    fs::write(manifests.join("cached.json"),serde_json::json!({"name":"cached","binary":fixture(),"cacheable":true,"arguments":[{"literal":"write"},{"literal":"cached bytes"}]}).to_string()).unwrap();
    let options = [
        "--manifest-dir",
        "manifests",
        "--cache-dir",
        "cache",
        "--trace",
    ];
    ok(&invoke(folder.path(), "run cached and cache\n", &options));
    let output = invoke(folder.path(), "run cached and cache\n", &options);
    ok(&output);
    assert_eq!(output.stdout, b"cached bytes");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("\"event\":\"spawn\""));
}
#[test]
fn intervals_listeners_and_stream_cutoff_use_native_children() {
    let folder = tempfile::tempdir().unwrap();
    let source = format!("every 1 millisecond\n  {}", native(&["write", "tick"]));
    let output = invoke(folder.path(), &source, &["--max-runs", "2"]);
    ok(&output);
    assert_eq!(output.stdout, b"ticktick");
    let source = format!(
        "stream from {} with arguments [\"produce\"]\ntake the first 2 lines\nshow each line\n",
        quoted(&fixture())
    );
    let output = invoke(folder.path(), &source, &[]);
    ok(&output);
    assert_eq!(output.stdout, b"one\ntwo\n");
    let source=format!("listen to {} with arguments [\"produce\"]\nwhenever a line arrives as event\n  then {} with arguments [\"upper\"]\n",quoted(&fixture()),quoted(&fixture()));
    let output = invoke(folder.path(), &source, &["--max-runs", "2"]);
    ok(&output);
    assert_eq!(output.stdout, b"ONE\nTWO\n");
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[cfg(feature = "live")]
fn live(socket: &Path, args: &[&str]) -> serde_json::Value {
    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("live")
        .args(args)
        .arg("--socket")
        .arg(socket)
        .output()
        .unwrap();
    ok(&result);
    serde_json::from_slice(&result.stdout).unwrap()
}
#[test]
#[cfg(feature = "live")]
fn owner_checked_named_pipe_supports_live_crud_execution_and_shutdown() {
    let folder = tempfile::tempdir().unwrap();
    let source = folder.path().join("live.program");
    fs::write(
        &source,
        format!(
            "accept message as text\nrun {} with arguments [\"write\", message]\n",
            quoted(&fixture())
        ),
    )
    .unwrap();
    let socket = folder.path().join("control.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["live", "serve"])
        .arg(source)
        .arg("--socket")
        .arg(&socket)
        .args(["--paused", "--bindings", "{\"message\":\"before\"}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = Server(child);
    let stdout = server.0.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut ready = String::new();
        let result = BufReader::new(stdout).read_line(&mut ready);
        let _ = sender.send((result, ready));
    });
    let (result, ready) = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("server readiness deadline");
    result.unwrap();
    reader.join().unwrap();
    assert!(
        ready.contains("ready"),
        "server did not become ready: {ready}"
    );
    live(
        &socket,
        &["update", "/bindings/message", "--json", "\"after 雪\""],
    );
    live(&socket, &["create", "/user/note", "--json", "true"]);
    let state = live(&socket, &["read", "/bindings/message"]);
    assert!(state.to_string().contains("after 雪"));
    live(&socket, &["delete", "/user/note"]);
    live(&socket, &["control", "resume"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = live(&socket, &["read", "/runtime/status"]);
        if state.to_string().contains("completed") {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    live(&socket, &["control", "stop"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
#[cfg(all(feature = "screens", feature = "model"))]
fn headless_widgets_and_bundled_model_helper_are_native() {
    let folder = tempfile::tempdir().unwrap();
    let source=format!("open a screen titled \"Native Windows\"\nplace main on the left\ndefine a program named main\n  show a badge named status with text \"waiting\"\n  whenever the program opens\n    {}    then show the result in status\n",native(&["write","native done"]));
    let output = invoke(folder.path(), &source, &["--headless"]);
    ok(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("native done"));
    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["model", "--help"])
        .output()
        .unwrap();
    ok(&result);
    assert!(String::from_utf8_lossy(&result.stdout).contains("execute-model"));
}

#[test]
fn finite_stdin_capture_preserves_bytes_and_obeys_limits_and_timeout() {
    let folder = tempfile::tempdir().unwrap();
    let script = folder.path().join("capture.program");
    fs::write(&script, "read from stdin\n").unwrap();
    let data: Vec<u8> = (0..256).cycle().take(65536).map(|n| n as u8).collect();
    let input = folder.path().join("input.bin");
    fs::write(&input, &data).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(&script)
        .stdin(fs::File::open(&input).unwrap())
        .output()
        .unwrap();
    ok(&result);
    assert_eq!(result.stdout, data);
    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(&script)
        .args(["--max-output-bytes", "32"])
        .stdin(fs::File::open(&input).unwrap())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    let mut child = Server(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(&script)
            .args(["--command-timeout-ms", "150"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "blocked native stdin did not cancel"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn unicode_file_uri_and_batch_rejection_are_explicit() {
    let folder = tempfile::tempdir().unwrap();
    let script = folder.path().join("snow 雪.program");
    fs::write(&script, native(&["write", "uri"])).unwrap();
    let uri = url::Url::from_file_path(&script).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(uri.as_str())
        .output()
        .unwrap();
    ok(&result);
    assert_eq!(result.stdout, b"uri");
    let batch = folder.path().join("forbidden.cmd");
    fs::write(&batch, "@echo must-not-run").unwrap();
    let result = invoke(
        folder.path(),
        &format!("run {}\n", quoted(batch.to_str().unwrap())),
        &[],
    );
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("explicit cmd.exe"));
}

#[test]
fn captured_stderr_flood_cannot_starve_native_deadlines() {
    let folder = tempfile::tempdir().unwrap();
    let program = flow::parser::parse(&native(&["stderr-flood"])).unwrap();
    let options = flow::runtime::RunOptions {
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_millis(150)),
        ..flow::runtime::RunOptions::new(folder.path().to_path_buf())
    };
    let started = Instant::now();
    let result = flow::runtime::run_with_context(
        &program,
        &flow::manifest::Registry::default(),
        &options,
        Default::default(),
        Vec::new(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("stderr flood exceeded its deadline"),
    };
    assert!(format!("{error:#}").contains("timed out"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stderr monopolized the supervisor"
    );
}
