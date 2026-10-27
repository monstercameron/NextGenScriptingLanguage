use std::process::Command;

#[test]
fn check_explain_prints_the_registered_diagnostic_entry() {
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "--explain", "check.range"])
        .output()
        .expect("execute check --explain");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    for expected in [
        "Diagnostic code: check.range",
        "Family: validate",
        "Meaning:",
        "Example:",
        "Typical fix:",
        "exit with status 300",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}: {stdout}");
    }
}

#[test]
fn check_explain_rejects_unregistered_codes() {
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "--explain", "check.not_a_code"])
        .output()
        .expect("execute check --explain");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown diagnostic code"));
}
