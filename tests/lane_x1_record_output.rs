#![cfg(feature = "records")]
use std::process::Command;

#[test]
fn show_run_escapes_untrusted_record_text() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("failure.program");
    let record = root.path().join("record");
    std::fs::write(&program, "accept msg as text\nfail with message msg\n").unwrap();
    let value = "prefix\u{001b}[2J\u{202e}";
    let _ = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            program.to_str().unwrap(),
            "--record",
            record.to_str().unwrap(),
            "--input",
            &format!("msg={value}"),
        ])
        .output()
        .unwrap();

    let shown = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["show-run", record.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let stdout = String::from_utf8_lossy(&shown.stdout);
    assert!(!stdout.contains('\u{001b}'));
    assert!(!stdout.contains('\u{202e}'));
    assert!(stdout.contains(r"\u{1b}[2J"));
    assert!(stdout.contains(r"\u{202e}"));
}
