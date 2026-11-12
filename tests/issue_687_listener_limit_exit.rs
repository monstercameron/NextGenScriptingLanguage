//! A listener producer that exits while its handler runs must be reported when the handler
//! reaches `--max-runs`, instead of the limit path reporting success from a stale observation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// Writes a producer that emits one record, waits until the handler has created `started`,
/// and exits with `exit_code`, plus a handler that creates `started` and then runs for about
/// two seconds. The producer therefore exits while the handler is still running.
fn write_scripts(directory: &Path, exit_code: i32) -> (PathBuf, PathBuf, PathBuf) {
    let started = directory.join("handler-started");
    let producer = directory.join(if cfg!(windows) {
        "producer.cmd"
    } else {
        "producer.sh"
    });
    let handler = directory.join(if cfg!(windows) {
        "handler.cmd"
    } else {
        "handler.sh"
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::write(
            &producer,
            format!(
                "#!/bin/sh\nprintf 'record\\n'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nexit {exit_code}\n",
                started.display()
            ),
        )
        .unwrap();
        fs::write(&handler, format!(": > '{}'\nsleep 2\n", started.display())).unwrap();
        for script in [&producer, &handler] {
            fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    #[cfg(windows)]
    {
        fs::write(
            &producer,
            format!(
                "@echo off\r\necho record\r\n:wait\r\nif exist \"{}\" goto done\r\nping -n 1 -w 50 127.0.0.1 >nul\r\ngoto wait\r\n:done\r\nexit /b {exit_code}\r\n",
                started.display()
            ),
        )
        .unwrap();
        fs::write(
            &handler,
            format!(
                "@echo off\r\necho.> \"{}\"\r\nping -n 3 127.0.0.1 >nul\r\n",
                started.display()
            ),
        )
        .unwrap();
    }
    (producer, handler, started)
}

/// Windows refuses batch files as direct binaries, so the script runs through an explicit
/// `cmd.exe /C` invocation there. Unix runs the script directly.
fn applet_manifest(name: &str, script: &Path) -> serde_json::Value {
    #[cfg(unix)]
    let (binary, arguments) = (script.display().to_string(), serde_json::json!([]));
    #[cfg(windows)]
    let (binary, arguments) = (
        std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string()),
        serde_json::json!([
            {"literal": "/C"},
            {"literal": script.display().to_string()}
        ]),
    );
    serde_json::json!({
        "name": name,
        "binary": binary,
        "arguments": arguments
    })
}

fn run_with_limit_one(exit_code: i32) -> (Output, PathBuf, TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let (producer, handler, started) = write_scripts(directory.path(), exit_code);
    let manifests = directory.path().join("manifests");
    fs::create_dir_all(&manifests).unwrap();
    fs::write(
        manifests.join("producer.json"),
        serde_json::to_vec(&applet_manifest("producer", &producer)).unwrap(),
    )
    .unwrap();
    fs::write(
        manifests.join("handler.json"),
        serde_json::to_vec(&applet_manifest("handler", &handler)).unwrap(),
    )
    .unwrap();
    let workflow = directory.path().join("listener.program");
    fs::write(
        &workflow,
        "listen to producer\nwhenever a line arrives as record\n  run handler\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(&manifests)
        .args(["--max-runs", "1"])
        .output()
        .unwrap();
    (output, started, directory)
}

#[test]
fn producer_exit_during_handler_fails_at_run_limit() {
    let (output, started, _directory) = run_with_limit_one(7);
    assert!(started.exists(), "handler never ran");
    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("listener failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn producer_success_during_handler_terminates_cleanly_at_run_limit() {
    let (output, started, _directory) = run_with_limit_one(0);
    assert!(started.exists(), "handler never ran");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
