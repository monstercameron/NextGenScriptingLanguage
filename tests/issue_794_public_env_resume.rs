//! #794: a checkpointed graph with a public environment input must resume with the
//! same environment without repeating the input, refuse a changed environment value,
//! and accept an explicit `--input` override that matches the run's value.

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const PROGRAM: &str =
    "accept greeting as text from public environment \"EXECUTE_ISSUE_794_GREETING\"\n\
graph g\n\
\x20 run text.concat with parts [greeting, \"x\"] and separator \" \" as joined\n\
end graph\n";

fn execute(directory: &Path, environment_value: &str, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .env("EXECUTE_ISSUE_794_GREETING", environment_value)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .expect("start execute")
}

#[test]
fn public_environment_input_resumes_without_an_explicit_override() {
    let work = tempfile::tempdir().unwrap();
    let program = work.path().join("greeting.program");
    let checkpoint = work.path().join("checkpoint");
    fs::write(&program, PROGRAM).unwrap();
    let program_text = program.to_string_lossy().into_owned();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();

    let run = execute(
        work.path(),
        "hello-794",
        &["run", &program_text, "--checkpoint", &checkpoint_text],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // Same environment, no `--input`: the resume resolves the input like the run did.
    let resumed = execute(
        work.path(),
        "hello-794",
        &["resume", &program_text, "--checkpoint", &checkpoint_text],
    );
    assert!(
        resumed.status.success(),
        "same-value resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );

    // A changed environment value is a different input, so the journal refuses it.
    let changed = execute(
        work.path(),
        "changed-794",
        &["resume", &program_text, "--checkpoint", &checkpoint_text],
    );
    assert!(
        !changed.status.success(),
        "a changed environment value must be refused"
    );
    assert!(!String::from_utf8_lossy(&changed.stderr).contains("changed-794"));

    // An explicit override with the run's value still resumes, even though the
    // environment now holds a different value.
    let overridden = execute(
        work.path(),
        "changed-794",
        &[
            "resume",
            &program_text,
            "--checkpoint",
            &checkpoint_text,
            "--input",
            "greeting=hello-794",
        ],
    );
    assert!(
        overridden.status.success(),
        "explicit override resume failed: {}",
        String::from_utf8_lossy(&overridden.stderr)
    );
}
