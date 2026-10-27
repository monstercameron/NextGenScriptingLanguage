use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const EXACT_MODE_MESSAGE: &str =
    "mode must be an integer from 0 through 4095 when permissions_policy is exact";

fn execute(
    root: &Path,
    command: &str,
    source: &str,
    inputs: &[&str],
    disk_manifests: bool,
) -> Output {
    let program = root.join("conditional.program");
    fs::write(&program, source).unwrap();
    let mut invocation = Command::new(env!("CARGO_BIN_EXE_execute"));
    invocation.arg(command).arg(&program).current_dir(root);
    for input in inputs {
        invocation.args(["--input", *input]);
    }
    if disk_manifests {
        invocation
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"));
    }
    invocation.output().unwrap()
}

#[test]
fn exact_permissions_policy_requires_a_bounded_mode() {
    let root = tempfile::tempdir().unwrap();
    for disk_manifests in [false, true] {
        for (suffix, expected_status) in
            [("", false), (" and mode 0", true), (" and mode 4095", true)]
        {
            let source = format!(
                "run fs.write_atomic with path \"candidate\" and expected_content_hash \"missing\" and symlink_policy \"preserve\" and permissions_policy \"exact\" and content \"x\"{suffix}\n"
            );
            let output = execute(root.path(), "check", &source, &[], disk_manifests);
            assert_eq!(
                output.status.success(),
                expected_status,
                "unexpected static result for {source:?}; disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if !expected_status {
                assert!(String::from_utf8_lossy(&output.stderr).contains(EXACT_MODE_MESSAGE));
            }
        }

        for invalid_mode in [-1, 4096] {
            let source = format!(
                "run fs.write_atomic with path \"must-not-exist\" and expected_content_hash \"missing\" and symlink_policy \"preserve\" and permissions_policy \"exact\" and content \"x\" and mode {invalid_mode}\n"
            );
            let output = execute(root.path(), "check", &source, &[], disk_manifests);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains(EXACT_MODE_MESSAGE),
                "check accepted exact mode {invalid_mode}; disk_manifests={disk_manifests}: {stderr}"
            );
        }

        // The conditional constraint must not reject an otherwise ignored mode
        // when the selected policy is preserve.
        let preserve = "run fs.write_atomic with path \"preserved-policy\" and expected_content_hash \"missing\" and symlink_policy \"preserve\" and permissions_policy \"preserve\" and content \"x\" and mode 4096\n";
        let output = execute(root.path(), "run", preserve, &[], disk_manifests);
        assert!(
            output.status.success(),
            "mode constraint leaked into preserve policy; disk_manifests={disk_manifests}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_file(root.path().join("preserved-policy")).unwrap();

        let dynamic_policy = "accept policy as text\nrun fs.write_atomic with path \"dynamic-policy\" and expected_content_hash \"missing\" and symlink_policy \"preserve\" and permissions_policy policy and content \"x\"\n";
        let checked = execute(root.path(), "check", dynamic_policy, &[], disk_manifests);
        assert!(
            checked.status.success(),
            "check rejected unresolved policy: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
        let invalid = execute(
            root.path(),
            "run",
            dynamic_policy,
            &["policy=exact"],
            disk_manifests,
        );
        let stderr = String::from_utf8_lossy(&invalid.stderr);
        assert!(
            !invalid.status.success() && stderr.contains(EXACT_MODE_MESSAGE),
            "runtime did not validate dynamically selected exact policy: {stderr}"
        );
        assert!(!root.path().join("dynamic-policy").exists());
        let valid = execute(
            root.path(),
            "run",
            dynamic_policy,
            &["policy=preserve"],
            disk_manifests,
        );
        assert!(
            valid.status.success(),
            "runtime rejected dynamic preserve policy: {}",
            String::from_utf8_lossy(&valid.stderr)
        );
        fs::remove_file(root.path().join("dynamic-policy")).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn resolved_exact_mode_values_are_checked_again_at_runtime() {
    let root = tempfile::tempdir().unwrap();
    let source = "accept policy as text\naccept mode as integer\nrun fs.write_atomic with path \"dynamic-mode\" and expected_content_hash \"missing\" and symlink_policy \"preserve\" and permissions_policy policy and content \"x\" and mode mode\n";

    for disk_manifests in [false, true] {
        assert!(execute(root.path(), "check", source, &[], disk_manifests)
            .status
            .success());
        for mode in ["384", "4095"] {
            let mode_input = format!("mode={mode}");
            let output = execute(
                root.path(),
                "run",
                source,
                &["policy=exact", mode_input.as_str()],
                disk_manifests,
            );
            assert!(
                output.status.success(),
                "runtime rejected exact mode {mode}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let _ = fs::remove_file(root.path().join("dynamic-mode"));
        }
        for mode in ["-1", "4096"] {
            let mode_input = format!("mode={mode}");
            let output = execute(
                root.path(),
                "run",
                source,
                &["policy=exact", mode_input.as_str()],
                disk_manifests,
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains(EXACT_MODE_MESSAGE),
                "runtime accepted exact mode {mode}: {stderr}"
            );
            assert!(!root.path().join("dynamic-mode").exists());
        }
    }
}
