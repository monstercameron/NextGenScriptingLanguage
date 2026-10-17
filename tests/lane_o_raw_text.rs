//! In-process text and path results cross runtime boundaries as raw UTF-8.
//! Records and lists keep their JSON encoding.
use std::fs;
use std::process::Command;
use tempfile::tempdir;

fn run_program(program_text: &str) -> std::process::Output {
    let directory = tempdir().unwrap();
    let program = directory.path().join("text.program");
    fs::write(&program, program_text).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&program)
        .current_dir(directory.path())
        .output()
        .unwrap()
}

#[test]
fn text_result_is_written_as_raw_utf8_at_the_terminal() {
    let output = run_program("run text.trim_final_lf with text \"a\"\n");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"a");
}

#[test]
fn text_result_keeps_multibyte_utf8_exact() {
    let output = run_program("run text.trim_final_lf with text \"caf\u{e9}\"\n");
    assert!(output.status.success());
    assert_eq!(output.stdout, "caf\u{e9}".as_bytes());
}

#[test]
fn text_result_counts_as_raw_bytes_in_a_native_pipeline() {
    // The applet's text is three bytes; `wc -c` must see those bytes, not a JSON encoding.
    let output = run_program(
        "run text.trim_final_lf with text \"abc\"\nthen \"wc\" with arguments [\"-c\"]\n",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "3");
}
