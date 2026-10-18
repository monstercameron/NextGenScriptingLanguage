//! Keep the port output and source-validation CI regressions covered.
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn port(name: &str) -> PathBuf {
    root().join("examples/omarchy-ports").join(name)
}

#[test]
fn invalid_animation_mode_does_not_print_its_internal_value() {
    let directory = tempfile::tempdir().unwrap();
    // Preflight resolves commands in later branches even though invalid mode exits
    // before invoking them. Give it real executables without needing Omarchy.
    for name in [
        "omarchy-hyprland-toggle",
        "omarchy-hyprland-toggle-enabled",
        "omarchy-i18n",
        "omarchy-notification-send",
    ] {
        std::fs::copy(
            env!("CARGO_BIN_EXE_execute"),
            directory
                .path()
                .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .env("PATH", directory.path())
        .arg("run")
        .arg(port("omarchy-toggle-animations").join("port.program"))
        .arg("--manifest-dir")
        .arg(root().join("manifests"))
        .args(["--", "bogus"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Usage: omarchy-toggle-animations [toggle|on|off]\n"
    );
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}

#[test]
fn bom_removal_example_passes_static_checks() {
    let path = root().join("examples/ladder/116-remove-utf8-bom-from-export.program");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("check")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(root().join("manifests"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let source = std::fs::read_to_string(path).unwrap();
    let program = flow::parser::parse(&source).unwrap();
    let flow::ast::StepKind::Invoke { call, .. } = &program.steps[1].kind else {
        panic!("BOM removal must invoke sed");
    };
    assert_eq!(call.operation, "sed");
    assert_eq!(
        call.parameters["program"],
        flow::ast::Value::String("s/^\u{feff}//".into())
    );
}

#[cfg(unix)]
#[test]
fn omarchy_ports_match_bash_for_every_declared_case() {
    for name in [
        "omarchy-hyprland-toggle-enabled",
        "omarchy-hyprland-toggle-disabled",
        "omarchy-toggle-enabled",
        "omarchy-toggle-animations",
    ] {
        let output = Command::new("python3")
            .arg(root().join("tools/port_compare.py"))
            .args(["--binary", env!("CARGO_BIN_EXE_execute"), "--only", name])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{name}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
