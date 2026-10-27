//! `accept NAME as TYPE from prompt "QUESTION"` and `from secret prompt`: a prompt is a
//! declared input. It is pre-answered by `--input` or `--answers`, asked on a terminal
//! when one exists, and refused before any effect when neither gives an answer. A
//! secret answer never reaches inspect, explain, traces, records, or checkpoints.
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SECRET: &str = "hunter2-correct-horse";

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

/// Run the binary with no terminal prompts, a private key location, and the given args.
fn execute(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .arg("--manifest-dir")
        .arg(manifests())
        .env("EXECUTE_NO_PROMPT", "1")
        .env("XDG_STATE_HOME", root.join("state"))
        .env("LOCALAPPDATA", root.join("state"))
        .current_dir(root)
        .output()
        .unwrap()
}

#[path = "support/private_answers.rs"]
mod private_answers;

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const NAME_PROGRAM: &str =
    "accept name as text from prompt \"Your name?\"\nuse value name\nthen save to \"out.txt\"\n";
// Feed the secret through a non-persistent applet so trace/record handling can prove it
// is withheld without asking the workflow to save plaintext secret data.
const SECRET_PROGRAM: &str = "accept token as text from secret prompt \"Token?\"\nuse value token\nthen text.trim\nuse value \"done\"\n";
const SECRET_SAVE_PROGRAM: &str = "accept token as text from secret prompt \"Token?\"\nuse value token\nthen save to \"out.txt\"\n";

#[test]
fn a_missing_answer_is_refused_before_any_effect_and_the_prompt_is_named() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("job.program"),
        "accept name as text from prompt \"Your name?\"\nuse value \"started\"\nthen save to \"before.txt\"\n",
    )
    .unwrap();
    let output = execute(root.path(), &["run", "job.program"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("Your name?"), "{message}");
    assert!(message.contains("--input name=VALUE"), "{message}");
    assert!(
        !root.path().join("before.txt").exists(),
        "no effect may run first"
    );
}

#[test]
fn a_prompt_is_pre_answered_with_input_or_an_answers_file() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("job.program"), NAME_PROGRAM).unwrap();
    let output = execute(root.path(), &["run", "job.program", "--input", "name=ada"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(root.path().join("out.txt")).unwrap(),
        "ada"
    );

    fs::write(root.path().join("answers.json"), r#"{"name":"grace"}"#).unwrap();
    private_answers::make_private(&root.path().join("answers.json"));
    fs::remove_file(root.path().join("out.txt")).unwrap();
    let output = execute(
        root.path(),
        &["run", "job.program", "--answers", "answers.json"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(root.path().join("out.txt")).unwrap(),
        "grace"
    );
}

#[test]
fn an_input_value_wins_over_the_answers_file() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("job.program"), NAME_PROGRAM).unwrap();
    fs::write(root.path().join("answers.json"), r#"{"name":"from-file"}"#).unwrap();
    private_answers::make_private(&root.path().join("answers.json"));
    let output = execute(
        root.path(),
        &[
            "run",
            "job.program",
            "--answers",
            "answers.json",
            "--input",
            "name=from-flag",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(root.path().join("out.txt")).unwrap(),
        "from-flag"
    );
}

#[test]
fn explain_lists_each_prompt_with_its_question_and_secret_flag() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("job.program"), SECRET_PROGRAM).unwrap();
    let output = execute(root.path(), &["explain", "job.program", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let input = &plan["inputs"][0];
    assert_eq!(input["prompt"]["question"], "Token?");
    assert_eq!(input["prompt"]["secret"], true);
}

#[test]
fn a_secret_answer_never_reaches_inspect_explain_trace_or_record() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("job.program"), SECRET_PROGRAM).unwrap();

    let inspected = execute(
        root.path(),
        &[
            "inspect",
            "job.program",
            "--input",
            &format!("token={SECRET}"),
        ],
    );
    assert!(inspected.status.success(), "{}", stderr(&inspected));
    let report: Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(report["inputs"][0]["value_withheld"], true);
    assert!(!String::from_utf8_lossy(&inspected.stdout).contains(SECRET));

    let explained = execute(
        root.path(),
        &[
            "explain",
            "job.program",
            "--input",
            &format!("token={SECRET}"),
        ],
    );
    assert!(!String::from_utf8_lossy(&explained.stdout).contains(SECRET));
    assert!(!stderr(&explained).contains(SECRET));

    #[cfg(feature = "records")]
    {
        // Records and checkpoints cannot be combined, so each gets its own run.
        let record = root.path().join("record");
        let recorded = execute(
            root.path(),
            &[
                "run",
                "job.program",
                "--input",
                &format!("token={SECRET}"),
                "--trace",
                "--record",
                record.to_str().unwrap(),
            ],
        );
        assert!(recorded.status.success(), "{}", stderr(&recorded));
        assert!(!root.path().join("out.txt").exists());
        assert!(
            !stderr(&recorded).contains(SECRET),
            "trace leaked the secret"
        );
        assert!(!String::from_utf8_lossy(&recorded.stdout).contains(SECRET));
        assert_no_file_contains(&record, SECRET);

        let trace = fs::read_to_string(record.join("trace.jsonl")).unwrap();
        let secret_step: Value = trace
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .find(|step: &Value| step["applets"][0] == "text.trim")
            .expect("the secret-bearing text.trim step");
        assert!(secret_step["stdout"]["blob"].is_null(), "{secret_step}");
        assert_eq!(secret_step["stdout"]["withheld"], "policy");
    }
}

