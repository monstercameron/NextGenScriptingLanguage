#![cfg(feature = "model")]
//! Issue 505: `--act-on` adds `act` and `reason` to a Jev classify, judge or
//! score result. Every case replays a fixture; no network is contacted.

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Output, Stdio};

const QUESTION: &str = "Is this line an error that needs a short explanation?";

fn invoke(arguments: &[&str], input: &str, fixture: &Value) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.json");
    std::fs::write(&path, serde_json::to_vec(fixture).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute-model"))
        .args(arguments)
        .arg("--fixture")
        .arg(path)
        .env("EXECUTE_MODELS_OFFLINE", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn normalized(output: Output) -> Value {
    assert!(
        output.status.success(),
        "unexpected failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn classify_fixture(labels: &[&str], minimum: u8, choice: &str, confidence: f64) -> Value {
    let other = (1.0 - confidence) / (labels.len() - 1) as f64;
    let probabilities = labels
        .iter()
        .map(|label| {
            let p = if *label == choice { confidence } else { other };
            ((*label).to_owned(), json!(p))
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "expected_request": {
            "provider": "jev", "kind": "classify", "model": "jev-latest",
            "state": "line one", "question": QUESTION,
            "labels": labels, "minimum": minimum
        },
        "response": {
            "model": "jev-latest",
            "answers": {"decision": {
                "type": "choice", "choice": choice, "confidence": confidence,
                "probabilities": probabilities
            }},
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }
    })
}

fn judge_fixture(minimum: u8, noul: f64) -> Value {
    json!({
        "expected_request": {
            "provider": "jev", "kind": "judge", "model": "jev-latest",
            "state": "line one", "question": QUESTION, "minimum": minimum
        },
        "response": {
            "model": "jev-latest",
            "answers": {"decision": {"type": "noul", "noul": noul}},
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }
    })
}

fn score_fixture(score: f64) -> Value {
    json!({
        "expected_request": {
            "provider": "jev", "kind": "score", "model": "jev-latest",
            "state": "line one", "question": QUESTION,
            "levels": ["low", "medium", "high"], "minimum": 0
        },
        "response": {
            "model": "jev-latest",
            "answers": {"decision": {
                "type": "score", "score": score,
                "legend": {"0": "low", "1": "medium", "2": "high"},
                "confidence": 0.9,
                "probabilities": {"0": 0.05, "1": 0.05, "2": 0.9}
            }},
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }
    })
}

fn classify_args<'a>(minimum: &'a str, act_on: &'a [&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "classify",
        "--provider",
        "jev",
        "--question",
        QUESTION,
        "--labels",
        "error",
        "warn",
        "noise",
        "--minimum",
        minimum,
    ];
    if !act_on.is_empty() {
        args.push("--act-on");
        args.extend_from_slice(act_on);
    }
    args
}

#[test]
fn classify_act_is_accepted_and_in_the_subset() {
    let result = normalized(invoke(
        &classify_args("80", &["error", "warn"]),
        "line one",
        &classify_fixture(&["error", "warn", "noise"], 80, "error", 0.9),
    ));
    assert_eq!(result["act"], json!(true));
    assert_eq!(result["reason"], json!("accepted"));
    assert_eq!(result["accepted"], json!(true));
}

#[test]
fn classify_accepted_but_other_label_does_not_act() {
    let result = normalized(invoke(
        &classify_args("80", &["error"]),
        "line one",
        &classify_fixture(&["error", "warn", "noise"], 80, "noise", 0.9),
    ));
    assert_eq!(result["act"], json!(false));
    assert_eq!(result["reason"], json!("other_label"));
    assert_eq!(result["accepted"], json!(true));
}

#[test]
fn classify_right_label_without_confidence_does_not_act() {
    let result = normalized(invoke(
        &classify_args("80", &["error"]),
        "line one",
        &classify_fixture(&["error", "warn", "noise"], 80, "error", 0.5),
    ));
    assert_eq!(result["act"], json!(false));
    assert_eq!(result["reason"], json!("low_confidence"));
    assert_eq!(result["accepted"], json!(false));
}

#[test]
fn without_act_on_the_result_has_no_act_or_reason_fields() {
    let result = normalized(invoke(
        &classify_args("80", &[]),
        "line one",
        &classify_fixture(&["error", "warn", "noise"], 80, "error", 0.9),
    ));
    let mut keys: Vec<&str> = result
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "accepted",
            "confidence_percent",
            "kind",
            "label",
            "model",
            "protocol",
            "provider",
            "transport",
            "usage"
        ]
    );
    assert_eq!(
        result,
        json!({
            "protocol": "execute.model/1", "provider": "jev", "model": "jev-latest",
            "kind": "classify", "transport": "fixture",
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
            "label": "error", "confidence_percent": 90, "accepted": true
        })
    );
}

