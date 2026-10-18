//! `use value` inside a service body and across a checkpointed run and resume.
use flow::{manifest::Registry, parser, runtime};
use std::{fs, path::Path, path::PathBuf, process::Command, time::Duration};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

#[test]
fn service_body_feeds_a_value_to_the_next_command() {
    let directory = tempfile::tempdir().unwrap();
    let wc = fs::read_to_string(manifests().join("wc.json")).unwrap();
    let registry = Registry::from_sources(&[wc]).unwrap();
    let workflow =
        parser::parse("every 10 milliseconds\n  use value \"hi\"\n  then wc with flags [\"-c\"]\n")
            .unwrap();
    let options = runtime::RunOptions {
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: Some(Duration::from_secs(5)),
        ..runtime::RunOptions::new(directory.path().to_owned())
    };
    let mut output = Vec::new();
    let result = runtime::run_service(
        &workflow,
        &registry,
        &options,
        &runtime::ServiceLimits {
            max_runs: Some(1),
            max_record_bytes: 1024,
        },
        |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(result.runs, 1);
    assert_eq!(String::from_utf8_lossy(&output).trim(), "2");
}

#[test]
fn checkpointed_use_value_survives_resume() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("ckpt");
    let path = directory.path().join("workflow.program");
    fs::write(
        &path,
        "accept message as text with default \"hello\"\nuse value message\npipe to wc with flags [\"-c\"]\n",
    )
    .unwrap();

    let first = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint)
        .output()
        .unwrap();
    assert_eq!(
        first.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&first.stdout).trim(), "5");

    let resumed = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("resume")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint)
        .output()
        .unwrap();
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    // The final output streamed during the first run, so a completed resume does not
    // replay it (#366).
    assert!(resumed.stdout.is_empty(), "{:?}", resumed.stdout);
}
