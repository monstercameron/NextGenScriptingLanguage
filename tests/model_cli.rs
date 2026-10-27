#![cfg(feature = "model")]
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

#[test]
fn jev_classify_requires_exactly_one_label_source_in_check_and_offline_run() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("classify.program");
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let invalid = [
        "run jev.classify with question \"Pick one\" as raw\n",
        "run jev.classify with question \"Pick one\" and labels [\"a\", \"b\"] and criteria {\"a\": \"first\", \"b\": \"second\"} as raw\n",
    ];

    for source in invalid {
        fs::write(&file, source).unwrap();
        for mode in ["check", "run"] {
            let result = Command::new(env!("CARGO_BIN_EXE_execute"))
                .arg(mode)
                .arg(&file)
                .arg("--manifest-dir")
                .arg(project.join("manifests"))
                .env("EXECUTE_MODELS_OFFLINE", "1")
                .env_remove("JEV_API_KEY")
                .env_remove("jev")
                .current_dir(directory.path())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&result.stderr);
            assert!(
                !result.status.success(),
                "{mode} accepted an invalid label source: {source}"
            );
            assert!(
                stderr.contains("jev.classify requires exactly one of labels or criteria"),
                "{mode} reported an unexpected error for {source:?}: {stderr}"
            );
            assert!(
                result.stdout.is_empty(),
                "{mode} emitted output before rejection"
            );
        }
    }

    for source in [
        "run jev.classify with question \"Pick one\" and labels [\"a\", \"b\"] as raw\n",
        "run jev.classify with question \"Pick one\" and criteria {\"a\": \"first\", \"b\": \"second\"} as raw\n",
    ] {
        fs::write(&file, source).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("check")
            .arg(&file)
            .arg("--manifest-dir")
            .arg(project.join("manifests"))
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "check rejected valid label source {source:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn jev_classify_alternatives_accept_dynamic_label_or_criteria_references() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("dynamic-classify.program");
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let cases = [
        (
            "accept labels as json\nrun jev.classify with question \"Pick one\" and labels labels as raw\n",
            r#"labels=["a","b"]"#,
        ),
        (
            "accept criteria as json\nrun jev.classify with question \"Pick one\" and criteria criteria as raw\n",
            r#"criteria={"a":"first","b":"second"}"#,
        ),
    ];

    for (source, input) in cases {
        fs::write(&file, source).unwrap();
        let checked = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("check")
            .arg(&file)
            .arg("--manifest-dir")
            .arg(project.join("manifests"))
            .arg("--input")
            .arg(input)
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(
            checked.status.success(),
            "check rejected dynamic alternative input {input}: {}",
            String::from_utf8_lossy(&checked.stderr)
        );

        let ran = Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("run")
            .arg(&file)
            .arg("--manifest-dir")
            .arg(project.join("manifests"))
            .arg("--input")
            .arg(input)
            .env("EXECUTE_MODELS_OFFLINE", "1")
            .env_remove("JEV_API_KEY")
            .env_remove("jev")
            .current_dir(directory.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&ran.stderr);
        assert!(!ran.status.success(), "offline run unexpectedly succeeded");
        assert!(
            stderr.contains("offline") && !stderr.contains("jev.classify requires exactly one"),
            "dynamic source was rejected as a missing/conflicting alternative: {stderr}"
        );
    }
}
