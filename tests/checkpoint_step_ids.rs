//! Program checkpoints match completed nodes by the id of their first step (#34). A
//! node is reused while it sits where the completed prefix puts it. A statement that
//! was inserted or removed above it is refused, with the statement and both lines.
//! Edits below it, and comments above it, resume without running the node again.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// `ctrl.count` appends a line to a log on every run and echoes its text. `ctrl.echo`
/// echoes its text. `ctrl.fail` exits with status 1.
struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    manifests: PathBuf,
    count_log: PathBuf,
}

/// Write a command applet's script and manifest. `body` is the script's text after
/// the shebang or `@echo off` line, with `$1` or `%1` as the text argument.
fn write_applet(manifests: &Path, root: &Path, name: &str, body_sh: &str, body_bat: &str) {
    let (binary, arguments) = if cfg!(windows) {
        let script = root.join(format!("{name}.bat"));
        fs::write(&script, format!("@echo off\r\n{body_bat}\r\n")).unwrap();
        (
            "cmd.exe",
            serde_json::json!([
                {"literal": "/d"},
                {"literal": "/c"},
                {"literal": script.to_string_lossy()},
                {"parameter": "text"}
            ]),
        )
    } else {
        let script = root.join(format!("{name}.sh"));
        fs::write(&script, format!("#!/bin/sh\n{body_sh}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        (
            "/bin/sh",
            serde_json::json!([
                {"literal": script.to_string_lossy()},
                {"parameter": "text"}
            ]),
        )
    };
    let manifest = serde_json::json!({
        "name": name,
        "binary": binary,
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {"text": {"type": "string", "required": true}},
        "arguments": arguments,
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw", "trim_final_lf": true},
            "outcomes": [{"status": 0, "name": "success", "kind": "success"}],
            "capabilities": [],
            "effects": []
        }
    });
    fs::write(manifests.join(format!("{name}.json")), manifest.to_string()).unwrap();
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let manifests = root.join("manifests");
    fs::create_dir(&manifests).unwrap();
    let count_log = root.join("count.log");
    write_applet(
        &manifests,
        &root,
        "ctrl.count",
        &format!(
            "echo run >> '{}'\nprintf '%s\\n' \"$1\"",
            count_log.display()
        ),
        &format!("echo run>> \"{}\"\r\necho %1", count_log.display()),
    );
    write_applet(
        &manifests,
        &root,
        "ctrl.echo",
        "printf '%s\\n' \"$1\"",
        "echo %1",
    );
    write_applet(&manifests, &root, "ctrl.fail", "exit 1", "exit /b 1");
    Fixture {
        _directory: directory,
        root,
        manifests,
        count_log,
    }
}

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .expect("start execute")
}

fn log_lines(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// Run a checkpointed program that stops at `ctrl.fail`, after the labelled node.
fn checkpoint_after_labelled_node(fixture: &Fixture, checkpoint: &Path) {
    fs::write(
        fixture.root.join("p.program"),
        "fetch: run ctrl.count with text \"a\"\nrun ctrl.fail with text \"x\"\n",
    )
    .unwrap();
    let run = execute(
        &fixture.root,
        &[
            "run",
            "p.program",
            "--manifest-dir",
            &fixture.manifests.to_string_lossy(),
            "--checkpoint",
            &checkpoint.to_string_lossy(),
        ],
    );
    assert!(
        !run.status.success(),
        "the checkpointed run should stop at `ctrl.fail`"
    );
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "the labelled node ran once"
    );
}

/// Resume after the failed `ctrl.fail` node. That node is not retry-safe and its
/// failure may have had partial effects, so retrying it is an explicit decision (#150).
fn resume(fixture: &Fixture, checkpoint: &Path, program: &str) -> Output {
    fs::write(fixture.root.join("p.program"), program).unwrap();
    execute(
        &fixture.root,
        &[
            "resume",
            "p.program",
            "--manifest-dir",
            &fixture.manifests.to_string_lossy(),
            "--checkpoint",
            &checkpoint.to_string_lossy(),
            "--retry-uncertain",
        ],
    )
}

