//! Decode and form encoders require inputs that their applet implementations read.
use std::{fs, path::Path, process::Command};

fn invoke(root: &Path, source: &str, command: &str, disk_manifests: bool) -> std::process::Output {
    let program = root.join("encoding.program");
    fs::write(&program, source).unwrap();
    let mut invocation = Command::new(env!("CARGO_BIN_EXE_execute"));
    invocation.arg(command).arg(&program).current_dir(root);
    if disk_manifests {
        invocation
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"));
    }
    invocation.output().unwrap()
}

#[test]
fn missing_required_encoding_inputs_fail_check_and_run_for_disk_and_bundled_catalogs() {
    let cases = [
        ("run decode.hex\n", "text"),
        ("run decode.base64\n", "text"),
        ("run decode.url\n", "text"),
        ("run encode.form\n", "value"),
    ];
    let root = tempfile::tempdir().unwrap();
    for disk_manifests in [false, true] {
        for (source, missing) in cases {
            for command in ["check", "run"] {
                let output = invoke(root.path(), source, command, disk_manifests);
                let diagnostic = String::from_utf8_lossy(&output.stderr);
                assert!(
                    !output.status.success(),
                    "{command} accepted {source:?}; disk_manifests={disk_manifests}"
                );
                assert!(
                    diagnostic.contains(&format!("requires parameter \"{missing}\"")),
                    "{command} {source:?}; disk_manifests={disk_manifests}: {diagnostic}"
                );
            }
        }
    }
}

#[test]
fn valid_decode_and_form_inputs_check_and_run_for_disk_and_bundled_catalogs() {
    let cases = [
        ("run decode.hex with text \"666f6f\"\n", "foo"),
        ("run decode.base64 with text \"Zm9v\"\n", "foo"),
        (
            "run decode.url with text \"hello%20world\"\n",
            "hello world",
        ),
        (
            "run encode.form with value {\"name\": \"a b\"}\n",
            "name=a+b",
        ),
    ];
    let root = tempfile::tempdir().unwrap();
    for disk_manifests in [false, true] {
        for (source, expected) in cases {
            for command in ["check", "run"] {
                let output = invoke(root.path(), source, command, disk_manifests);
                assert!(
                    output.status.success(),
                    "{command} {source:?}; disk_manifests={disk_manifests}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if command == "run" {
                    assert_eq!(
                        String::from_utf8_lossy(&output.stdout).trim_end_matches('\n'),
                        expected,
                        "{source:?}; disk_manifests={disk_manifests}"
                    );
                }
            }
        }
    }
}
