#![cfg(feature = "live")]
//! `execute attach SOCKET [--control ACTION]`: argument checks and a missing socket
//! are portable. The live-server round trip uses a unix socket path, so it is
//! unix-only.

use serde_json::Value;
use std::process::{Command, Output, Stdio};

fn attach(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("attach")
        .args(arguments)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("start execute")
}

#[test]
fn attach_refuses_bad_arguments_before_any_child_process_starts() {
    let no_socket = attach(&[]);
    assert!(!no_socket.status.success());
    assert!(String::from_utf8_lossy(&no_socket.stderr).contains("usage: execute attach"));

    let unknown = attach(&["some.sock", "--control", "explode"]);
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown control action"));
    assert!(unknown.stdout.is_empty());

    let missing_action = attach(&["some.sock", "--control"]);
    assert!(!missing_action.status.success());
    assert!(String::from_utf8_lossy(&missing_action.stderr).contains("--control requires"));

    let two_sockets = attach(&["a.sock", "b.sock"]);
    assert!(!two_sockets.status.success());
    assert!(String::from_utf8_lossy(&two_sockets.stderr).contains("takes one socket"));

    let unknown_option = attach(&["some.sock", "--force"]);
    assert!(!unknown_option.status.success());
    assert!(String::from_utf8_lossy(&unknown_option.stderr).contains("unknown option"));
}

#[test]
fn attach_to_a_missing_socket_reports_a_json_error_and_exits_nonzero() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("missing.sock");
    let output = attach(&[&socket.to_string_lossy()]);
    assert!(!output.status.success());
    let object: Value = serde_json::from_slice(&output.stdout).expect("a JSON error object");
    assert_eq!(object["ok"], false);
}

#[cfg(unix)]
mod live_server {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for_socket(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "live server did not create its socket"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn attach_reads_the_revision_and_applies_a_guarded_control_action() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("tool.program"),
            "accept name as text with default \"x\"\nthen encode name as json\n",
        )
        .unwrap();
        let socket = directory.path().join("tool.sock");
        let state = directory.path().join("state.json");
        // --state-file is the initial state, which must already be a JSON object.
        std::fs::write(&state, "{}").unwrap();
        let mut server = Command::new(env!("CARGO_BIN_EXE_execute"))
            .args([
                "live",
                "serve",
                "tool.program",
                "--state-file",
                &state.to_string_lossy(),
                "--socket",
                &socket.to_string_lossy(),
                "--paused",
            ])
            .current_dir(directory.path())
            .env_remove("EXECUTE_MODELS_OFFLINE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for_socket(&socket);
        let socket_text = socket.to_string_lossy().into_owned();

        let read = attach(&[&socket_text]);
        assert!(
            read.status.success(),
            "{}",
            String::from_utf8_lossy(&read.stderr)
        );
        let object: Value = serde_json::from_slice(&read.stdout).unwrap();
        assert_eq!(object["protocol"], "execute.attach/1");
        assert!(object["revision"].as_u64().is_some(), "{object}");
        assert!(!object["runtime"].is_null(), "{object}");

        let control = attach(&[&socket_text, "--control", "resume"]);
        let applied: Value = serde_json::from_slice(&control.stdout).unwrap();
        assert_eq!(applied["protocol"], "execute.attach/1");
        assert_eq!(applied["action"], "resume");
        assert_eq!(applied["applied"], true, "{applied}");
        assert!(applied["revision_before"].as_u64().is_some());
        assert!(control.status.success());

        let stop = Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["live", "control", "stop", "--socket", &socket_text])
            .env_remove("EXECUTE_MODELS_OFFLINE")
            .output()
            .unwrap();
        assert!(
            stop.status.success(),
            "{}",
            String::from_utf8_lossy(&stop.stdout)
        );
        let _ = server.wait();
    }
}
