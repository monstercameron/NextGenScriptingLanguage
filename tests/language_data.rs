//! Data-flow boundary tests that need process stdin, working directories, or records.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_script(directory: &Path, script: &str, args: &[&str]) -> std::process::Output {
    let program = directory.join("workflow.program");
    fs::write(&program, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn read_from_stdin_binds_text_as_pipeline_input() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("stdin.program");
    fs::write(
        &program,
        "read from stdin as incoming\nuse value incoming\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(manifests())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"stdin payload")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"stdin payload");
}

#[test]
fn use_folder_resolves_following_relative_reads() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("work")).unwrap();
    fs::write(directory.path().join("work/input.txt"), "inside folder").unwrap();
    let output = run_script(
        directory.path(),
        "use folder \"work\"\nread \"input.txt\" as contents\nuse value contents\n",
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"inside folder");
}

#[test]
fn declared_deterministic_applet_cache_is_hit_on_second_run() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("cached.program");
    fs::write(&program, "run printf with message \"abc\"\nthen tr with from \"a\" and to \"b\" as changed and cache\n").unwrap();
    let cache = directory.path().join("cache");
    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(directory.path())
            .arg("run")
            .arg(&program)
            .arg("--manifest-dir")
            .arg(manifests())
            .arg("--cache-dir")
            .arg(&cache)
            .arg("--trace")
            .output()
            .unwrap()
    };
    let first = invoke();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&first.stderr).contains("\"event\":\"cache_hit\""),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = invoke();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("\"event\":\"cache_hit\""),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
}

#[test]
fn runtime_secret_binding_is_absent_from_stdout_and_record() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "DATA-FLOW-SECRET-CANARY";
    let program = directory.path().join("secret.program");
    fs::write(&program, format!("run printf with message \"{canary}\" as token and keep secret\nrun printf with message \"safe\"\n")).unwrap();
    let record = directory.path().join("record");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--record")
        .arg(&record)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"safe\n");
    fn check_files(path: &Path, canary: &str) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                check_files(&entry.path(), canary);
            } else {
                let bytes = fs::read(entry.path()).unwrap();
                assert!(
                    !bytes
                        .windows(canary.len())
                        .any(|part| part == canary.as_bytes()),
                    "secret leaked into {}",
                    entry.path().display()
                );
            }
        }
    }
    check_files(&record, canary);
}

#[test]
fn file_call_resolves_from_the_calling_program_directory() {
    let directory = tempfile::tempdir().unwrap();
    let caller_dir = directory.path().join("nested");
    fs::create_dir(&caller_dir).unwrap();
    fs::write(
        caller_dir.join("callee.program"),
        "accept word as text\nuse value word\n",
    )
    .unwrap();
    fs::write(caller_dir.join("caller.program"), "run program file \"callee.program\" with word \"resolved\" as answer\nrun printf with message answer\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(caller_dir.join("caller.program"))
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"resolved\n");
}
