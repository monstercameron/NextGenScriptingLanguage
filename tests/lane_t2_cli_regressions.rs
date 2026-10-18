use std::process::{Child, Command, ExitStatus, Stdio};

fn execute() -> Command {
    Command::new(env!("CARGO_BIN_EXE_execute"))
}

fn close_stdout(command: &mut Command) -> ExitStatus {
    let mut child: Child = command
        .stdout(Stdio::piped())
        .spawn()
        .expect("start execute");
    drop(child.stdout.take());
    child.wait().expect("wait for execute")
}

#[test]
fn help_usage_places_program_options_after_the_separator_and_loads_file_calls() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("sub")).unwrap();
    std::fs::write(
        directory.path().join("main.program"),
        "accept who as text with default \"you\"\nrun program file \"sub/child.program\" with who who\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("sub/child.program"),
        "accept who as text\n",
    )
    .unwrap();

    let output = execute()
        .args(["help", "main.program", "--json"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let object: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        object["usage"],
        "execute main.program [--input NAME=VALUE ...] [-- OPTIONS]"
    );
}

#[test]
fn help_exits_quietly_when_stdout_is_closed() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("tool.program"),
        "run \"true\" with arguments []\n",
    )
    .unwrap();
    let status = close_stdout(
        execute()
            .args(["help", "tool.program"])
            .current_dir(directory.path()),
    );
    assert_eq!(status.code(), Some(141));
}

#[test]
#[cfg(feature = "live")]
fn attach_exits_quietly_when_stdout_is_closed() {
    let directory = tempfile::tempdir().unwrap();
    let status = close_stdout(
        execute()
            .args(["attach", "missing.sock"])
            .current_dir(directory.path()),
    );
    assert_eq!(status.code(), Some(141));
}

#[test]
#[cfg(feature = "agent-advisory")]
fn checkpoint_status_exits_quietly_when_stdout_is_closed() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("checkpoint");
    std::fs::create_dir_all(checkpoint.join("blobs")).unwrap();
    std::fs::write(
        checkpoint.join("journal.json"),
        r#"{"data":{"kind":"linear","status":"completed","completed":[],"identity":{}}}"#,
    )
    .unwrap();
    let checkpoint = checkpoint.to_string_lossy().into_owned();
    let status = close_stdout(
        execute()
            .args(["checkpoint", "status", &checkpoint])
            .current_dir(directory.path()),
    );
    assert_eq!(status.code(), Some(141));
}

#[test]
#[cfg(feature = "records")]
fn show_run_exits_quietly_when_stdout_is_closed() {
    let directory = tempfile::tempdir().unwrap();
    let (command, arguments) = if cfg!(windows) {
        ("cmd", vec!["/c", "echo", "hello"])
    } else {
        ("printf", vec!["hello\n"])
    };
    let program = directory.path().join("tool.program");
    let record = directory.path().join("record");
    std::fs::write(
        &program,
        format!("run \"{command}\" with arguments {arguments:?}\n"),
    )
    .unwrap();
    let recorded = execute()
        .arg("run")
        .arg(&program)
        .arg("--record")
        .arg(&record)
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );

    let status = close_stdout(
        execute()
            .arg("show-run")
            .arg(&record)
            .current_dir(directory.path()),
    );
    assert_eq!(status.code(), Some(141));
}

#[test]
#[cfg(feature = "records")]
fn replay_exits_quietly_when_stdout_is_closed() {
    let directory = tempfile::tempdir().unwrap();
    let (command, arguments) = if cfg!(windows) {
        ("cmd", vec!["/c", "echo", "hello"])
    } else {
        ("printf", vec!["hello\n"])
    };
    let program = directory.path().join("tool.program");
    let record = directory.path().join("record");
    std::fs::write(
        &program,
        format!("run \"{command}\" with arguments {arguments:?}\n"),
    )
    .unwrap();
    let recorded = execute()
        .arg("run")
        .arg(&program)
        .arg("--record")
        .arg(&record)
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );

    let status = close_stdout(
        execute()
            .arg("replay")
            .arg(&record)
            .current_dir(directory.path()),
    );
    assert_eq!(status.code(), Some(141));
}

#[test]
#[cfg(feature = "records")]
fn repl_exits_quietly_when_stdout_is_closed() {
    use std::io::Write;
    let mut child = execute()
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    child.stdin.take().unwrap().write_all(b":help\n").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(141));
}
