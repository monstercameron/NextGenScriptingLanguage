#![cfg(all(feature = "records", feature = "model"))]
//! Issue 373: a recorded run writes one `model_decision` event per jev classify call,
//! and `show-run` lists it. The call replays an offline fixture; no network is contacted.

use serde_json::{json, Value};
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

/// Write the fixture for the classify request, named by the SHA-256 of its JSON.
/// The fixture answers `yes` at confidence 0.9, so with a minimum of 80 it is accepted.
fn write_classify_fixture(directory: &Path, confidence: f64) {
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
                "type": "choice", "choice": "yes", "confidence": confidence,
                "probabilities": {"yes": confidence, "no": 1.0 - confidence}
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

fn write_program(directory: &Path) {
    let source = concat!(
        "run \"printf\" with arguments [\"ok\\n\"]\n",
        "run jev.classify with question \"Is this line an error?\" and labels [\"yes\", \"no\"] and minimum 80 as classification\n",
    );
    fs::write(directory.join("classify.program"), source).unwrap();
}

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .env_remove("EXECUTE_MODEL_BUDGET")
        .env_remove("EXECUTE_MODEL_FIXTURES")
        .args(arguments)
        .output()
        .unwrap()
}

fn trace_events(record: &Path) -> Vec<Value> {
    fs::read_to_string(record.join("trace.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn a_recorded_classify_call_writes_one_model_decision_matching_its_result() {
    let work = tempfile::tempdir().unwrap();
    write_classify_fixture(work.path(), 0.9);
    write_program(work.path());
    let record = work.path().join("record");

    let run = execute(
        work.path(),
        &[
            "run",
            "classify.program",
            "--record",
            record.to_str().unwrap(),
            "--model-fixtures",
            "fixtures",
        ],
    );
    assert!(run.status.success(), "{}", describe(&run));

    let events = trace_events(&record);
    let steps: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == "step")
        .collect();
    assert_eq!(
        steps.len(),
        2,
        "the printf and the classify step are recorded"
    );

    let decisions: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == "model_decision")
        .collect();
    assert_eq!(decisions.len(), 1, "exactly one decision per classify call");
    let decision = decisions[0];
    assert_eq!(decision["kind"], "classify");
    assert_eq!(decision["label"], "yes");
    assert_eq!(decision["label_count"], 2);
    assert_eq!(decision["confidence_percent"], 90);
    assert_eq!(decision["minimum"], 80);
    assert_eq!(decision["accepted"], true);
    assert_eq!(decision["transport"], "fixture");
    assert_eq!(decision["model"], "jev-latest");
    let line = fs::read_to_string(record.join("trace.jsonl")).unwrap();
    let decision_line = line
        .lines()
        .find(|line| line.contains("\"model_decision\""))
        .unwrap();
    assert!(
        !decision_line.contains(QUESTION),
        "the decision event never carries the question text"
    );

    let shown = execute(
        work.path(),
        &["show-run", record.to_str().unwrap(), "--json"],
    );
    assert!(shown.status.success(), "{}", describe(&shown));
    let report: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(report["model_decisions"].as_array().unwrap().len(), 1);
    assert_eq!(report["model_decisions"][0]["label"], "yes");
    assert_eq!(report["model_decisions"][0]["accepted"], true);
}

#[test]
fn a_rejected_classify_call_records_accepted_false_below_the_minimum() {
    let work = tempfile::tempdir().unwrap();
    write_classify_fixture(work.path(), 0.6);
    write_program(work.path());
    let record = work.path().join("record");

    let run = execute(
        work.path(),
        &[
            "run",
            "classify.program",
            "--record",
            record.to_str().unwrap(),
            "--model-fixtures",
            "fixtures",
        ],
    );
    assert!(run.status.success(), "{}", describe(&run));

    let decisions: Vec<Value> = trace_events(&record)
        .into_iter()
        .filter(|event| event["event"] == "model_decision")
        .collect();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["confidence_percent"], 60);
    assert_eq!(decisions[0]["minimum"], 80);
    assert_eq!(decisions[0]["accepted"], false);
}
