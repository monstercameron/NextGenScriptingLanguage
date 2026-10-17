#![cfg(unix)]

use flow::{manifest::Registry, parser, runtime, stream};
use serde_json::{json, Value};
use std::fs;
#[cfg(target_os = "macos")]
use std::fs::OpenOptions;
#[cfg(target_os = "macos")]
use std::io::Write;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("manifests")).unwrap();
    dir
}

fn manifest_dir(dir: &TempDir) -> PathBuf {
    dir.path().join("manifests")
}

fn executable(name: &str) -> PathBuf {
    [Path::new("/usr/bin"), Path::new("/bin")]
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("could not find system {name:?}"))
        .canonicalize()
        .unwrap()
}

fn write_manifest(dir: &TempDir, name: &str, manifest: Value) {
    fs::write(
        manifest_dir(dir).join(format!("{name}.json")),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn applet(
    name: &str,
    binary: &Path,
    accepts_stdin: bool,
    parameters: Value,
    arguments: Value,
) -> Value {
    json!({
        "name": name,
        "binary": binary,
        "accepts_stdin": accepts_stdin,
        "cacheable": false,
        "parameters": parameters,
        "arguments": arguments,
    })
}

fn add_cat_copy(dir: &TempDir) {
    write_manifest(
        dir,
        "cat.copy",
        applet("cat.copy", &executable("cat"), true, json!({}), json!([])),
    );
}

fn add_cat_file(dir: &TempDir) {
    write_manifest(
        dir,
        "cat.file",
        applet(
            "cat.file",
            &executable("cat"),
            false,
            json!({ "path": { "type": "string", "required": true } }),
            json!([
                { "literal": "--" },
                { "parameter": "path" }
            ]),
        ),
    );
}

fn add_tr_upper(dir: &TempDir) {
    write_manifest(
        dir,
        "tr.upper",
        applet(
            "tr.upper",
            &executable("tr"),
            true,
            json!({
                "from": { "type": "string", "required": true },
                "to": { "type": "string", "required": true },
                "flags": { "type": "list", "items": "string" }
            }),
            json!([
                { "when_present": "flags", "append": [{ "spread": "flags" }] },
                { "parameter": "from" },
                { "parameter": "to" }
            ]),
        ),
    );
}

fn add_python_applet(
    dir: &TempDir,
    name: &str,
    script_name: &str,
    script: &str,
    parameters: Value,
    arguments: Value,
    accepts_stdin: bool,
) {
    let script_path = dir.path().join(script_name);
    fs::write(&script_path, script).unwrap();
    let mut argv = vec![json!({ "literal": script_path.to_string_lossy() })];
    argv.extend(arguments.as_array().unwrap().iter().cloned());
    write_manifest(
        dir,
        name,
        applet(
            name,
            &executable("python3"),
            accepts_stdin,
            parameters,
            Value::Array(argv),
        ),
    );
}

fn workflow_path(dir: &TempDir, source: &str) -> PathBuf {
    let path = dir.path().join("case.program");
    fs::write(&path, source).unwrap();
    path
}

fn runtime_options(
    dir: &TempDir,
    command_timeout: Option<Duration>,
    total_timeout: Option<Duration>,
) -> runtime::RunOptions {
    runtime::RunOptions {
        inputs: Default::default(),
        working_directory: dir.path().to_owned(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024,
        command_timeout,
        total_timeout,
    }
}

fn add_idle_closed_stdout_child(dir: &TempDir) {
    add_python_applet(
        dir,
        "idle.child",
        "idle_child.py",
        "import os, time\nfrom pathlib import Path\nPath('child.pid').write_text(str(os.getpid()))\nos.close(1)\nwhile True: time.sleep(1)\n",
        json!({}),
        json!([]),
        false,
    );
}

fn wait_for_child_pid(path: &Path) -> libc::pid_t {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            // The fixture creates the file before writing its contents, so
            // existence alone does not mean the PID is ready to parse.
            if let Ok(pid) = contents.trim().parse() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "idle fixture did not publish a valid PID"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn assert_process_group_reaped(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: signal 0 sends nothing and the call takes only integers. `pid` is the
        // process-group leader recorded by the idle fixture; the runtime starts applets with
        // setpgid(0, 0) (src/runtime/unix_process.rs), so -pid names that group. Nothing here
        // rejects 0 or a negative pid, so callers must pass a positive one.
        let result = unsafe { libc::kill(-pid, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "owned process group was not reaped"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn run_capture(dir: &TempDir, command: &str, source: &str, extra: &[&str]) -> std::process::Output {
    let mut runner = Running::spawn_command(dir, command, source, extra);
    let stdout = runner.take_stdout();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = BufReader::new(stdout).read_to_end(&mut bytes);
        let _ = tx.send((result, bytes));
    });
    let status = runner.wait(Duration::from_secs(12));
    let (result, stdout) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    result.unwrap();
    let stderr = runner.stderr.lock().unwrap().clone();
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

struct Running {
    child: Child,
    stdout: Option<ChildStdout>,
    trace_lines: mpsc::Receiver<Vec<u8>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl Running {
    fn spawn(dir: &TempDir, source: &str, extra: &[&str]) -> Self {
        Self::spawn_command(dir, "run", source, extra)
    }
    fn spawn_command(dir: &TempDir, command: &str, source: &str, extra: &[&str]) -> Self {
        let file = workflow_path(dir, source);
        let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(dir.path())
            .arg(command)
            .arg(file)
            .arg("--manifest-dir")
            .arg(manifest_dir(dir))
            .args(extra)
            .env("LC_ALL", "C")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr_pipe = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let saved_stderr = Arc::clone(&stderr);
        let (trace_tx, trace_lines) = mpsc::channel();
        let stderr_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            loop {
                let mut line = Vec::new();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        saved_stderr.lock().unwrap().extend_from_slice(&line);
                        if trace_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            stdout: Some(stdout),
            trace_lines,
            stderr,
            stderr_thread: Some(stderr_thread),
        }
    }

    fn wait_for_spawns(&self, expected: &[&str]) {
        let mut found = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while found.len() < expected.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for spawn trace events"
            );
            let line = self
                .trace_lines
                .recv_timeout(remaining)
                .expect("stream ended before all applets were spawned");
            if let Ok(event) = serde_json::from_slice::<Value>(&line) {
                if event.get("event").and_then(Value::as_str) == Some("spawn") {
                    if let Some(name) = event.get("applet").and_then(Value::as_str) {
                        found.push(name.to_owned());
                    }
                }
            }
        }
        for name in expected {
            assert_eq!(
                found.iter().filter(|found| found.as_str() == *name).count(),
                1,
                "spawn trace: {found:?}"
            );
        }
    }

    fn line_reader(&mut self) -> mpsc::Receiver<Vec<u8>> {
        let stdout = self.stdout.take().expect("stdout reader already started");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) if tx.send(line).is_err() => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        rx
    }

    fn take_stdout(&mut self) -> ChildStdout {
        self.stdout.take().expect("stdout already taken")
    }

    fn assert_still_running(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "stream runner reached EOF before the source was closed"
        );
    }

    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                if let Some(thread) = self.stderr_thread.take() {
                    while !thread.is_finished() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    assert!(
                        thread.is_finished(),
                        "stderr remained open after runner exit"
                    );
                    thread.join().unwrap();
                }
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "stream runner did not finish in time"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Signal only the owned CLI child. Its runtime owns and reaps its applet groups.
        if self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: kill takes only integers. The try_wait above returned Ok(None) or an
            // error (both map to is_none), never Ok(Some), so the child has not been reaped
            // and `self.child.id()` still names it.
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, libc::SIGINT);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

fn receive_line(lines: &mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    lines
        .recv_timeout(Duration::from_secs(5))
        .expect("timed out waiting for a streamed record")
}

fn assert_spawn_lines(stderr: &str, expected: &[&str]) {
    let events: Vec<Value> = stderr
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let spawns: Vec<&str> = events
        .iter()
        .filter(|event| event.get("event").and_then(Value::as_str) == Some("spawn"))
        .filter_map(|event| event.get("applet").and_then(Value::as_str))
        .collect();
    for name in expected {
        assert_eq!(
            spawns.iter().filter(|found| **found == *name).count(),
            1,
            "spawn trace: {spawns:?}"
        );
    }
    assert_eq!(spawns.len(), expected.len(), "spawn trace: {spawns:?}");
    for pid in events
        .iter()
        .filter(|e| e["event"] == "spawn")
        .filter_map(|e| e["pid"].as_u64())
    {
        assert_eq!(
            // SAFETY: signal 0 sends nothing and takes only an integer. `pid` is the applet
            // pid the runner reported in its own spawn trace for a child it owns.
            unsafe { libc::kill(pid as libc::pid_t, 0) },
            -1,
            "owned applet still alive after runner exit"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn mac_tail_and_unbuffered_tr_stream_appended_rows_then_reap_tail_at_limit() {
    let dir = fixture();
    let tail_path = executable("tail");
    let tr_path = executable("tr");
    write_manifest(
        &dir,
        "tail.follow",
        applet(
            "tail.follow",
            &tail_path,
            false,
            json!({ "path": { "type": "string", "required": true } }),
            json!([
                { "literal": "-n" },
                { "literal": "+1" },
                { "literal": "-F" },
                { "literal": "--" },
                { "parameter": "path" }
            ]),
        ),
    );
    write_manifest(
        &dir,
        "tr.upper",
        applet(
            "tr.upper",
            &tr_path,
            true,
            json!({
                "from": { "type": "string", "required": true },
                "to": { "type": "string", "required": true },
                "flags": { "type": "list", "items": "string" }
            }),
            json!([
                { "when_present": "flags", "append": [{ "spread": "flags" }] },
                { "parameter": "from" },
                { "parameter": "to" }
            ]),
        ),
    );
    let input_path = dir.path().join("input.log");
    fs::write(&input_path, b"").unwrap();
    let source = format!(
        "stream from tail.follow with path {:?}\npipe to tr.upper with flags [\"-u\"] and from \"[:lower:]\" and to \"[:upper:]\"\ntake the first 3 lines\nshow each line\n",
        input_path.to_string_lossy()
    );
    let mut runner = Running::spawn(
        &dir,
        &source,
        &[
            "--trace",
            "--max-record-bytes",
            "64",
            "--max-output-bytes",
            "1",
        ],
    );
    let lines = runner.line_reader();
    runner.wait_for_spawns(&["tail.follow", "tr.upper"]);
    let mut input = OpenOptions::new().append(true).open(&input_path).unwrap();
    let expected = [
        b"ROW-0\n".to_vec(),
        b"ROW-1\n".to_vec(),
        b"ROW-2\n".to_vec(),
    ];
    for (index, expected) in expected.iter().enumerate() {
        writeln!(input, "row-{index}").unwrap();
        if index == 2 {
            writeln!(input, "row-3").unwrap();
        }
        input.flush().unwrap();
        assert_eq!(receive_line(&lines), *expected);
        if index < 2 {
            runner.assert_still_running();
        }
    }
    assert!(
        runner.wait(Duration::from_secs(5)).success(),
        "{}",
        runner.stderr_text()
    );
    assert!(
        matches!(
            lines.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ),
        "take emitted an extra record or retained stdout after exit"
    );
    assert_spawn_lines(&runner.stderr_text(), &["tail.follow", "tr.upper"]);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_persistent_producer_and_cat_stream_before_eof_then_stop_at_limit() {
    let dir = fixture();
    add_cat_copy(&dir);
    add_python_applet(
        &dir,
        "live.producer",
        "live_producer.py",
        "import os, time\nfor i in range(3):\n os.write(1, f'row-{i}\\n'.encode())\n time.sleep(0.25)\nwhile True:\n time.sleep(1)\n",
        json!({}),
        json!([]),
        false,
    );
    let source =
        "stream from live.producer\npipe to cat.copy\ntake the first 3 lines\nshow each line\n";
    let mut runner = Running::spawn(&dir, source, &["--trace", "--max-record-bytes", "32"]);
    let lines = runner.line_reader();
    runner.wait_for_spawns(&["live.producer", "cat.copy"]);
    for index in 0..3 {
        assert_eq!(
            receive_line(&lines).as_slice(),
            format!("row-{index}\n").as_bytes()
        );
        if index < 2 {
            runner.assert_still_running();
        }
    }
    assert!(
        runner.wait(Duration::from_secs(5)).success(),
        "{}",
        runner.stderr_text()
    );
    assert!(
        matches!(
            lines.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ),
        "take emitted an extra record or retained stdout after exit"
    );
    assert_spawn_lines(&runner.stderr_text(), &["live.producer", "cat.copy"]);
}

#[test]
fn finite_file_to_tr_preserves_binary_bytes_at_exact_total_output_limit() {
    let dir = fixture();
    add_cat_file(&dir);
    add_tr_upper(&dir);
    fs::write(
        dir.path().join("binary.input"),
        b"Alpha\0Beta\nHigh:\xffz\n",
    )
    .unwrap();
    let expected = b"ALPHA\0BETA\nHIGH:\xffZ\n";
    let source = "stream from cat.file with path \"binary.input\"\npipe to tr.upper with from \"[:lower:]\" and to \"[:upper:]\"\nshow each line\n";
    let exact_limit = expected.len().to_string();
    let result = run_capture(
        &dir,
        "run",
        source,
        &["--max-output-bytes", exact_limit.as_str()],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, expected);
}

#[test]
fn stream_framing_counts_records_preserves_eof_bytes_and_enforces_record_bounds() {
    let dir = fixture();
    add_cat_file(&dir);
    fs::write(dir.path().join("rows"), b"one\ntwo\nthree\nfour\n").unwrap();
    let result = run_capture(
        &dir,
        "run",
        "stream from cat.file with path \"rows\"\ntake the first 3 lines\nshow each line\n",
        &[],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"one\ntwo\nthree\n");

    fs::write(dir.path().join("rows"), b"").unwrap();
    let empty = run_capture(
        &dir,
        "run",
        "stream from cat.file with path \"rows\"\ntake the first 1 lines\nshow each line\n",
        &[],
    );
    assert!(
        empty.status.success(),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
    assert!(empty.stdout.is_empty());

    fs::write(dir.path().join("rows"), b"last\0\xff").unwrap();
    let unterminated = run_capture(
        &dir,
        "run",
        "stream from cat.file with path \"rows\"\ntake the first 1 lines\nshow each line\n",
        &[],
    );
    assert!(
        unterminated.status.success(),
        "{}",
        String::from_utf8_lossy(&unterminated.stderr)
    );
    assert_eq!(unterminated.stdout, b"last\0\xff");

    fs::write(dir.path().join("rows"), b"1234567\n").unwrap();
    let exact = run_capture(
        &dir,
        "run",
        "stream from cat.file with path \"rows\"\nshow each line\n",
        &["--max-record-bytes", "8"],
    );
    assert!(
        exact.status.success(),
        "{}",
        String::from_utf8_lossy(&exact.stderr)
    );
    assert_eq!(exact.stdout, b"1234567\n");

    fs::write(dir.path().join("rows"), b"12345678\n").unwrap();
    let over = run_capture(
        &dir,
        "run",
        "stream from cat.file with path \"rows\"\nshow each line\n",
        &["--max-record-bytes", "8"],
    );
    assert!(!over.status.success());
    assert!(String::from_utf8_lossy(&over.stderr).contains("stream record exceeds 8 bytes"));
    assert!(
        over.stdout.is_empty(),
        "oversized record was partially emitted"
    );
}

#[test]
fn middle_stage_failure_does_not_emit_records_after_the_failure() {
    let dir = fixture();
    add_python_applet(
        &dir,
        "finite.producer",
        "finite_producer.py",
        "import os\nos.write(1, b'first\\nsecond\\nthird\\n')\n",
        json!({}),
        json!([]),
        false,
    );
    add_python_applet(
        &dir,
        "fail.middle",
        "fail_middle.py",
        "import sys\nline = sys.stdin.buffer.readline()\nsys.stdout.buffer.write(line)\nsys.stdout.buffer.flush()\nsys.exit(17)\n",
        json!({}),
        json!([]),
        true,
    );
    add_cat_copy(&dir);
    let source =
        "stream from finite.producer\npipe to fail.middle\npipe to cat.copy\nshow each line\n";
    let mut runner = Running::spawn(&dir, source, &["--command-timeout-ms", "5000"]);
    let stdout = runner.take_stdout();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = BufReader::new(stdout).read_to_end(&mut bytes);
        let _ = tx.send((result, bytes));
    });
    let status = runner.wait(Duration::from_secs(7));
    assert!(
        !status.success(),
        "middle stage failure unexpectedly succeeded"
    );
    let (read_result, output) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    read_result.unwrap();
    assert!(
        output.is_empty() || output == b"first\n",
        "unexpected output after failure: {output:?}"
    );
    assert!(!output
        .windows(b"second".len())
        .any(|part| part == b"second"));
    assert!(
        runner.stderr_text().contains("fail.middle"),
        "{}",
        runner.stderr_text()
    );
}

#[test]
fn slow_stdout_consumer_backpressures_a_persistent_source_and_stream_ignores_total_limit() {
    let dir = fixture();
    let marker_parameters = json!({
        "started": { "type": "string", "required": true },
        "done": { "type": "string", "required": true }
    });
    add_python_applet(
        &dir,
        "burst.producer",
        "burst.py",
        "import os, sys\nopen(sys.argv[1], 'wb').close()\nrecord = b'x' * 65535 + b'\\n'\nblock = record * 64\nview = memoryview(block)\nwhile view:\n written = os.write(1, view)\n view = view[written:]\nopen(sys.argv[2], 'wb').close()\n",
        marker_parameters,
        json!([
            { "parameter": "started" },
            { "parameter": "done" }
        ]),
        false,
    );
    add_cat_copy(&dir);
    let started = dir.path().join("started.marker");
    let done = dir.path().join("done.marker");
    let source = "stream from burst.producer with started \"started.marker\" and done \"done.marker\"\npipe to cat.copy\nshow each line\n";
    let mut runner = Running::spawn(
        &dir,
        source,
        &[
            "--max-record-bytes",
            "65536",
            "--max-output-bytes",
            "1",
            "--command-timeout-ms",
            "20000",
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(started.exists(), "producer did not start");
    thread::sleep(Duration::from_millis(150));
    runner.assert_still_running();
    assert!(
        !done.exists(),
        "producer passed the blocked stdout consumer"
    );

    let stdout = runner.take_stdout();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = BufReader::new(stdout).read_to_end(&mut bytes);
        let _ = tx.send((result, bytes));
    });
    assert!(
        runner.wait(Duration::from_secs(20)).success(),
        "{}",
        runner.stderr_text()
    );
    let (read_result, output) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    read_result.unwrap();
    let expected_record = [vec![b'x'; 65535], vec![b'\n']].concat();
    assert_eq!(output.len(), expected_record.len() * 64);
    assert!(output
        .chunks_exact(expected_record.len())
        .all(|record| record == expected_record));
    assert!(
        done.exists(),
        "producer did not resume after stdout was drained"
    );
}

#[test]
fn check_and_unsupported_resume_checkpoint_max_runs_and_headless_do_not_launch_source() {
    let dir = fixture();
    let marker = dir.path().join("launched.marker");
    add_python_applet(
        &dir,
        "side.effect",
        "side_effect.py",
        "import os, sys\nopen(sys.argv[1], 'wb').close()\nos.write(1, b'ok\\n')\n",
        json!({ "marker": { "type": "string", "required": true } }),
        json!([{ "parameter": "marker" }]),
        false,
    );
    let source = "stream from side.effect with marker \"launched.marker\"\nshow each line\n";
    let check = run_capture(&dir, "check", source, &[]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(!marker.exists(), "check launched the stream source");

    for (command, extra) in [
        ("run", vec!["--checkpoint", "checkpoint"]),
        ("resume", vec!["--checkpoint", "checkpoint"]),
        ("run", vec!["--max-runs", "1"]),
        ("run", vec!["--headless"]),
    ] {
        let result = run_capture(&dir, command, source, &extra);
        assert!(
            !result.status.success(),
            "{command} {extra:?} unexpectedly succeeded"
        );
        assert!(
            !marker.exists(),
            "{command} {extra:?} launched the stream source"
        );
    }
}

#[test]
fn blocked_stdout_remains_deadline_bounded_and_cancellable() {
    for cancel in [false, true] {
        let dir = fixture();
        add_python_applet(
            &dir,
            "writer",
            "writer.py",
            "import os\nrecord = b'x' * 65535 + b'\\n'\nwhile True:\n os.write(1, record)\n",
            json!({}),
            json!([]),
            false,
        );
        let mut runner = Running::spawn(
            &dir,
            "stream from writer\nshow each line\n",
            &[
                "--trace",
                "--command-timeout-ms",
                if cancel { "10000" } else { "2000" },
            ],
        );
        runner.wait_for_spawns(&["writer"]);
        // Retain stdout without draining it: the line sink must block on its
        // descriptor, then remain responsive to the deadline or owned signal.
        thread::sleep(Duration::from_millis(100));
        if cancel {
            runner.assert_still_running();
            assert_eq!(
                // SAFETY: kill takes only integers. `assert_still_running` just asserted that
                // try_wait returned Ok(None), so the runner has not exited or been reaped and
                // `runner.child.id()` still names it.
                unsafe { libc::kill(runner.child.id() as libc::pid_t, libc::SIGINT) },
                0
            );
        }
        assert!(!runner.wait(Duration::from_secs(5)).success());
        let error = runner.stderr_text();
        assert!(
            error.contains(if cancel { "cancelled" } else { "timed out" }),
            "{error}"
        );
        assert_spawn_lines(&error, &["writer"]);
    }
}

#[test]
fn shared_cancellation_kills_idle_child_after_stdout_closes() {
    let dir = fixture();
    add_idle_closed_stdout_child(&dir);
    let registry = Registry::load(&manifest_dir(&dir)).unwrap();
    let program = stream::parse("stream from idle.child\nshow each line\n").unwrap();
    let options = runtime_options(
        &dir,
        Some(Duration::from_secs(5)),
        Some(Duration::from_secs(5)),
    );
    let cancelled = Arc::new(AtomicBool::new(false));
    let watcher_flag = Arc::clone(&cancelled);
    let pid_path = dir.path().join("child.pid");
    let watcher = thread::spawn(move || {
        let pid = wait_for_child_pid(&pid_path);
        watcher_flag.store(true, Ordering::Relaxed);
        pid
    });
    let started = Instant::now();
    let mut records = 0;
    let result = runtime::run_stream_with_cancellation(
        &program,
        &registry,
        &options,
        1024,
        cancelled,
        |_, _, _| {
            records += 1;
            Ok(())
        },
    );
    let elapsed = started.elapsed();
    let pid = watcher.join().unwrap();
    assert!(result.is_err(), "idle child unexpectedly succeeded");
    assert!(
        format!("{:#}", result.unwrap_err()).contains("cancelled"),
        "external cancellation should be observable"
    );
    assert_eq!(
        records, 0,
        "the child closed stdout before emitting records"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "cancellation took {elapsed:?}"
    );
    assert_process_group_reaped(pid);
}

#[test]
fn inherited_absolute_deadline_kills_idle_child_after_stdout_closes() {
    let dir = fixture();
    add_idle_closed_stdout_child(&dir);
    let registry = Registry::load(&manifest_dir(&dir)).unwrap();
    let program = stream::parse("stream from idle.child\nshow each line\n").unwrap();
    let options = runtime_options(
        &dir,
        Some(Duration::from_secs(5)),
        Some(Duration::from_secs(4)),
    );
    let deadline = Instant::now() + Duration::from_millis(750);
    let context = runtime::StreamExecutionContext::default().with_deadline(Some(deadline));
    let started = Instant::now();
    let mut records = 0;
    let result = runtime::run_stream_with_context(
        &program,
        &registry,
        &options,
        1024,
        context,
        |_, _, _| {
            records += 1;
            Ok(())
        },
    );
    let elapsed = started.elapsed();
    let pid = wait_for_child_pid(&dir.path().join("child.pid"));
    assert!(
        result.is_err(),
        "idle child outlived the inherited deadline"
    );
    assert_eq!(records, 0);
    assert!(
        elapsed < Duration::from_secs(3),
        "deadline took {elapsed:?}"
    );
    assert_process_group_reaped(pid);
}

#[test]
fn total_and_command_timeouts_still_bound_idle_stream_children() {
    for timeout in [
        (
            Some(Duration::from_secs(5)),
            Some(Duration::from_millis(750)),
        ),
        (Some(Duration::from_millis(750)), None),
    ] {
        let dir = fixture();
        add_idle_closed_stdout_child(&dir);
        let registry = Registry::load(&manifest_dir(&dir)).unwrap();
        let program = stream::parse("stream from idle.child\nshow each line\n").unwrap();
        let options = runtime_options(&dir, timeout.0, timeout.1);
        let started = Instant::now();
        let result = runtime::run_stream(&program, &registry, &options, 1024, |_, _, _| Ok(()));
        let elapsed = started.elapsed();
        let pid = wait_for_child_pid(&dir.path().join("child.pid"));
        assert!(result.is_err(), "idle child outlived timeout configuration");
        assert!(elapsed < Duration::from_secs(3), "timeout took {elapsed:?}");
        assert_process_group_reaped(pid);
    }
}

#[test]
fn secret_stream_cannot_reach_a_declared_writer_even_with_a_weaker_context_floor() {
    let dir = fixture();
    add_python_applet(
        &dir,
        "secret.source",
        "secret_source.py",
        "import os\nfrom pathlib import Path\nPath('source-launched').touch()\nos.write(1, b'secret\\n')\n",
        json!({}),
        json!([]),
        false,
    );
    let mut source_manifest: Value =
        serde_json::from_slice(&fs::read(manifest_dir(&dir).join("secret.source.json")).unwrap())
            .unwrap();
    source_manifest["contract"] = json!({
        "version": 1,
        "output": {"type": "text", "encoding": "raw"},
        "ports": {
            "inputs": {},
            "outputs": {
                "value": {
                    "schema": {"type": "text"},
                    "delivery": "single",
                    "sensitivity": "secret"
                }
            },
            "events": {}
        }
    });
    write_manifest(&dir, "secret.source", source_manifest);
    add_python_applet(
        &dir,
        "fixture.writer",
        "writer.py",
        "import sys\nfrom pathlib import Path\nPath('writer-launched').touch()\nPath('writer-output').write_bytes(sys.stdin.buffer.read())\n",
        json!({}),
        json!([]),
        true,
    );
    let mut writer_manifest: Value =
        serde_json::from_slice(&fs::read(manifest_dir(&dir).join("fixture.writer.json")).unwrap())
            .unwrap();
    writer_manifest["contract"] = json!({
        "version": 1,
        "capabilities": ["filesystem.write"],
        "effects": ["filesystem.write"]
    });
    write_manifest(&dir, "fixture.writer", writer_manifest);

    let registry = Registry::load(&manifest_dir(&dir)).unwrap();
    let program =
        stream::parse("stream from secret.source\npipe to fixture.writer\nshow each line\n")
            .unwrap();
    let options = runtime_options(&dir, Some(Duration::from_secs(3)), None);
    let weak_context = runtime::StreamExecutionContext::default().with_sensitivity_floor(
        flow::security::SensitivitySet::from_labels([
            flow::contracts::Sensitivity::Public,
            flow::contracts::Sensitivity::Unknown,
        ]),
    );
    let result = runtime::run_stream_with_context(
        &program,
        &registry,
        &options,
        1024,
        weak_context,
        |_, _, _| Ok(()),
    );
    assert!(result.is_err(), "secret bytes reached a declared writer");
    assert!(format!("{:#}", result.unwrap_err()).contains("persist"));
    assert!(!dir.path().join("source-launched").exists());
    assert!(!dir.path().join("writer-launched").exists());
    assert!(!dir.path().join("writer-output").exists());
}

#[test]
fn secret_listener_source_is_rejected_before_spawn_when_it_declares_writes() {
    let dir = fixture();
    add_python_applet(
        &dir,
        "fixture.listener",
        "listener.py",
        "import os\nfrom pathlib import Path\nPath('listener-launched').touch()\nos.write(1, b'secret\\n')\n",
        json!({}),
        json!([]),
        false,
    );
    let mut listener_manifest: Value = serde_json::from_slice(
        &fs::read(manifest_dir(&dir).join("fixture.listener.json")).unwrap(),
    )
    .unwrap();
    listener_manifest["contract"] = json!({
        "version": 1,
        "capabilities": ["filesystem.write"],
        "effects": ["filesystem.write"],
        "ports": {
            "inputs": {},
            "outputs": {},
            "events": {
                "records": {
                    "schema": {"type": "text"},
                    "delivery": "events",
                    "sensitivity": "secret"
                }
            }
        }
    });
    write_manifest(&dir, "fixture.listener", listener_manifest);
    let registry = Registry::load(&manifest_dir(&dir)).unwrap();
    let program = parser::parse(
        "listen to fixture.listener\nwhenever a line arrives as event\n  then \"cat\"\n",
    )
    .unwrap();
    let options = runtime_options(&dir, Some(Duration::from_secs(3)), None);
    let result = runtime::run_service(
        &program,
        &registry,
        &options,
        &runtime::ServiceLimits {
            max_runs: Some(1),
            max_record_bytes: 1024,
        },
        |_| Ok(()),
    );
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("secret listener source unexpectedly started"),
    };
    assert!(format!("{error:#}").contains("persist"));
    assert!(!dir.path().join("listener-launched").exists());
}
