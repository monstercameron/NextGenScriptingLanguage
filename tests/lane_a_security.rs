//! Capability-policy regressions for the Hyprland listener and dispatcher
//! manifests. Address syntax and dispatcher mini-languages are validated before
//! any process starts. The Unix-only tests put a fake tool first on PATH and
//! assert that the refused runs never reach it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn execute(directory: &Path, subcommand: &str, source: &str, inputs: &[(&str, &str)]) -> Output {
    let file = directory.join("workflow.program");
    fs::write(&file, source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory)
        .arg(subcommand)
        .arg(&file)
        .arg("--manifest-dir")
        .arg(manifests());
    for (name, value) in inputs {
        command.arg("--input").arg(format!("{name}={value}"));
    }
    command.output().unwrap()
}

fn explain(source: &str) -> Output {
    let directory = TempDir::new().unwrap();
    execute(directory.path(), "explain", source, &[])
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn socket_address_renders_the_unix_connect_family_only() {
    let output =
        explain("run hypr.socket2 with socket_path \"/run/user/1000/hypr/sig/.socket2.sock\"\n");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output)
            .contains(r#"argv=["-U","-","UNIX-CONNECT:/run/user/1000/hypr/sig/.socket2.sock"]"#),
        "{}",
        stdout(&output)
    );
}

#[test]
fn socket_address_rejects_address_language_and_unsafe_paths() {
    let refused = [
        "EXEC:/bin/sh",
        "SYSTEM:id",
        "OPEN:/etc/passwd",
        "/run/x,fork",
        "/run/x:y",
        "/run/x!y",
        "/run/a b",
        "/run/tab\there",
        "/run/../etc/passwd",
        "relative/socket.sock",
        "",
    ];
    for path in refused {
        let source = format!("run hypr.socket2 with socket_path {path:?}\n");
        let output = explain(&source);
        assert!(
            !output.status.success(),
            "accepted {path:?}: {}",
            stdout(&output)
        );
        assert!(!stdout(&output).contains("socat"), "{path:?} reached argv");
    }
}

