//! The manifest can reject substring arguments that exceed every possible text
//! range while leaving input-dependent validation to the runtime.
use std::{fs, path::Path, process::Command};

fn execute(root: &Path, command: &str, source: &str, disk_manifests: bool) -> std::process::Output {
    execute_with_input(root, command, source, disk_manifests, None)
}

fn execute_with_input(
    root: &Path,
    command: &str,
    source: &str,
    disk_manifests: bool,
    input: Option<&str>,
) -> std::process::Output {
    let program = root.join("substring-bounds.program");
    fs::write(&program, source).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_execute"));
    cmd.arg(command).arg(&program).current_dir(root);
    if let Some(input) = input {
        cmd.arg("--input").arg(format!("dynamic={input}"));
    }
    if disk_manifests {
        cmd.arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"));
    }
    cmd.output().unwrap()
}

#[test]
fn check_rejects_universally_invalid_substring_bounds() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        (
            "run text.substring with text \"abc\" and start 0 and length -1\n",
            "length must be an integer from 0 through 16777216",
        ),
        (
            "run text.substring with text \"abc\" and start -16777217 and length 0\n",
            "start must be an integer from -16777216 through 16777216",
        ),
        (
            "run text.substring with text \"abc\" and start 16777217 and length 0\n",
            "start must be an integer from -16777216 through 16777216",
        ),
        (
            "run text.substring with text \"abc\" and start 0 and length 16777217\n",
            "length must be an integer from 0 through 16777216",
        ),
    ];
    for disk_manifests in [false, true] {
        for (source, diagnostic) in cases {
            let output = execute(root.path(), "check", source, disk_manifests);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains(diagnostic),
                "accepted invalid bounds with disk_manifests={disk_manifests}: {stderr}"
            );
        }
    }
}

#[test]
fn check_rejects_universally_invalid_list_slice_bounds() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        (
            "run list.slice with items [1] and offset 100001 and length 0\n",
            "offset must be an integer from -100000 through 100000",
        ),
        (
            "run list.slice with items [1] and offset 0 and length -1\n",
            "length must be an integer from 0 through 100000",
        ),
        (
            "run list.slice with items [1] and offset 0 and length 100001\n",
            "length must be an integer from 0 through 100000",
        ),
        (
            "run list.slice with items [1] and offset -100001 and length 0\n",
            "offset must be an integer from -100000 through 100000",
        ),
    ];
    for disk_manifests in [false, true] {
        for (source, diagnostic) in cases {
            let output = execute(root.path(), "check", source, disk_manifests);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains(diagnostic),
                "accepted invalid list.slice bounds with disk_manifests={disk_manifests}: {stderr}"
            );
        }
    }
}

#[test]
fn list_slice_accepts_safe_boundaries_and_the_full_supported_range() {
    let root = tempfile::tempdir().unwrap();
    for source in [
        "run list.slice with items [1] and offset -100000 and length 0\n",
        "run list.slice with items [1] and offset 100000 and length 0\n",
        "run list.slice with items [1] and offset 0 and length 100000\n",
        "run list.slice with items [1] and offset -1 and length 1\n",
    ] {
        let output = execute(root.path(), "check", source, true);
        assert!(
            output.status.success(),
            "rejected safe list.slice bounds: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let items = vec!["0"; 100_000].join(",");
    let source =
        format!("run list.slice with items [{items}] and offset -100000 and length 100000\n");
    let output = execute(root.path(), "run", &source, true);
    assert!(
        output.status.success(),
        "full list.slice bounds failed at the supported item cap: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        format!("[{items}]")
    );
}

#[test]
fn check_accepts_safe_boundaries_and_valid_negative_indexes() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        "run text.substring with text \"abc\" and start -16777216 and length 0\n",
        "run text.substring with text \"abc\" and start 16777216 and length 0\n",
        "run text.substring with text \"abc\" and start 0 and length 16777216\n",
        "run text.substring with text \"abc\" and start -3 and length 3\n",
    ];
    for disk_manifests in [false, true] {
        for source in cases {
            let output = execute(root.path(), "check", source, disk_manifests);
            assert!(
                output.status.success(),
                "rejected safe bounds with disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let output = execute(
        root.path(),
        "run",
        "run text.substring with text \"abc\" and start -3 and length 3\n",
        true,
    );
    assert!(
        output.status.success(),
        "valid negative index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim_end(), "abc");
}

#[test]
fn dynamic_arguments_reach_runtime_range_validation() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        (
            "start",
            -16777217,
            "start must be an integer from -16777216 through 16777216",
        ),
        (
            "start",
            16777217,
            "start must be an integer from -16777216 through 16777216",
        ),
        (
            "length",
            16777217,
            "length must be an integer from 0 through 16777216",
        ),
    ];
    for disk_manifests in [false, true] {
        for (parameter, value, diagnostic) in cases {
            let source = format!(
                "accept dynamic as integer\nrun text.substring with text \"abc\" and start {} and length {}\n",
                if parameter == "start" { "dynamic" } else { "0" },
                if parameter == "length" { "dynamic" } else { "0" },
            );
            let checked = execute(root.path(), "check", &source, disk_manifests);
            assert!(
                checked.status.success(),
                "static check rejected dynamic {parameter} with disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&checked.stderr)
            );

            let run = execute_with_input(
                root.path(),
                "run",
                &source,
                disk_manifests,
                Some(&value.to_string()),
            );
            let stderr = String::from_utf8_lossy(&run.stderr);
            assert!(
                !run.status.success() && stderr.contains(diagnostic),
                "runtime accepted invalid dynamic {parameter} with disk_manifests={disk_manifests}: {stderr}"
            );
        }
    }
}

#[test]
fn dynamic_list_slice_arguments_reach_runtime_range_validation() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        (
            "offset",
            -100001,
            "offset must be an integer from -100000 through 100000",
        ),
        (
            "offset",
            100001,
            "offset must be an integer from -100000 through 100000",
        ),
        (
            "length",
            -1,
            "length must be an integer from 0 through 100000",
        ),
        (
            "length",
            100001,
            "length must be an integer from 0 through 100000",
        ),
    ];
    for disk_manifests in [false, true] {
        for (parameter, value, diagnostic) in cases {
            let source = format!(
                "accept dynamic as integer\nrun list.slice with items [1] and offset {} and length {}\n",
                if parameter == "offset" { "dynamic" } else { "0" },
                if parameter == "length" { "dynamic" } else { "0" },
            );
            let checked = execute(root.path(), "check", &source, disk_manifests);
            assert!(
                checked.status.success(),
                "static check rejected dynamic list.slice {parameter} with disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&checked.stderr)
            );

            let run = execute_with_input(
                root.path(),
                "run",
                &source,
                disk_manifests,
                Some(&value.to_string()),
            );
            let stderr = String::from_utf8_lossy(&run.stderr);
            assert!(
                !run.status.success() && stderr.contains(diagnostic),
                "runtime accepted invalid dynamic list.slice {parameter} with disk_manifests={disk_manifests}: {stderr}"
            );
        }
    }
}
