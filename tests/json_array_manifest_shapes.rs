//! JSON array applet manifests reject known non-array roots without making
//! dynamically supplied JSON shapes a static error.
use flow::applets;
use serde_json::json;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn execute(root: &Path, source: &str, action: &str) -> Output {
    fs::write(root.join("case.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(root)
        .args([action, "case.program", "--manifest-dir"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .output()
        .unwrap()
}

#[test]
fn statically_known_non_array_documents_fail_check_and_valid_arrays_run() {
    let root = tempfile::tempdir().unwrap();
    let cases = [
        (
            "json.group_by",
            "run json.group_by with document {}\n",
            "run json.group_by with document [1,2]\n",
        ),
        (
            "json.unique_by",
            "run json.unique_by with document {}\n",
            "run json.unique_by with document [1,2]\n",
        ),
        (
            "json.sum",
            "run json.sum with document {}\n",
            "run json.sum with document [1,2]\n",
        ),
        (
            "json.sort_by",
            "run json.sort_by with document {}\n",
            "run json.sort_by with document [\"b\",\"a\"]\n",
        ),
    ];

    for (name, invalid, valid) in cases {
        let check = execute(root.path(), invalid, "check");
        assert!(
            !check.status.success(),
            "check accepted a non-array root for {name}"
        );
        assert!(
            String::from_utf8_lossy(&check.stderr).contains("document"),
            "{name}: {}",
            String::from_utf8_lossy(&check.stderr)
        );

        for action in ["check", "run"] {
            let output = execute(root.path(), valid, action);
            assert!(
                output.status.success(),
                "{action} rejected a valid array for {name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn unresolved_json_shape_passes_check_and_runtime_still_rejects_non_array_values() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "json.group_by",
        "json.unique_by",
        "json.sum",
        "json.sort_by",
    ] {
        let source = format!(
            "accept source as text\nrun json.parse with text source as document\nrun {name} with document document\n"
        );
        let check = execute(root.path(), &source, "check");
        assert!(
            check.status.success(),
            "static checking must defer unknown JSON shape for {name}: {}",
            String::from_utf8_lossy(&check.stderr)
        );

        fs::write(root.path().join("case.program"), &source).unwrap();
        let run = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(root.path())
            .args(["run", "case.program", "--manifest-dir"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
            .args(["--input", "source={}"])
            .output()
            .unwrap();
        assert!(
            !run.status.success(),
            "runtime accepted a non-array JSON value for {name}"
        );
        assert!(
            String::from_utf8_lossy(&run.stderr).contains("parameter \"document\" expects list"),
            "{name}: {}",
            String::from_utf8_lossy(&run.stderr)
        );

        let arguments = json!({"document":{}});
        let runtime_error =
            applets::execute(name, arguments.as_object().unwrap(), b"").unwrap_err();
        assert!(
            runtime_error.message.contains("document must be an array"),
            "{name}: {}",
            runtime_error.message
        );
    }
}
