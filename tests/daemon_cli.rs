//! `execute daemon` (#77): status, serve, stop, directory and peer checks, and forwarded
//! runs. A forwarded run must match a cold run in output and exit status.
#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const PRINT_OUT: &str = r#"run printf with message "out""#;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn daemon_command(runtime_dir: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.env("XDG_RUNTIME_DIR", runtime_dir);
    command.env_remove("EXECUTE_DAEMON");
    command.arg("daemon").args(arguments);
    command
}

fn daemon_output(runtime_dir: &Path, arguments: &[&str]) -> Output {
    daemon_command(runtime_dir, arguments).output().unwrap()
}

/// The socket `daemon serve` uses when no `--socket` is given under `runtime_dir`.
fn default_socket_under(runtime_dir: &Path) -> PathBuf {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    runtime_dir
        .join(format!("execute-{uid}"))
        .join("daemon.sock")
}

/// Kills the background daemon if a test fails before stopping it.
struct Daemon(Option<Child>);

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn serve_in_background(runtime_dir: &Path) -> Daemon {
    let child = daemon_command(runtime_dir, &["serve"])
        .spawn()
        .expect("spawn daemon serve");
    let mut daemon = Daemon(Some(child));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = daemon_output(runtime_dir, &["status"]);
        let reply: Value = serde_json::from_slice(&status.stdout).unwrap();
        if reply["running"] == true {
            return daemon;
        }
        if Instant::now() > deadline {
            let _ = daemon.0.as_mut().unwrap().kill();
            panic!("daemon did not start within 10 s");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Run `source` as a program. Stdin is null so an interactive program cannot block.
fn run_source(runtime_dir: &Path, directory: &Path, source: &str, extra: &[&str]) -> Output {
    run_source_with(runtime_dir, directory, source, extra, || Ok(()))
}

/// Run `source`, with `hook` applied to the client's command in the child before exec. The
/// hook sets what the client process itself has, such as its umask or a limit.
fn run_source_with<F>(
    runtime_dir: &Path,
    directory: &Path,
    source: &str,
    extra: &[&str],
    hook: F,
) -> Output
where
    F: FnMut() -> io::Result<()> + Send + Sync + 'static,
{
    let path = directory.join("workflow.program");
    fs::write(&path, source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env_remove("EXECUTE_DAEMON")
        .current_dir(directory)
        .stdin(Stdio::null())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(extra);
    // SAFETY: the hooks used here only call umask and setrlimit, which are safe between
    // fork and exec.
    unsafe {
        command.pre_exec(hook);
    }
    command.output().unwrap()
}

fn run_program(runtime_dir: &Path, directory: &Path, extra: &[&str]) -> Output {
    run_source(runtime_dir, directory, PRINT_OUT, extra)
}

/// One request on a fresh connection, read up to its newline.
fn raw_request(socket: &Path, operation: &str) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = format!("{{\"protocol\":\"execute.daemon/1\",\"op\":\"{operation}\"}}\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let count = stream.read(&mut byte).unwrap();
        if count == 0 || byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    serde_json::from_slice(&line).unwrap()
}

#[test]
fn daemon_status_reports_not_running_when_no_socket_exists() {
    let runtime = tempfile::tempdir().unwrap();
    let output = daemon_output(runtime.path(), &["status"]);
    assert!(output.status.success(), "{:?}", output);
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["running"], false);
    assert_eq!(reply["protocol"], "execute.daemon/1");
}

#[test]
fn daemon_serve_answers_status_and_stop_removes_its_socket() {
    let runtime = tempfile::tempdir().unwrap();
    let socket = default_socket_under(runtime.path());
    let mut daemon = serve_in_background(runtime.path());

    let status = daemon_output(runtime.path(), &["status"]);
    let reply: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(reply["running"], true);
    assert!(socket.exists(), "socket at {}", socket.display());

    let stop = daemon_output(runtime.path(), &["stop"]);
    assert!(stop.status.success(), "{:?}", stop);
    let child = daemon.0.as_mut().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        assert!(Instant::now() < deadline, "daemon did not exit after stop");
        thread::sleep(Duration::from_millis(20));
    };
    assert!(exit.success());
    assert!(
        !socket.exists(),
        "socket left behind at {}",
        socket.display()
    );
    daemon.0 = None;
}

#[test]
fn daemon_serve_refuses_a_directory_that_is_not_private() {
    let runtime = tempfile::tempdir().unwrap();
    let directory = runtime.path().join("open");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = directory.join("daemon.sock");
    let output = daemon_output(
        runtime.path(),
        &["serve", "--socket", socket.to_str().unwrap()],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("private"), "{stderr}");
    assert!(!socket.exists());
}

#[test]
fn run_with_daemon_flag_prints_the_same_output_silently() {
    let runtime = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cold = run_program(runtime.path(), work.path(), &[]);
    assert!(cold.status.success(), "{:?}", cold);

    let _daemon = serve_in_background(runtime.path());
    let warm = run_program(runtime.path(), work.path(), &["--daemon"]);
    assert!(warm.status.success(), "{:?}", warm);
    assert_eq!(warm.stdout, cold.stdout);
    assert!(
        warm.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
}

#[test]
fn run_with_daemon_flag_without_a_daemon_is_silent_and_cold() {
    let runtime = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let output = run_program(runtime.path(), work.path(), &["--daemon"]);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn forwarded_run_exit_status_and_output_match_a_cold_run() {
    let runtime = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = "run printf with message \"a\"\nexit with status 7\n";
    let cold = run_source(runtime.path(), work.path(), source, &[]);
    assert_eq!(cold.status.code(), Some(7), "{:?}", cold);

    let _daemon = serve_in_background(runtime.path());
    let warm = run_source(runtime.path(), work.path(), source, &["--daemon"]);
    assert_eq!(warm.status.code(), Some(7), "{:?}", warm);
    assert_eq!(warm.stdout, cold.stdout);
}

#[test]
fn concurrent_forwarded_runs_each_get_their_own_output_and_status() {
    let runtime = tempfile::tempdir().unwrap();
    let _daemon = serve_in_background(runtime.path());
    let handles: Vec<_> = (0..4_i32)
        .map(|index| {
            let runtime = runtime.path().to_path_buf();
            thread::spawn(move || {
                let work = tempfile::tempdir().unwrap();
                let source = format!(
                    "run printf with message \"run{index}\"\nexit with status {}\n",
                    10 + index
                );
                (
                    index,
                    run_source(&runtime, work.path(), &source, &["--daemon"]),
                )
            })
        })
        .collect();
    for handle in handles {
        let (index, output) = handle.join().unwrap();
        assert_eq!(output.status.code(), Some(10 + index), "{:?}", output);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("run{index}\n")
        );
    }
}

#[test]
fn a_program_that_takes_the_terminal_is_declined_before_it_runs() {
    let runtime = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let _daemon = serve_in_background(runtime.path());
    let source = "interactively printf with message \"x\"\n";
    let output = run_source(runtime.path(), work.path(), source, &["--daemon"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("will not run this program"), "{stderr}");
}

#[test]
fn a_socket_in_a_non_private_directory_is_never_contacted() {
    let runtime = tempfile::tempdir().unwrap();
    let directory = runtime.path().join("open");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = directory.join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let contacted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&contacted);
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let _ = stream
                .write_all(b"{\"ok\":true,\"protocol\":\"execute.daemon/1\",\"running\":true}\n");
        }
    });
    let path = socket.to_str().unwrap();

    let status = daemon_output(runtime.path(), &["status", "--socket", path]);
    assert!(status.status.success(), "{:?}", status);
    let reply: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(reply["running"], false, "{reply}");
    let reason = reply["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("private"), "{reply}");

    let stop = daemon_output(runtime.path(), &["stop", "--socket", path]);
    assert!(!stop.status.success(), "stop must not succeed: {:?}", stop);
    let stderr = String::from_utf8_lossy(&stop.stderr);
    assert!(
        stderr.contains("refusing to use the daemon socket"),
        "{stderr}"
    );
    assert!(stderr.contains(path), "{stderr}");

    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        contacted.load(Ordering::SeqCst),
        0,
        "a request reached the untrusted socket"
    );
}

