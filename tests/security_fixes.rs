//! Regression tests for the security fixes of the audit (#87). Each test names the
//! behaviour it guards, so a change that reopens the hole fails here first.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

/// Run `source` as a program with the repository manifests and no capability policy.
fn run_source(source: &str) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workflow.program");
    fs::write(&path, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .current_dir(directory.path())
        .output()
        .unwrap()
}

#[test]
fn loader_variables_cannot_be_set_from_a_program_without_a_policy() {
    // The dynamic loader reads these before the program starts, so a binding that could
    // set them would run attacker-chosen code in every applet.
    for name in ["LD_PRELOAD", "LD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES"] {
        let source =
            format!("run printf with message \"x\" and environment {{\"{name}\": \"/tmp/x\"}}\n");
        let output = run_source(&source);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{name} was accepted: {output:?}");
        assert!(stderr.contains("dynamic loader"), "{name}: {stderr}");
        assert!(output.stdout.is_empty(), "{name} ran before it was refused");
    }
}

#[test]
fn ordinary_environment_names_still_pass_without_a_policy() {
    let output =
        run_source("run printf with message \"x\" and environment {\"SAFE_NAME\": \"1\"}\n");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "x\n");
}

#[test]
fn a_rejected_sensitive_input_value_is_not_echoed() {
    // An environment input is sensitive: a wrong value must not appear in the error.
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("job.program"),
        "accept mode as text one of [\"light\", \"dark\"] from environment \"EXECUTE_SECURITY_CHOICE\"\nrun printf with message \"x\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "job.program", "--manifest-dir"])
        .arg(manifests())
        .env("EXECUTE_SECURITY_CHOICE", "hunter2-guess")
        .current_dir(directory.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("must be one of"), "{stderr}");
    assert!(
        !stderr.contains("hunter2-guess"),
        "the rejected value was echoed: {stderr}"
    );
}

#[test]
fn a_rejected_plain_input_value_is_still_shown() {
    // A supplied value that is not sensitive keeps the message that names it.
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("job.program"),
        "accept mode as text one of [\"light\", \"dark\"]\nrun printf with message \"x\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            "job.program",
            "--input",
            "mode=blue",
            "--manifest-dir",
        ])
        .arg(manifests())
        .current_dir(directory.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("got \"blue\""), "{stderr}");
}

#[cfg(unix)]
mod links {
    //! A symbolic link inside a granted directory must not reach outside it. The policy
    //! grants only `inside/`; the secret lives in `outside/`.
    use super::*;
    use std::os::unix::fs::symlink;

    const SECRET: &str = "SECRET-TOKEN-123";

    /// The layout: a granted `inside/`, a secret in `outside/`, and links into both.
    fn layout(root: &Path) {
        fs::create_dir_all(root.join("inside")).unwrap();
        fs::create_dir_all(root.join("outside")).unwrap();
        fs::write(root.join("outside/secret.txt"), format!("{SECRET}\n")).unwrap();
        fs::write(root.join("inside/own.txt"), "own\n").unwrap();
        symlink("../outside/secret.txt", root.join("inside/file-link")).unwrap();
        symlink("../outside", root.join("inside/dir-link")).unwrap();
        symlink("own.txt", root.join("inside/own-link")).unwrap();
    }

    /// A policy that grants reads and writes on `inside/` only.
    fn policy(root: &Path) {
        let granted = root.join("inside").display().to_string();
        let body = format!(
            r#"{{"version":1,"applets":{{"text.concat":{{"capabilities":["filesystem.read","filesystem.write"],"filesystem":{{"read":["{granted}"],"write":["{granted}"]}}}}}}}}"#
        );
        fs::write(root.join("policy.json"), body).unwrap();
    }

    fn run(root: &Path, source: &str) -> Output {
        fs::write(root.join("main.program"), source).unwrap();
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["run", "main.program", "--capability-policy", "policy.json"])
            .arg("--manifest-dir")
            .arg(manifests())
            .current_dir(root)
            .output()
            .unwrap()
    }

    fn shown(output: &Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    #[test]
    fn read_through_a_link_that_leaves_the_granted_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        layout(root.path());
        policy(root.path());
        let output = run(root.path(), "read \"inside/file-link\"\n");
        let text = shown(&output);
        assert!(!output.status.success(), "{text}");
        assert!(
            text.contains("not allowed by the capability policy"),
            "{text}"
        );
        assert!(!text.contains(SECRET), "the secret was read: {text}");
    }

    #[test]
    fn a_read_through_a_link_inside_the_grant_still_works() {
        let root = tempfile::tempdir().unwrap();
        layout(root.path());
        policy(root.path());
        let output = run(root.path(), "read \"inside/own-link\"\n");
        assert!(output.status.success(), "{}", shown(&output));
    }

    #[test]
    fn use_folder_through_a_link_that_leaves_the_granted_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        layout(root.path());
        policy(root.path());
        let output = run(
            root.path(),
            "use folder \"inside/dir-link\"\nread \"secret.txt\"\n",
        );
        let text = shown(&output);
        assert!(!output.status.success(), "{text}");
        assert!(
            text.contains("not allowed by the capability policy"),
            "{text}"
        );
        assert!(!text.contains(SECRET), "the folder's file was read: {text}");
    }
}

#[cfg(unix)]
mod use_folder_handle {
    //! A child launched after `use folder` enters the directory the handle names. The
    //! child's working directory is then the opened directory, not a path looked up again.
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn a_child_after_use_folder_runs_in_the_opened_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("work/sub")).unwrap();
        symlink("work/sub", root.path().join("link")).unwrap();
        for folder in ["work/sub", "link"] {
            fs::write(
                root.path().join("main.program"),
                format!("use folder \"{folder}\"\nrun pwd\n"),
            )
            .unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_execute"))
                .args(["run", "main.program", "--manifest-dir"])
                .arg(manifests())
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{folder}: {output:?}");
            let shown = String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned();
            let expected = fs::canonicalize(root.path().join("work/sub")).unwrap();
            assert_eq!(shown, expected.display().to_string(), "{folder}");
        }
    }
}
