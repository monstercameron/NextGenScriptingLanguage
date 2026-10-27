//! Hash and encoding applets preserve pipeline bytes when explicit text is omitted.
use flow::applets;
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};

fn invoke(root: &Path, source: &str, command: &str, disk_manifests: bool) -> std::process::Output {
    let program = root.join("hash-encoding.program");
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

fn inspect(root: &Path, operation: &str, disk_manifests: bool) -> std::process::Output {
    let mut invocation = Command::new(env!("CARGO_BIN_EXE_execute"));
    invocation
        .args(["applets", "inspect", operation])
        .current_dir(root);
    if disk_manifests {
        invocation
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"));
    }
    invocation.output().unwrap()
}

fn operations() -> [(&'static str, &'static str); 5] {
    [
        (
            "hash.sha256",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            "hash.sha512",
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        ),
        ("encode.hex", "616263"),
        ("encode.base64", "YWJj"),
        ("encode.url", "abc"),
    ]
}

#[test]
fn applets_receive_nonempty_pipeline_input_from_previous_step() {
    let root = tempfile::tempdir().unwrap();
    for disk_manifests in [false, true] {
        for (operation, expected) in operations() {
            let source = format!("run text.lower with text \"ABC\"\nthen {operation}\n");
            for command in ["check", "run"] {
                let output = invoke(root.path(), &source, command, disk_manifests);
                assert!(
                    output.status.success(),
                    "{command} {operation}, disk_manifests={disk_manifests}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if command == "run" {
                    assert_eq!(
                        String::from_utf8_lossy(&output.stdout).trim_end_matches('\n'),
                        expected,
                        "{operation}, disk_manifests={disk_manifests}"
                    );
                }
            }
        }
    }
}

#[test]
fn explicit_text_overrides_pipeline_input_for_every_operation() {
    let root = tempfile::tempdir().unwrap();
    for disk_manifests in [false, true] {
        for (operation, expected) in operations() {
            let source = format!(
                "run text.lower with text \"not the input\"\nthen {operation} with text \"abc\"\n"
            );
            for command in ["check", "run"] {
                let output = invoke(root.path(), &source, command, disk_manifests);
                assert!(
                    output.status.success(),
                    "{command} {operation}, disk_manifests={disk_manifests}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if command == "run" {
                    assert_eq!(
                        String::from_utf8_lossy(&output.stdout).trim_end_matches('\n'),
                        expected,
                        "{operation}, disk_manifests={disk_manifests}"
                    );
                }
            }
        }
    }
}

#[test]
fn hash_and_encoding_applets_keep_binary_pipeline_bytes() {
    let args = json!({});
    let args = args.as_object().unwrap();
    for (operation, expected) in [
        (
            "hash.sha256",
            "a8100ae6aa1940d0b663bb31cd466142ebbdbd5187131b92d93818987832eb89",
        ),
        (
            "hash.sha512",
            "6700df6600b118ab0432715a7e8a68b0bf37cdf4adaf0fb9e2b3ebe04ad19c7032cbad55e932792af360bafaa09962e2e690652bc075b2dad0c30688ba2f31a3",
        ),
        ("encode.hex", "ff"),
        ("encode.base64", "/w=="),
        ("encode.url", "%FF"),
    ] {
        assert_eq!(
            applets::execute(operation, args, &[0xff]).unwrap(),
            Value::String(expected.into()),
            "{operation}"
        );
    }
}

#[cfg(unix)]
#[test]
fn native_non_utf8_bytes_reach_each_hash_and_encoding_applet() {
    let root = tempfile::tempdir().unwrap();
    for disk_manifests in [false, true] {
        for (operation, expected) in [
            (
                "hash.sha256",
                "a8100ae6aa1940d0b663bb31cd466142ebbdbd5187131b92d93818987832eb89",
            ),
            (
                "hash.sha512",
                "6700df6600b118ab0432715a7e8a68b0bf37cdf4adaf0fb9e2b3ebe04ad19c7032cbad55e932792af360bafaa09962e2e690652bc075b2dad0c30688ba2f31a3",
            ),
            ("encode.hex", "ff"),
            ("encode.base64", "/w=="),
            ("encode.url", "%FF"),
        ] {
            let source = format!(
                "run \"printf\" with arguments [\"%b\", \"\\\\377\"]\nthen {operation}\n"
            );
            for command in ["check", "run"] {
                let output = invoke(root.path(), &source, command, disk_manifests);
                assert!(
                    output.status.success(),
                    "{command} {operation}, disk_manifests={disk_manifests}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if command == "run" {
                    assert_eq!(
                        String::from_utf8_lossy(&output.stdout).trim_end_matches('\n'),
                        expected,
                        "{operation}, disk_manifests={disk_manifests}"
                    );
                }
            }
        }
    }
}

#[test]
fn every_hash_and_encoding_applet_exposes_raw_stdin_in_its_contract() {
    let root = tempfile::tempdir().unwrap();
    for disk_manifests in [false, true] {
        for (operation, _) in operations() {
            let output = inspect(root.path(), operation, disk_manifests);
            assert!(
                output.status.success(),
                "inspect {operation}, disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["applet"]["io"]["stdin"], "bytes", "{operation}");
            assert_eq!(
                report["applet"]["ports"]["inputs"]["stdin"]["schema"]["type"], "text",
                "{operation}"
            );
            assert_eq!(
                report["applet"]["ports"]["inputs"]["stdin"]["encoding"], "raw",
                "{operation}"
            );
        }
    }
}
