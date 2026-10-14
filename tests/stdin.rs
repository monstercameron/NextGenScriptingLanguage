#![cfg(unix)]
//! Finite stdin composition through the real CLI, native pipes and checkpoints.
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn run(root: &Path, source: &str, input: &[u8], extra: &[&str]) -> Output {
    let script = root.join("job.program");
    let data = root.join("stdin.bin");
    fs::write(&script, source).unwrap();
    fs::write(&data, input).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(script)
        .args(extra)
        .current_dir(root)
        .stdin(fs::File::open(data).unwrap())
        .output()
        .unwrap()
}
fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl OwnedChild {
    fn wait(&mut self) {
        let until = Instant::now() + Duration::from_secs(3);
        while self.0.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < until,
                "stdin operation did not stop within three seconds"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn preserves_arbitrary_binary_stdin_and_capture_boundaries() {
    let root = tempfile::tempdir().unwrap();
    for input in [b"".as_slice(), b"a\0\xff\nlast".as_slice()] {
        assert_eq!(
            success(run(
                root.path(),
                "read from stdin\n",
                input,
                &["--max-output-bytes", &input.len().to_string()]
            )),
            input
        );
    }
    let fail = run(
        root.path(),
        "read from stdin\nthen save to \"forbidden\"\n",
        b"abc",
        &["--max-output-bytes", "2"],
    );
    assert!(!fail.status.success());
    assert!(fail.stdout.is_empty());
    assert!(!root.path().join("forbidden").exists());
}

#[test]
fn named_stdin_is_text_and_can_supply_literal_native_arguments() {
    let root = tempfile::tempdir().unwrap();
    let source = "read from stdin as message\nrun \"printf\" with arguments [\"<%s>\", message]\n";
    assert_eq!(
        success(run(root.path(), source, b"$HOME; *\n", &[])),
        b"<$HOME; *\n>"
    );
    let fail = run(root.path(), source, b"\xff", &[]);
    assert!(!fail.status.success());
    assert!(String::from_utf8_lossy(&fail.stderr).contains("UTF-8"));
}

#[test]
fn native_stages_consume_more_input_than_the_finite_output_limit() {
    let root = tempfile::tempdir().unwrap();
    let input = vec![b'x'; 8 * 1024 * 1024];
    let output = success(run(
        root.path(),
        "read from stdin\npipe to \"wc\" with arguments [\"-c\"]\n",
        &input,
        &["--max-output-bytes", "32"],
    ));
    assert_eq!(
        String::from_utf8(output).unwrap().trim(),
        input.len().to_string()
    );
}

#[test]
fn existing_file_bindings_named_stdin_and_from_keep_their_meaning() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("data.txt"), b"file input").unwrap();
    for name in ["stdin", "from"] {
        let source = format!("accept {name} as text with default \"data.txt\"\nread {name}\n");
        assert_eq!(
            success(run(root.path(), &source, b"ignored stdin", &[])),
            b"file input"
        );
    }
    for source in [
        "read from stdin file",
        "read from stdin extra",
        "read from stdin as false",
    ] {
        assert!(flow::parser::parse(source).is_err(), "{source}");
    }
}

#[test]
fn check_explain_and_inspect_never_read_a_quiet_stdin_pipe() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("job.program");
    fs::write(&script, "read from stdin as text\nthen \"cat\"\n").unwrap();
    for command in ["check", "explain", "inspect"] {
        let mut child = OwnedChild(
            Command::new(env!("CARGO_BIN_EXE_execute"))
                .arg(command)
                .arg(&script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        child.wait();
        assert!(child.0.try_wait().unwrap().unwrap().success());
        if command == "inspect" {
            let report: serde_json::Value =
                serde_json::from_reader(child.0.stdout.take().unwrap()).unwrap();
            assert_eq!(report["stdin_mode"], "bytes");
        }
    }
}

#[test]
fn quiet_stdin_captures_observe_deadlines_and_signals() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("job.program");
    for source in [
        "read from stdin\nthen save to \"forbidden\"\n",
        "read from stdin\npipe to \"cat\"\nthen save to \"forbidden\"\n",
    ] {
        fs::write(&script, source).unwrap();
        for cancel in [false, true] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
            command
                .arg(&script)
                .current_dir(root.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if !cancel {
                command.args(["--command-timeout-ms", "30"]);
            }
            let mut child = OwnedChild(command.spawn().unwrap());
            if cancel {
                thread::sleep(Duration::from_millis(50));
                assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
            }
            child.wait();
            assert!(!child.0.try_wait().unwrap().unwrap().success());
            assert!(!root.path().join("forbidden").exists());
        }
    }
}

