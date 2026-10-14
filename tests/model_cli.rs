#![cfg(unix)]
use std::{fs, process::Command};

#[test]
fn model_subcommand_exposes_the_bundled_applet_without_credentials() {
    let result = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["model", "--help"])
        .env_remove("OPENAI_API_KEY")
        .env_remove("JEV_API_KEY")
        .env_remove("TYPESAFE_API_KEY")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    let help = String::from_utf8(result.stdout).unwrap();
    assert!(help.contains("execute-model"));
    assert!(help.contains("--fixture"));
    assert!(help.contains("--env-file"));
}

#[test]
fn missing_bundled_applet_does_not_fall_back_to_a_path_version() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("execute");
    fs::copy(env!("CARGO_BIN_EXE_execute"), &executable).unwrap();
    let helper_directory = std::path::Path::new(env!("CARGO_BIN_EXE_execute-model"))
        .parent()
        .unwrap();
    let result = Command::new(executable)
        .args(["model", "--help"])
        .env("PATH", helper_directory)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("bundled execute-model helper missing")
    );
}

#[test]
fn model_manifest_contracts_reject_known_invalid_inputs_before_earlier_effects() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("invalid.program");
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for invalid in [
        "then jev.classify with question \"Pick one\" and labels [\"only\"]",
        "then jev.score with question \"Rate this\" and levels [\"only\"]",
        "then jev.judge with question \"Is it valid?\" and cache",
        "then llm.ask with instruction \"Draft a result\"",
    ] {
        fs::write(
            &file,
            format!("run \"printf\" with arguments [\"forbidden\"]\nthen save to \"forbidden\"\n{invalid}\n"),
        )
        .unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg(&file)
            .arg("--manifest-dir")
            .arg(project.join("manifests"))
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(!result.status.success(), "{invalid}");
        assert!(!directory.path().join("forbidden").exists(), "{invalid}");
        assert!(result.stdout.is_empty());
    }
}
