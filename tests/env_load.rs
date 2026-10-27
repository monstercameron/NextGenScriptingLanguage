//! `env.load` through a real program: a host read feeds the in-process parser, and
//! nothing from the file is ever run.
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn execute(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .arg("--manifest-dir")
        .arg(manifests())
        .current_dir(root)
        .output()
        .unwrap()
}

const PROGRAM: &str = "read \"os-release.env\"\nthen env.load\nthen save to \"out.json\"\n";

#[test]
fn an_environment_file_loads_into_a_json_object_without_being_run() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("os-release.env"),
        "# release\nNAME=\"Arch Linux\"\nID=arch\nexport PRETTY='Arch $HOME literal'\n",
    )
    .unwrap();
    fs::write(root.path().join("job.program"), PROGRAM).unwrap();

    let output = execute(root.path(), &["run", "job.program"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let loaded: Value =
        serde_json::from_str(&fs::read_to_string(root.path().join("out.json")).unwrap()).unwrap();
    assert_eq!(loaded["NAME"], "Arch Linux");
    assert_eq!(loaded["ID"], "arch");
    assert_eq!(loaded["PRETTY"], "Arch $HOME literal");
}

#[test]
fn a_file_with_command_substitution_is_refused_before_anything_is_written() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("os-release.env"),
        "ID=arch\nVERSION=$(touch pwned)\n",
    )
    .unwrap();
    fs::write(root.path().join("job.program"), PROGRAM).unwrap();

    let output = execute(root.path(), &["run", "job.program"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(!root.path().join("out.json").exists());
    assert!(
        !root.path().join("pwned").exists(),
        "the file must never run"
    );
}

#[test]
fn explain_shows_the_read_that_feeds_the_load() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("os-release.env"), "ID=arch\n").unwrap();
    fs::write(root.path().join("job.program"), PROGRAM).unwrap();

    let output = execute(root.path(), &["explain", "job.program", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(steps[0]["kind"], "read");
    assert_eq!(steps[0]["reads"], true);
    assert_eq!(steps[1]["applet"], "env.load");
}

#[test]
#[cfg(feature = "records")]
fn selected_env_load_values_are_redacted_from_the_entire_run_record() {
    let root = tempfile::tempdir().unwrap();
    let canary = "ENV-LOAD-CANARY-6f24";
    fs::write(
        root.path().join("credentials.env"),
        format!("TOKEN={canary}\nREGION=test-region\n"),
    )
    .unwrap();
    fs::write(
        root.path().join("job.program"),
        "read \"credentials.env\"\nthen env.load with secret [\"TOKEN\"] as loaded\n",
    )
    .unwrap();

    let output = execute(root.path(), &["run", "job.program", "--record", "record"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (path, bytes) in files_under(&root.path().join("record")) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(canary),
            "secret appeared in {}",
            path.display()
        );
    }
    let trace = fs::read_to_string(root.path().join("record/trace.jsonl")).unwrap();
    assert!(trace.contains("\"withheld\":\"policy\""), "{trace}");
}

#[test]
#[cfg(feature = "records")]
fn runtime_selected_env_load_values_are_redacted_from_the_entire_run_record() {
    let root = tempfile::tempdir().unwrap();
    let canary = "ENV-LOAD-REFERENCE-CANARY-2d80";
    fs::write(
        root.path().join("credentials.env"),
        format!("TOKEN={canary}\nREGION=test-region\n"),
    )
    .unwrap();
    fs::write(
        root.path().join("job.program"),
        "use value [\"TOKEN\"] as selected\nread \"credentials.env\"\nthen env.load with secret selected as loaded\n",
    )
    .unwrap();
    let output = execute(root.path(), &["run", "job.program", "--record", "record"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (path, bytes) in files_under(&root.path().join("record")) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(canary),
            "secret appeared in {}",
            path.display()
        );
    }
}

#[test]
fn checkpointed_env_load_with_secret_values_is_refused_before_writing_state() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("credentials.env"),
        "TOKEN=checkpoint-canary\n",
    )
    .unwrap();
    fs::write(
        root.path().join("job.program"),
        "read \"credentials.env\"\nthen env.load with secret true as loaded\n",
    )
    .unwrap();

    let output = execute(
        root.path(),
        &["run", "job.program", "--checkpoint", "checkpoint"],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("secret"), "{error}");
    assert!(!error.contains("checkpoint-canary"));
    assert!(!root.path().join("checkpoint").exists());
}

#[test]
#[cfg(unix)]
fn env_load_secret_redacts_a_failing_stdin_consumers_stderr_without_records() {
    let root = tempfile::tempdir().unwrap();
    let canary = "ENV-LOAD-STDERR-CANARY-91c4";
    fs::write(
        root.path().join("credentials.env"),
        format!("TOKEN={canary}\n"),
    )
    .unwrap();
    fs::write(
        root.path().join("job.program"),
        "read \"credentials.env\"\nthen env.load with secret [\"TOKEN\"] as loaded\ntake text at \"/TOKEN\" from loaded as token\nuse value token\npipe to \"sh\" with arguments [\"-c\", \"cat >&2; exit 7\"]\n",
    )
    .unwrap();

    let output = execute(root.path(), &["run", "job.program"]);
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("[REDACTED]"), "{diagnostic}");
    assert!(!diagnostic.contains(canary));
}

#[cfg(feature = "records")]
fn files_under(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }

    let mut files = Vec::new();
    visit(root, root, &mut files);
    files
}
