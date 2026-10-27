//! `hand over to` replaces the script with one native program, and `launch detached`
//! starts one that the script does not own. These tests cover the grammar and the
//! placement and mode rules, a detached launch that returns before its child
//! finishes, and exit status through a hand-over on each platform.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn execute(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(directory)
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
    // waiting for it. The script continues and finishes first.
    let child = if cfg!(windows) {
        "\"C:/Windows/System32/cmd.exe\" with arguments [\"/c\", \"ping -n 2 127.0.0.1 >nul & echo done\"]"
    } else {
        "\"/bin/sh\" with arguments [\"-c\", \"sleep 0.5; printf done\"]"
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
fn detached_output_follows_the_write_policy_before_the_child_starts() {
    // A policy with no grants forbids the write, so the launch is refused before
    // any child starts and the output file is never created.
    let directory = tempdir().unwrap();
    let policy = directory.path().join("policy.json");
    fs::write(&policy, r#"{"version":1,"applets":{}}"#).unwrap();
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