#[test]
fn inherited_stdin_capture_preserves_shared_descriptor_flags() {
    use std::os::fd::AsRawFd;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("input");
    fs::write(&file, b"data").unwrap();
    let input = fs::File::open(file).unwrap();
    let initial = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) };
    let script = root.path().join("job.program");
    fs::write(&script, "read from stdin\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg(script)
        .stdin(input.try_clone().unwrap())
        .output()
        .unwrap();
    assert_eq!(success(output), b"data");
    assert_eq!(
        unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) },
        initial
    );
}

#[test]
fn managed_programs_reject_process_stdin_without_opening_a_control_socket() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("job.program");
    let socket = root.path().join("control.sock");
    fs::write(&script, "read from stdin\nthen save to \"forbidden\"\n").unwrap();
    let mut child = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["live", "serve"])
            .arg(script)
            .arg("--socket")
            .arg(&socket)
            .arg("--paused")
            .current_dir(root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    child.wait();
    assert!(!child.0.try_wait().unwrap().unwrap().success());
    assert!(!socket.exists());
    assert!(!root.path().join("forbidden").exists());
}

#[test]
fn two_program_files_compose_through_an_ordinary_unix_pipe() {
    let root = tempfile::tempdir().unwrap();
    let upper = root.path().join("upper.program");
    let count = root.path().join("count.program");
    fs::write(
        &upper,
        "read from stdin\npipe to \"tr\" with arguments [\"[:lower:]\", \"[:upper:]\"]\n",
    )
    .unwrap();
    fs::write(
        &count,
        "read from stdin\npipe to \"wc\" with arguments [\"-l\"]\n",
    )
    .unwrap();
    let mut first = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(upper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut second = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(count)
            .stdin(first.0.stdout.take().unwrap())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    first
        .0
        .stdin
        .take()
        .unwrap()
        .write_all(b"one\ntwo\n")
        .unwrap();
    first.wait();
    second.wait();
    assert!(first.0.try_wait().unwrap().unwrap().success());
    assert!(second.0.try_wait().unwrap().unwrap().success());
    let mut output = String::new();
    std::io::Read::read_to_string(&mut second.0.stdout.take().unwrap(), &mut output).unwrap();
    assert_eq!(output.trim(), "2");
}

#[test]
fn completed_checkpoint_reuses_captured_input_without_reading_replacement_stdin() {
    let root = tempfile::tempdir().unwrap();
    let source = "read from stdin\nthen save to \"saved\"\n";
    let first = success(run(
        root.path(),
        source,
        b"original\0\xff",
        &["--checkpoint", "journal"],
    ));
    fs::write(root.path().join("replacement"), b"replacement").unwrap();
    let again = success(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("resume")
            .arg(root.path().join("job.program"))
            .args(["--checkpoint", "journal"])
            .current_dir(root.path())
            .stdin(fs::File::open(root.path().join("replacement")).unwrap())
            .output()
            .unwrap(),
    );
    assert_eq!(first, again);
    assert_eq!(fs::read(root.path().join("saved")).unwrap(), first);
}

#[test]
fn interrupted_stdin_checkpoint_requires_explicit_retry_before_consuming_again() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("job.program");
    fs::write(&script, "read from stdin\n").unwrap();
    let mut child = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(&script)
            .args(["--checkpoint", "journal", "--command-timeout-ms", "30"])
            .current_dir(root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    child.wait();
    assert!(!child.0.try_wait().unwrap().unwrap().success());
    fs::write(root.path().join("input"), b"retry input").unwrap();
    for retry in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .arg("resume")
            .arg(&script)
            .args(["--checkpoint", "journal"])
            .current_dir(root.path())
            .stdin(fs::File::open(root.path().join("input")).unwrap());
        if retry {
            command.arg("--retry-uncertain");
        }
        let result = command.output().unwrap();
        if retry {
            assert_eq!(success(result), b"retry input");
        } else {
            assert!(!result.status.success());
            assert!(String::from_utf8_lossy(&result.stderr).contains("--retry-uncertain"));
        }
    }
}

#[test]
fn services_and_screen_handlers_reject_process_stdin_before_prior_effects() {
    let root = tempfile::tempdir().unwrap();
    for source in [
        "every 1 second\n  run \"sh\" with arguments [\"-c\", \"touch forbidden\"]\n  read from stdin\n",
        "open a screen titled \"Test\"\nplace app on the left\ndefine a program named app\n  show a text panel named output with text \"ready\"\n  whenever the program opens\n    run \"sh\" with arguments [\"-c\", \"touch forbidden\"]\n    read from stdin\n",
    ] {
        let result = run(root.path(), source, b"data", &[]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("stdin"));
        assert!(!root.path().join("forbidden").exists());
    }
}