#[test]
fn generic_dispatch_refuses_exec_and_execr_in_any_case() {
    // The generic manifest allows only named non-spawning dispatchers as whole
    // first items. Option-shaped items (-j), blank items, sub-word forms and
    // unknown names are refused alongside exec and execr.
    for arguments in [
        r#"["exec", "kitty"]"#,
        r#"["EXEC", "kitty"]"#,
        r#"["Execr", "rm -rf x"]"#,
        r#"["-j", "exec", "kitty"]"#,
        r#"[" exec", "kitty"]"#,
        r#"["", "x"]"#,
        r#"["   ", "x"]"#,
        r#"["exec kitty"]"#,
        r#"["exec,", "kitty"]"#,
        r#"["(exec)", "kitty"]"#,
        r#"["unknown", "x"]"#,
    ] {
        let source = format!("run hypr.dispatch with arguments {arguments}\n");
        let output = explain(&source);
        assert!(
            !output.status.success(),
            "accepted {arguments}: {}",
            stdout(&output)
        );
        assert!(
            !stdout(&output).contains("hyprctl"),
            "{arguments} reached argv"
        );
    }
    let ordinary = explain("run hypr.dispatch with arguments [\"workspace\", \"2\"]\n");
    assert!(ordinary.status.success(), "{}", stderr(&ordinary));
    assert!(stdout(&ordinary).contains(r#"argv=["dispatch","workspace","2"]"#));
}

#[test]
fn exec_dispatch_manifest_accepts_only_process_dispatchers_and_declares_spawn() {
    let accepted = explain("run hypr.dispatch_exec with arguments [\"EXEC\", \"kitty\"]\n");
    assert!(accepted.status.success(), "{}", stderr(&accepted));
    assert!(stdout(&accepted).contains(r#"argv=["dispatch","EXEC","kitty"]"#));

    let refused = explain("run hypr.dispatch_exec with arguments [\"workspace\", \"2\"]\n");
    assert!(!refused.status.success());

    let inspect = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["applets", "inspect", "hypr.dispatch_exec", "--manifest-dir"])
        .arg(manifests())
        .output()
        .unwrap();
    assert!(inspect.status.success(), "{}", stderr(&inspect));
    let text = stdout(&inspect);
    assert!(text.contains("process.spawn"), "{text}");
    assert!(text.contains("compositor.dispatch"), "{text}");

    let generic = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["applets", "inspect", "hypr.dispatch", "--manifest-dir"])
        .arg(manifests())
        .output()
        .unwrap();
    assert!(!stdout(&generic).contains("process.spawn"));
}

#[test]
fn option_and_separator_items_are_refused_in_every_position() {
    for arguments in [
        // A later --batch runs ";"-separated commands, including exec.
        r#"["workspace", "--batch", "1;dispatch exec x"]"#,
        r#"["workspace", "-j"]"#,
        r#"["workspace", "1;2"]"#,
        r#"["workspace", "2\n"]"#,
        r#"["resizeactive", "-x", "0"]"#,
        r#"["exec", "--batch", "kitty"]"#,
    ] {
        let source = format!("run hypr.dispatch with arguments {arguments}\n");
        let output = explain(&source);
        assert!(
            !output.status.success(),
            "accepted {arguments}: {}",
            stdout(&output)
        );
        assert!(
            !stdout(&output).contains("hyprctl"),
            "{arguments} reached argv"
        );
    }
    let exec_batch =
        explain("run hypr.dispatch_exec with arguments [\"exec\", \"--batch\", \"x\"]\n");
    assert!(!exec_batch.status.success(), "{}", stdout(&exec_batch));
}

#[test]
fn plain_numbers_remain_usable_after_the_option_rule() {
    for amount in ["-10", "-10%", "10.5%", "-0.5", "3"] {
        let source =
            format!("run hypr.dispatch with arguments [\"resizeactive\", \"{amount}\", \"0\"]\n");
        let output = explain(&source);
        assert!(output.status.success(), "{amount}: {}", stderr(&output));
        assert!(
            stdout(&output).contains(&format!(
                r#"argv=["dispatch","resizeactive","{amount}","0"]"#
            )),
            "{amount}: {}",
            stdout(&output)
        );
    }
}

#[cfg(unix)]
mod runtime {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const FAKE: &str = r#"#!/bin/sh
name=$(basename "$0")
printf '%s\n' "$name $*" >> "$FAKE_LOG"
exit 0
"#;

    fn fake_bin(names: &[&str]) -> TempDir {
        let directory = TempDir::new().unwrap();
        for name in names {
            let path = directory.path().join(name);
            fs::write(&path, FAKE).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        directory
    }

    fn run_with_fakes(source: &str, inputs: &[(&str, &str)], bin: &TempDir, log: &Path) -> Output {
        let directory = TempDir::new().unwrap();
        let file = directory.path().join("workflow.program");
        fs::write(&file, source).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .current_dir(directory.path())
            .arg(&file)
            .arg("--manifest-dir")
            .arg(manifests());
        for (name, value) in inputs {
            command.arg("--input").arg(format!("{name}={value}"));
        }
        let path = format!(
            "{}:{}",
            bin.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command.env("PATH", path).env("FAKE_LOG", log);
        command.output().unwrap()
    }

    #[test]
    fn bound_exec_dispatcher_is_refused_before_spawn() {
        let bin = fake_bin(&["hyprctl"]);
        let log = bin.path().join("calls.log");
        let source = "accept word as text\nrun hypr.dispatch with arguments [word, \"kitty\"]\n";
        for word in ["exec", "EXEC", "execr"] {
            let output = run_with_fakes(source, &[("word", word)], &bin, &log);
            assert!(!output.status.success(), "{word} ran: {}", stdout(&output));
            assert!(!log.exists(), "{word} spawned hyprctl");
        }
    }

    #[test]
    fn ordinary_dispatcher_runs_through_the_generic_manifest() {
        let bin = fake_bin(&["hyprctl"]);
        let log = bin.path().join("calls.log");
        let source = "accept word as text\nrun hypr.dispatch with arguments [word, \"2\"]\n";
        let output = run_with_fakes(source, &[("word", "workspace")], &bin, &log);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "hyprctl dispatch workspace 2\n"
        );
    }

    #[test]
    fn bound_batch_and_option_items_are_refused_before_spawn() {
        let bin = fake_bin(&["hyprctl"]);
        let log = bin.path().join("calls.log");
        let source =
            "accept word as text\nrun hypr.dispatch with arguments [\"workspace\", word]\n";
        for item in ["--batch", "-j", "1;dispatch exec x"] {
            let output = run_with_fakes(source, &[("word", item)], &bin, &log);
            assert!(
                !output.status.success(),
                "{item:?} ran: {}",
                stdout(&output)
            );
            assert!(!log.exists(), "{item:?} spawned hyprctl");
        }
    }

    #[test]
    fn unsafe_socket_path_is_refused_before_socat_starts() {
        let bin = fake_bin(&["socat"]);
        let log = bin.path().join("calls.log");
        let source = "accept socket as text\nrun hypr.socket2 with socket_path socket\n";
        for socket in ["EXEC:/bin/sh", "/run/x,fork", "/run/../x"] {
            let output = run_with_fakes(source, &[("socket", socket)], &bin, &log);
            assert!(
                !output.status.success(),
                "{socket} ran: {}",
                stdout(&output)
            );
            assert!(!log.exists(), "{socket} spawned socat");
        }
    }
}