#[test]
fn inserting_a_statement_above_a_completed_labelled_node_is_refused_with_both_lines() {
    let fixture = fixture();
    let checkpoint = fixture.root.join("checkpoint");
    checkpoint_after_labelled_node(&fixture, &checkpoint);

    let refused = resume(
        &fixture,
        &checkpoint,
        "run ctrl.echo with text \"new\"\nfetch: run ctrl.count with text \"a\"\nrun ctrl.echo with text \"two\"\n",
    );
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("\"fetch\""), "{stderr}");
    assert!(stderr.contains("moved from line 1 to line 2"), "{stderr}");
    assert!(
        stderr.contains("a statement was inserted or removed above completed node"),
        "{stderr}"
    );
    assert!(stderr.contains("use a fresh checkpoint"), "{stderr}");
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "the completed node must not run again"
    );
}

#[test]
fn an_edit_below_the_last_completed_node_resumes_without_rerunning_it() {
    let fixture = fixture();
    let checkpoint = fixture.root.join("checkpoint");
    checkpoint_after_labelled_node(&fixture, &checkpoint);

    let resumed = resume(
        &fixture,
        &checkpoint,
        "fetch: run ctrl.count with text \"a\"\nrun ctrl.echo with text \"after\"\n",
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&resumed.stdout).trim(), "after");
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "the completed node must not run again"
    );
}

#[test]
fn a_comment_above_a_completed_labelled_node_resumes_without_rerunning_it() {
    let fixture = fixture();
    let checkpoint = fixture.root.join("checkpoint");
    checkpoint_after_labelled_node(&fixture, &checkpoint);

    // The comment moves the labelled node down a line, but no statement changes order.
    let resumed = resume(
        &fixture,
        &checkpoint,
        "# a note added above the node\nfetch: run ctrl.count with text \"a\"\nrun ctrl.echo with text \"after\"\n",
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "the completed node must not run again"
    );
}

#[test]
fn removing_a_completed_statement_above_a_labelled_node_is_refused() {
    let fixture = fixture();
    let checkpoint = fixture.root.join("checkpoint");
    fs::write(
        fixture.root.join("p.program"),
        "run ctrl.echo with text \"pre\"\nfetch: run ctrl.count with text \"a\"\nrun ctrl.fail with text \"x\"\n",
    )
    .unwrap();
    let run = execute(
        &fixture.root,
        &[
            "run",
            "p.program",
            "--manifest-dir",
            &fixture.manifests.to_string_lossy(),
            "--checkpoint",
            &checkpoint.to_string_lossy(),
        ],
    );
    assert!(!run.status.success());
    assert_eq!(log_lines(&fixture.count_log), 1);

    let refused = resume(
        &fixture,
        &checkpoint,
        "fetch: run ctrl.count with text \"a\"\nrun ctrl.echo with text \"two\"\n",
    );
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("no longer in the script"), "{stderr}");
    assert!(stderr.contains("(line 1)"), "{stderr}");
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "the completed node must not run again"
    );
}

#[test]
fn renaming_the_label_of_a_completed_node_resumes_without_rerunning_it() {
    let fixture = fixture();
    let checkpoint = fixture.root.join("checkpoint");
    checkpoint_after_labelled_node(&fixture, &checkpoint);

    // A label is an alias for the node's id (#186): the statement keeps its place and
    // its content, so the completed node is reused under its new name.
    let resumed = resume(
        &fixture,
        &checkpoint,
        "download: run ctrl.count with text \"a\"\nrun ctrl.echo with text \"after\"\n",
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&resumed.stdout).trim(), "after");
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "the completed node must not run again"
    );
}

#[test]
fn a_relabelled_completed_node_with_changed_content_is_refused() {
    let fixture = fixture();
    let checkpoint = fixture.root.join("checkpoint");
    checkpoint_after_labelled_node(&fixture, &checkpoint);

    let refused = resume(
        &fixture,
        &checkpoint,
        "download: run ctrl.count with text \"b\"\nrun ctrl.echo with text \"after\"\n",
    );
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("\"fetch\""), "{stderr}");
    assert!(stderr.contains("no longer in the script"), "{stderr}");
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "the completed node must not run again"
    );
}
