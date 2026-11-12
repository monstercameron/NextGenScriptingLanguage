#![cfg(feature = "model")]
//! Issue 377: `run --model-budget calls=N` refuses the call after the budget is spent
//! with the typed `model_budget_exhausted` failure, and `try` takes the fallback branch.
//! Every model call replays a fixture from a temporary directory; no network is contacted.

use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const QUESTION: &str = "Is this line an error?";

fn describe(output: &Output) -> String {
    format!(
        "status {:?} stdout {:?} stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The fixture file name is the SHA-256 of the canonical (sorted-key) JSON request,
/// as `request_hash` computes it inside the library.
fn write_classify_fixture(directory: &Path) {
    let request = json!({
        "provider": "jev", "kind": "classify", "model": "jev-latest",
        "state": "", "question": QUESTION,
        "labels": ["yes", "no"], "minimum": 80
    });
    let bytes = serde_json::to_vec(&request).unwrap();
    let name: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let response = json!({
        "expected_request": request,
        "response": {
            "model": "jev-latest",
            "answers": {"decision": {
                "type": "choice", "choice": "yes", "confidence": 0.9,
                "probabilities": {"yes": 0.9, "no": 0.1}
            }},
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }
    });
    let fixtures = directory.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    fs::write(
        fixtures.join(format!("{name}.json")),
        serde_json::to_vec(&response).unwrap(),
    )
    .unwrap();
}

/// Fifty classify calls. Each success prints one `ok` line and each caught failure prints
/// one `refused` line. A value produced with `then` inside the loop reaches stdout only
/// once, as the final value of the program, so the catch prints directly.
fn loop_program(directory: &Path) {
    let source = concat!(
        "repeat 50 times\n",
        "  try\n",
        "    run jev.classify with question \"Is this line an error?\" and labels [\"yes\", \"no\"] and minimum 80 as classification\n",
        "    run \"printf\" with arguments [\"ok\\n\"]\n",
        "  catch failure\n",
        "    run \"printf\" with arguments [\"refused\\n\"]\n",
    );
    fs::write(directory.join("loop.program"), source).unwrap();
}

fn run(directory: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory)
        .env_remove("EXECUTE_MODEL_BUDGET")
        .env_remove("EXECUTE_MODEL_FIXTURES")
        .args(["run", "loop.program", "--model-fixtures", "fixtures"])
        .args(extra);
    command.output().unwrap()
}

#[test]
fn the_call_after_the_budget_is_refused_and_the_fallback_runs() {
    let directory = tempfile::tempdir().unwrap();
    write_classify_fixture(directory.path());
    loop_program(directory.path());
    let output = run(directory.path(), &["--model-budget", "calls=10"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let successes = stdout.lines().filter(|line| *line == "ok").count();
    let refusals = stdout.lines().filter(|line| *line == "refused").count();
    assert_eq!(successes, 10, "{}", describe(&output));
    assert_eq!(refusals, 40, "{}", describe(&output));
}

#[test]
fn without_a_budget_all_fifty_calls_run() {
    let directory = tempfile::tempdir().unwrap();
    write_classify_fixture(directory.path());
    loop_program(directory.path());
    let output = run(directory.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let successes = stdout.lines().filter(|line| *line == "ok").count();
    assert_eq!(successes, 50, "{}", describe(&output));
    assert!(
        !stdout.contains("model_budget_exhausted"),
        "{}",
        describe(&output)
    );
}
