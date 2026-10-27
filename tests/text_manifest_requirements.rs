//! Text-shaping contracts reject missing inputs while retaining stdin support.
use std::{fs, path::Path, process::Command};

fn execute(root: &Path, command: &str, source: &str, disk: bool) -> std::process::Output {
    let program = root.join("text.program");
    fs::write(&program, source).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_execute"));
    cmd.arg(command).arg(&program).current_dir(root);
    if disk {
        cmd.arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"));
    }
    cmd.output().unwrap()
}

#[test]
fn check_rejects_each_missing_shaping_parameter() {
    let cases = [
        ("run text.contains_any with text \"hello\"\n", "needles"),
        ("run text.ends_with with text \"hello\"\n", "suffix"),
        ("run text.starts_with with text \"hello\"\n", "prefix"),
        ("run text.pad with text \"hello\"\n", "width"),
        ("run text.truncate with text \"hello\"\n", "width"),
        ("run text.repeat with text \"hello\"\n", "count"),
        (
            "run text.substring with text \"hello\" and length 2\n",
            "start",
        ),
        (
            "run text.substring with text \"hello\" and start 1\n",
            "length",
        ),
    ];
    let root = tempfile::tempdir().unwrap();
    for disk in [false, true] {
        for (source, missing) in cases {
            let result = execute(root.path(), "check", source, disk);
            assert!(!result.status.success(), "accepted {source:?}, disk={disk}");
            assert!(
                String::from_utf8_lossy(&result.stderr).contains(missing),
                "{source:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}

#[test]
fn complete_calls_work_with_explicit_text_and_pipeline_input() {
    let cases = [
        ("text.contains_any", "needles [\"ell\", \"xyz\"]", "true"),
        ("text.ends_with", "suffix \"lo\"", "true"),
        ("text.starts_with", "prefix \"he\"", "true"),
        ("text.pad", "width 7 and fill \"_\"", "hello__"),
        ("text.truncate", "width 3", "he…"),
        ("text.repeat", "count 2", "hellohello"),
        ("text.substring", "start 1 and length 3", "ell"),
    ];
    let root = tempfile::tempdir().unwrap();
    for disk in [false, true] {
        for (operation, args, expected) in cases {
            let scripts = [
                format!("run {operation} with text \"hello\" and {args}\n"),
                format!("use value \"hello\"\nthen {operation} with {args}\n"),
            ];
            for source in scripts {
                for command in ["check", "run"] {
                    let result = execute(root.path(), command, &source, disk);
                    assert!(
                        result.status.success(),
                        "{command} {source:?}, disk={disk}: {}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                    if command == "run" {
                        assert_eq!(
                            String::from_utf8_lossy(&result.stdout).trim_end_matches('\n'),
                            expected,
                            "{source:?}, disk={disk}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn display_width_padding_runs_through_the_manifest_contract() {
    let root = tempfile::tempdir().unwrap();
    let source = "run text.pad with text \"界\" and width 2 and display_width true\n";
    for command in ["check", "run"] {
        let result = execute(root.path(), command, source, true);
        assert!(
            result.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        if command == "run" {
            assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "界");
        }
    }
}

#[test]
fn required_parameters_can_be_present_with_empty_or_zero_values() {
    let cases = [
        (
            "run text.contains_any with text \"hello\" and needles []\n",
            "false",
        ),
        ("run text.repeat with text \"hello\" and count 0\n", ""),
        (
            "run text.substring with text \"hello\" and start 0 and length 0\n",
            "",
        ),
    ];
    let root = tempfile::tempdir().unwrap();
    for (source, expected) in cases {
        for command in ["check", "run"] {
            let result = execute(root.path(), command, source, false);
            assert!(
                result.status.success(),
                "{command} {source:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            if command == "run" {
                assert_eq!(
                    String::from_utf8_lossy(&result.stdout).trim_end_matches('\n'),
                    expected
                );
            }
        }
    }
}
