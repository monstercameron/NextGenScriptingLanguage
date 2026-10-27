//! `run --once` runs one calendar activation. It must honour the program's
//! exit contract like a finite run. Linux CI also validates generated
//! `OnCalendar=` values with systemd-analyze when it is available.
use std::fs;
use std::process::Command;
use tempfile::tempdir;

fn run_once(program_text: &str) -> std::process::Output {
    let directory = tempdir().unwrap();
    let program = directory.path().join("nightly.program");
    fs::write(&program, program_text).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "--once"])
        .arg(&program)
        .current_dir(directory.path())
        .output()
        .unwrap()
}

#[test]
fn once_reports_fail_message_and_status_like_a_finite_run() {
    let output = run_once(
        "on schedule \"30 9 * * 1-5\"\n  fail with message \"nightly failed\" and status 4\n",
    );
    assert_eq!(output.status.code(), Some(4));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("nightly failed"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn once_flushes_output_before_exiting_with_status() {
    let output = run_once(
        "on schedule \"30 9 * * 1-5\"\n  run text.trim_final_lf with text \"done\"\n  exit with status 3\n",
    );
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "done");
}

#[cfg(all(target_os = "linux", feature = "tooling"))]
#[test]
fn generated_oncalendar_values_are_accepted_by_systemd_analyze() {
    // Skip where systemd-analyze is not installed, so the test stays a CI check.
    let probe = Command::new("systemd-analyze").arg("--version").output();
    if !matches!(probe, Ok(ref output) if output.status.success()) {
        eprintln!("skipping: systemd-analyze is not available");
        return;
    }
    let directory = tempdir().unwrap();
    let program = directory.path().join("nightly.program");
    fs::write(
        &program,
        "on schedule \"0,30 9-17/2 1-31/2 1,3,12 0,7\"\n  run text.trim_final_lf with text \"x\"\n",
    )
    .unwrap();
    let generated = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["service", "generate", "systemd-timer"])
        .arg(&program)
        .args(["--name", "com.example.nightly"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let text = String::from_utf8(generated.stdout).unwrap();
    let values = text
        .lines()
        .filter_map(|line| line.strip_prefix("OnCalendar="))
        .collect::<Vec<_>>();
    assert!(!values.is_empty(), "no OnCalendar values in:\n{text}");
    for value in values {
        let analysed = Command::new("systemd-analyze")
            .args(["calendar", value])
            .output()
            .unwrap();
        assert!(
            analysed.status.success(),
            "systemd-analyze rejected {value:?}: {}",
            String::from_utf8_lossy(&analysed.stderr)
        );
    }
}