/// Measures the warm round trip (connect, request, reply) and the end-to-end wall time of a
/// forwarded run against a cold one. The figures are printed for CI; the assertion only
/// guards against a gross regression.
#[test]
fn warm_round_trip_and_forwarded_run_latency_are_measured() {
    let runtime = tempfile::tempdir().unwrap();
    let socket = default_socket_under(runtime.path());
    let _daemon = serve_in_background(runtime.path());

    let mut round_trips = Vec::new();
    for _ in 0..200 {
        let started = Instant::now();
        let reply = raw_request(&socket, "ping");
        round_trips.push(started.elapsed());
        assert_eq!(reply["ok"], true);
    }
    round_trips.sort();
    let p50 = round_trips[round_trips.len() / 2];
    let p95 = round_trips[round_trips.len() * 95 / 100];
    let ping_line =
        format!("daemon ping round trip (connect, request, reply): p50 {p50:?}, p95 {p95:?}");
    eprintln!("{ping_line}");
    report_latency(&ping_line);
    assert!(
        p50 < Duration::from_millis(20),
        "ping round trip p50 {p50:?} is far above the target"
    );

    let work = tempfile::tempdir().unwrap();
    let mut forwarded = Vec::new();
    let mut cold = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let output = run_program(runtime.path(), work.path(), &["--daemon"]);
        forwarded.push(started.elapsed());
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");

        let started = Instant::now();
        let output = run_program(runtime.path(), work.path(), &[]);
        cold.push(started.elapsed());
        assert!(output.status.success(), "{:?}", output);
    }
    forwarded.sort();
    cold.sort();
    let run_line = format!(
        "end-to-end run wall time: forwarded p50 {:?}, cold p50 {:?}",
        forwarded[forwarded.len() / 2],
        cold[cold.len() / 2]
    );
    eprintln!("{run_line}");
    report_latency(&run_line);
}

