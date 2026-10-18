#![cfg(feature = "model")]
//! Everyday model examples must check and run using authored offline fixtures.
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn bounded_output(command: &mut Command) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let stdout = directory.path().join("stdout");
    let stderr = directory.path().join("stderr");
    let mut child = command
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("gauntlet subprocess exceeded 15 seconds: {command:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: std::fs::read(stdout).unwrap(),
        stderr: std::fs::read(stderr).unwrap(),
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.file_type().unwrap().is_symlink());
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn command(workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(workspace)
        .env("EXECUTE_MODELS_OFFLINE", "1")
        .env_remove("OPENAI_API_KEY")
        .env_remove("openai")
        .env_remove("JEV_API_KEY")
        .env_remove("TYPESAFE_API_KEY")
        .env_remove("jev");
    command
}

#[test]
fn all_twenty_four_gauntlet_programs_check_and_match_offline_oracles() {
    let project = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = project.join("examples/ai-gauntlet");
    let catalog: Value =
        serde_json::from_slice(&std::fs::read(root.join("catalog.json")).unwrap()).unwrap();
    let workflows = catalog["workflows"].as_array().unwrap();
    assert_eq!(workflows.len(), 24);
    assert_eq!(catalog["version"], 1);
    let sources: std::collections::BTreeSet<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "program")
        })
        .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    let listed: std::collections::BTreeSet<_> = workflows
        .iter()
        .map(|workflow| workflow["file"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(listed.len(), 24, "catalog must not repeat a program");
    assert_eq!(sources, listed, "catalog must cover every program");
    let mut cases_run = 0;
    for workflow in workflows {
        let source = root.join(workflow["file"].as_str().unwrap());
        let output = bounded_output(
            command(&root)
                .arg("check")
                .arg(&source)
                .arg("--manifest-dir")
                .arg(project.join("manifests")),
        );
        assert!(
            output.status.success(),
            "{} did not check: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        // Only 012 runs a Unix command (the literal `pwd`); all sources check everywhere.
        #[cfg(not(unix))]
        if workflow["unix_only"] == true {
            eprintln!("skipping Unix-only offline runs: {}", source.display());
            continue;
        }
        let cases = workflow["cases"].as_array().unwrap();
        assert!(cases.len() >= 4);
        for case in cases {
            let directory = tempfile::tempdir().unwrap();
            let workspace = directory.path();
            let ordinal = format!("{:03}", workflow["id"].as_u64().unwrap());
            copy_tree(
                &root.join("fixtures").join(&ordinal),
                &workspace.join("fixtures").join(&ordinal),
            );
            std::fs::create_dir(workspace.join("out")).unwrap();
            let mut run = command(workspace);
            run.arg(&source)
                .arg("--manifest-dir")
                .arg(project.join("manifests"));
            for (name, value) in case["inputs"].as_object().unwrap() {
                assert_ne!(name, "live", "the test must use the offline default");
                run.arg("--input")
                    .arg(format!("{name}={}", value.as_str().unwrap()));
            }
            let output = bounded_output(&mut run);
            assert!(
                output.status.success(),
                "{} / {} failed: {}",
                source.display(),
                case["name"],
                String::from_utf8_lossy(&output.stderr)
            );
            for (path, expected) in case["outputs"].as_object().unwrap() {
                let actual = std::fs::read(workspace.join(path)).unwrap();
                assert_eq!(
                    actual,
                    expected.as_str().unwrap().as_bytes(),
                    "{} / {} / {path}",
                    source.display(),
                    case["name"]
                );
            }
            assert_eq!(
                output.stdout,
                case["expected_stdout"].as_str().unwrap().as_bytes(),
                "{} / {} stdout",
                source.display(),
                case["name"]
            );
            assert!(!workspace.join("MODEL_TEXT_EXECUTED").exists());
            cases_run += 1;
        }
    }
    assert!(cases_run >= 92, "expected all platform-supported cases");
}
