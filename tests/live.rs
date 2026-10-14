//! Real CLI/socket/native-process regressions for opt-in live execution.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn live_control_end_to_end() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live_control.py");
    let mut child = Command::new("python3")
        .arg(script)
        .arg(env!("CARGO_BIN_EXE_execute"))
        .stdin(Stdio::null())
        .spawn()
        .expect("Python 3 is required for live CLI integration tests");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "live CLI integration failed");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("live CLI tests exceeded their outer deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