/// Passing tests have their stderr captured, so the figures also go to the CI job summary
/// when it is available, where they can be read without `--nocapture`.
fn report_latency(line: &str) {
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        use std::io::Write;
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(summary)
        {
            let _ = writeln!(file, "- {line}");
        }
    }
}

#[test]
fn a_program_with_a_prompt_input_is_declined_before_it_runs() {
    let runtime = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let _daemon = serve_in_background(runtime.path());
    let source = "accept name as text from prompt \"Your name?\"\nuse value name\n";
    // The supplied answer lets the cold fallback finish without a terminal.
    let output = run_source(
        runtime.path(),
        work.path(),
        source,
        &["--daemon", "--input", "name=Ada"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("will not run this program"), "{stderr}");
    assert!(output.status.success(), "{:?}", output);
}

#[test]
fn a_forwarded_run_creates_files_with_the_clients_umask() {
    let runtime = tempfile::tempdir().unwrap();
    let _daemon = serve_in_background(runtime.path());
    let source = "use value \"kept\"\nthen save to \"made.txt\"\n";
    for (label, extra) in [("cold", vec![]), ("forwarded", vec!["--daemon"])] {
        let work = tempfile::tempdir().unwrap();
        // SAFETY: umask is safe between fork and exec.
        let output = run_source_with(runtime.path(), work.path(), source, &extra, || {
            unsafe { libc::umask(0o077) };
            Ok(())
        });
        assert!(output.status.success(), "{label}: {:?}", output);
        let mode = fs::metadata(work.path().join("made.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "{label} run created the file with mode {mode:o}"
        );
    }
}

#[test]
fn a_forwarded_run_keeps_the_clients_file_size_limit() {
    let runtime = tempfile::tempdir().unwrap();
    let _daemon = serve_in_background(runtime.path());
    let source = format!(
        "use value \"{}\"\nthen save to \"big.txt\"\n",
        "x".repeat(64)
    );
    for (label, extra) in [("cold", vec![]), ("forwarded", vec!["--daemon"])] {
        let work = tempfile::tempdir().unwrap();
        let output = run_source_with(runtime.path(), work.path(), &source, &extra, || {
            let limit = libc::rlimit {
                rlim_cur: 10,
                rlim_max: 10,
            };
            // SAFETY: setrlimit is safe between fork and exec.
            unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) };
            Ok(())
        });
        assert!(
            !output.status.success(),
            "{label} run wrote past the limit: {:?}",
            output
        );
        let length = fs::metadata(work.path().join("big.txt"))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        assert!(
            length <= 10,
            "{label} run wrote {length} bytes past a 10-byte limit"
        );
    }
}