#[test]
fn judge_acts_on_the_wanted_decision_only() {
    let yes = normalized(invoke(
        &[
            "judge",
            "--provider",
            "jev",
            "--question",
            QUESTION,
            "--act-on",
            "true",
        ],
        "line one",
        &judge_fixture(0, 0.9),
    ));
    assert_eq!(
        (yes["act"].clone(), yes["reason"].clone()),
        (json!(true), json!("accepted"))
    );

    let no = normalized(invoke(
        &[
            "judge",
            "--provider",
            "jev",
            "--question",
            QUESTION,
            "--act-on",
            "true",
        ],
        "line one",
        &judge_fixture(0, 0.1),
    ));
    assert_eq!(
        (no["act"].clone(), no["reason"].clone()),
        (json!(false), json!("other_label"))
    );
}

#[test]
fn score_exact_level_and_at_least_level() {
    let base = [
        "score",
        "--provider",
        "jev",
        "--question",
        QUESTION,
        "--levels",
        "low",
        "medium",
        "high",
    ];
    let mut exact = base.to_vec();
    exact.extend(["--act-on", "medium"]);
    let high = normalized(invoke(&exact, "line one", &score_fixture(2.0)));
    assert_eq!(
        (high["act"].clone(), high["reason"].clone()),
        (json!(false), json!("other_label"))
    );
    let medium = normalized(invoke(&exact, "line one", &score_fixture(1.0)));
    assert_eq!(
        (medium["act"].clone(), medium["reason"].clone()),
        (json!(true), json!("accepted"))
    );

    let mut at_least = base.to_vec();
    at_least.extend(["--act-on", "at_least", "medium"]);
    let high = normalized(invoke(&at_least, "line one", &score_fixture(2.0)));
    assert_eq!(
        (high["act"].clone(), high["reason"].clone()),
        (json!(true), json!("accepted"))
    );
    let low = normalized(invoke(&at_least, "line one", &score_fixture(0.0)));
    assert_eq!(
        (low["act"].clone(), low["reason"].clone()),
        (json!(false), json!("other_label"))
    );
}

#[test]
fn an_existing_gauntlet_fixture_replays_with_act_computed_from_it() {
    let path = format!(
        "{}/examples/ai-gauntlet/fixtures/001/confident.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let run = |extra: &[&str]| {
        let mut args = vec![
            "classify", "--provider", "jev",
            "--question", "Is this line an error that needs a short explanation, rather than a warning or routine noise?",
            "--labels", "error", "warn", "noise", "--minimum", "80",
            "--fixture", path.as_str(),
        ];
        args.extend_from_slice(extra);
        Command::new(env!("CARGO_BIN_EXE_execute-model"))
            .args(&args)
            .env("EXECUTE_MODELS_OFFLINE", "")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"cache: could not open index; rebuild required\n")?;
                child.wait_with_output()
            })
            .unwrap()
    };
    let with = normalized(run(&["--act-on", "error"]));
    assert_eq!(with["label"], json!("error"));
    assert_eq!(
        (with["act"].clone(), with["reason"].clone()),
        (json!(true), json!("accepted"))
    );
    let without = normalized(run(&[]));
    assert!(without.get("act").is_none());
    assert!(without.get("reason").is_none());
}

#[test]
fn bad_act_on_values_fail_before_any_call() {
    let fixture = classify_fixture(&["error", "warn", "noise"], 80, "error", 0.9);
    let cases: [&[&str]; 2] = [&["missing"], &["error", "error"]];
    for act_on in cases {
        let output = invoke(&classify_args("80", act_on), "line one", &fixture);
        assert!(!output.status.success(), "accepted {act_on:?}");
        assert!(output.stdout.is_empty());
    }
    // `--act-on` with no values before the next flag is an empty list.
    let output = invoke(
        &[
            "classify",
            "--provider",
            "jev",
            "--question",
            QUESTION,
            "--labels",
            "error",
            "warn",
            "noise",
            "--minimum",
            "80",
            "--act-on",
        ],
        "line one",
        &fixture,
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let judge = json!({"expected_request": {}, "response": {}});
    let output = invoke(
        &[
            "judge",
            "--provider",
            "jev",
            "--question",
            QUESTION,
            "--act-on",
            "maybe",
        ],
        "line one",
        &judge,
    );
    assert!(!output.status.success());
    let output = invoke(
        &[
            "score",
            "--provider",
            "jev",
            "--question",
            QUESTION,
            "--levels",
            "low",
            "high",
            "--act-on",
            "at_least",
            "urgent",
        ],
        "line one",
        &score_fixture(1.0),
    );
    assert!(!output.status.success());
}