#[test]
fn a_secret_answer_cannot_be_saved_as_plaintext() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("job.program"), SECRET_SAVE_PROGRAM).unwrap();

    let output = execute(
        root.path(),
        &["run", "job.program", "--input", &format!("token={SECRET}")],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("secret data cannot be persisted without encryption"),
        "{}",
        stderr(&output)
    );
    assert!(!root.path().join("out.txt").exists());
}

#[test]
fn a_checkpoint_is_refused_for_a_run_with_a_secret_answer() {
    // A journal would keep the answer, or data computed from it, on disk. The run is
    // refused before any effect, and no checkpoint directory is created.
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("job.program"), SECRET_PROGRAM).unwrap();
    let checkpoint = root.path().join("checkpoint");
    let output = execute(
        root.path(),
        &[
            "run",
            "job.program",
            "--input",
            &format!("token={SECRET}"),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("secret prompt input"),
        "{}",
        stderr(&output)
    );
    assert!(!checkpoint.exists());
    assert!(
        !root.path().join("out.txt").exists(),
        "no effect may run first"
    );
}

#[test]
fn a_secret_prompt_cannot_be_a_choice_input() {
    let error = flow::parser::parse(
        "accept token as text one of [\"a\", \"b\"] from secret prompt \"Token?\"\n",
    )
    .expect_err("a secret choice would quote the answer in an error");
    assert!(format!("{error:#}").contains("secret prompt"), "{error:#}");
}

/// Assert that no file under `directory`, in any nested folder, holds `needle`.
#[cfg(feature = "records")]
fn assert_no_file_contains(directory: &Path, needle: &str) {
    if !directory.exists() {
        return;
    }
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_no_file_contains(&path, needle);
        } else if let Ok(bytes) = fs::read(&path) {
            assert!(
                !bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "{} contains the secret",
                path.display()
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn answers_must_be_private_regular_files_and_never_follow_symlinks() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("job.program"), NAME_PROGRAM).unwrap();
    let answers = root.path().join("answers.json");
    fs::write(&answers, r#"{"name":"private-value"}"#).unwrap();
    for mode in [0o644, 0o640, 0o604] {
        fs::set_permissions(&answers, fs::Permissions::from_mode(mode)).unwrap();
        let output = execute(
            root.path(),
            &["run", "job.program", "--answers", "answers.json"],
        );
        assert!(!output.status.success(), "mode {mode:o}");
        assert!(stderr(&output).contains("answers file"));
        assert!(!root.path().join("out.txt").exists());
    }
    fs::set_permissions(&answers, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&answers, root.path().join("link.json")).unwrap();
    let output = execute(
        root.path(),
        &["run", "job.program", "--answers", "link.json"],
    );
    assert!(!output.status.success());
    assert!(!root.path().join("out.txt").exists());
    let output = execute(
        root.path(),
        &["run", "job.program", "--answers", "answers.json"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(root.path().join("out.txt")).unwrap(),
        "private-value"
    );
}

#[cfg(unix)]
#[test]
fn answers_file_size_is_bounded_and_directories_are_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("answers.json");
    fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        format!("{:#}", flow::prompt::answers_from_file(&path).unwrap_err()).contains("64 KiB")
    );
    assert!(flow::prompt::answers_from_file(root.path()).is_err());
    let fifo = root.path().join("answers.fifo");
    assert!(Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    assert!(flow::prompt::answers_from_file(&fifo).is_err());
}

#[cfg(windows)]
#[test]
fn answers_reject_inherited_acls_and_reparse_points() {
    let root = tempfile::tempdir().unwrap();
    let answers = root.path().join("answers.json");
    fs::write(&answers, r#"{"name":"private-value"}"#).unwrap();
    private_answers::make_private(&answers);
    assert!(flow::prompt::answers_from_file(&answers).is_ok());
    let link = root.path().join("link.json");
    match std::os::windows::fs::symlink_file(&answers, &link) {
        Ok(()) => assert!(flow::prompt::answers_from_file(&link).is_err()),
        Err(error) => assert!(
            std::env::var_os("CI").is_none(),
            "CI must create the reparse-point fixture to verify answers-file rejection: {error}"
        ),
    }
    let status = Command::new("icacls")
        .arg(&answers)
        .arg("/inheritance:e")
        .status()
        .unwrap();
    assert!(status.success());
    assert!(flow::prompt::answers_from_file(&answers).is_err());
}
