//! Keep all practical utilities and the retained recovery regressions executable.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn check_collection(script: &str, expected: usize, timeout: Duration) {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("results.json");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(script);
    let mut child = Command::new("python3")
        .arg(script)
        .args(["--binary", env!("CARGO_BIN_EXE_execute"), "--report"])
        .arg(&report)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("utility collection needs Python 3 and standard Unix binaries");
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_secs(2));
            if child.try_wait().unwrap().is_none() {
                child.kill().unwrap();
            }
            child.wait().unwrap();
            panic!("utility collection exceeded its outer deadline");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let evidence: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert!(status.success(), "{evidence:#}");
    assert_eq!(evidence["cases_run"], expected);
    assert_eq!(evidence["passed"], expected);
}

#[test]
fn one_hundred_fifty_useful_programs_pass_in_isolated_fixtures() {
    check_collection("examples/ladder/run.py", 150, Duration::from_secs(240));
}

#[test]
fn retained_negative_and_recovery_regressions_pass() {
    check_collection(
        "tests/fixtures/legacy-ladder/run.py",
        9,
        Duration::from_secs(45),
    );
}

#[test]
fn utility_runner_contracts_pass() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/utility_catalog.py");
    assert!(Command::new("python3")
        .arg(script)
        .status()
        .unwrap()
        .success());
}
