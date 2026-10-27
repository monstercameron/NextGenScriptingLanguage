//! CI check for the flagship demo: `examples/demo/walkthrough.sh` walks write, validate,
//! what-if, confirm, a failing run, repair and resume, with the fake compositor. Unix
//! only, because the compositor is a shell script that the runtime launches directly.
#![cfg(all(unix, feature = "agent-advisory", feature = "records"))]

use std::path::Path;
use std::process::Command;

#[test]
fn the_flagship_walkthrough_runs_end_to_end_without_network_or_models() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo/walkthrough.sh");
    let output = Command::new("sh")
        .arg(&script)
        .env("EXECUTE", env!("CARGO_BIN_EXE_execute"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "walkthrough failed\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("walkthrough complete"), "{stdout}");
    for step in [
        "== 1. write",
        "== 2. validate",
        "== 3. what-if",
        "== 4. confirm",
        "== 5. repair",
        "== 6. resume",
        "== 7. verify",
    ] {
        assert!(stdout.contains(step), "missing {step}\n{stdout}");
    }
}
