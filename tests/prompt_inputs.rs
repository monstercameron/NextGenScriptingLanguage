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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const NAME_PROGRAM: &str =
    "accept name as text from prompt \"Your name?\"\nuse value name\nthen save to \"out.txt\"\n";
// The program saves the secret to a file, then ends on a value of its own, so the
// run's final output is not the secret: only the file holds it, as the program chose.
const SECRET_PROGRAM: &str = "accept token as text from secret prompt \"Token?\"\nuse value token\nthen save to \"out.txt\"\nuse value \"done\"\n";

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
        assert_eq!(
            fs::read_to_string(root.path().join("out.txt")).unwrap(),
            SECRET
        );
        assert!(
            !stderr(&recorded).contains(SECRET),
            "trace leaked the secret"
        );
        assert!(!String::from_utf8_lossy(&recorded.stdout).contains(SECRET));
        assert_no_file_contains(&record, SECRET);
    }
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
