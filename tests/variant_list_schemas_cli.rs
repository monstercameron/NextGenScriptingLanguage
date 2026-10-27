//! CLI-level coverage for the shipped JSON variant schemas.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn invoke(root: &Path, command: &str, source: &str, inputs: &[String]) -> Output {
    let program = root.join("case.program");
    fs::write(&program, source).unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_execute"));
    process.current_dir(root).arg(command).arg(program);
    for input in inputs {
        process.arg("--input").arg(input);
    }
    process.output().unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn literal(value: &Value) -> String {
    serde_json::to_string(value).unwrap()
}

fn diagnostic(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn transaction_variants_and_list_bounds_check_and_run_through_cli() {
    let directory = tempfile::tempdir().unwrap();
    let write_path = directory.path().join("write-target.txt");
    let remove_path = directory.path().join("remove-target.txt");
    fs::write(&write_path, b"before write").unwrap();
    fs::write(&remove_path, b"before remove").unwrap();
    let operations = json!([
        {
            "action":"write",
            "path":write_path.to_string_lossy(),
            "expected_content_hash":sha256(b"before write"),
            "content":"after write"
        },
        {
            "action":"remove",
            "path":remove_path.to_string_lossy(),
            "expected_content_hash":sha256(b"before remove")
        }
    ]);
    let valid_source = format!(
        "run fs.transaction with journal \"valid-journal\" and operations {}\n",
        literal(&operations)
    );
    let checked = invoke(directory.path(), "check", &valid_source, &[]);
    assert!(checked.status.success(), "{}", diagnostic(&checked));
    let run = invoke(directory.path(), "run", &valid_source, &[]);
    assert!(run.status.success(), "{}", diagnostic(&run));
    assert_eq!(fs::read(&write_path).unwrap(), b"after write");
    assert!(!remove_path.exists());
    assert!(directory.path().join("valid-journal").exists());

    let too_many = Value::Array(
        (0..33)
            .map(|index| {
                json!({
                    "action":"remove",
                    "path":format!("missing-{index}"),
                    "expected_content_hash":"0000000000000000000000000000000000000000000000000000000000000000"
                })
            })
            .collect(),
    );
    let invalid_operations = [
        json!([]),
        too_many,
        json!([1]),
        json!([{"action":"write","path":"target","expected_content_hash":"hash"}]),
        json!([{"action":"remove","path":"target","expected_content_hash":"hash","content":"unexpected"}]),
        json!([{"action":"copy","path":"target","expected_content_hash":"hash"}]),
    ];
    for (index, operations) in invalid_operations.iter().enumerate() {
        let journal = format!("rejected-journal-{index}");
        let source = format!(
            "run fs.transaction with journal {} and operations {}\n",
            literal(&json!(journal)),
            literal(operations)
        );
        for command in ["check", "run"] {
            let output = invoke(directory.path(), command, &source, &[]);
            assert!(
                !output.status.success(),
                "{command} accepted {operations}: {}",
                diagnostic(&output)
            );
            assert!(
                !directory.path().join(&journal).exists(),
                "{command} created journal {journal} for an invalid operation"
            );
        }
    }

    // A JSON input reference passes static checking. Once resolved, malformed
    // write operations must fail before creating a journal or changing files.
    let dynamic_path = directory.path().join("dynamic-target.txt");
    fs::write(&dynamic_path, b"unchanged").unwrap();
    let dynamic_source =
        "accept operations as json\nrun fs.transaction with journal \"dynamic-journal\" and operations operations\n";
    let checked = invoke(directory.path(), "check", dynamic_source, &[]);
    assert!(checked.status.success(), "{}", diagnostic(&checked));

    let valid_dynamic = json!([{
        "action":"write",
        "path":dynamic_path.to_string_lossy(),
        "expected_content_hash":sha256(b"unchanged"),
        "content":"dynamic success"
    }]);
    let run = invoke(
        directory.path(),
        "run",
        dynamic_source,
        &[format!("operations={}", literal(&valid_dynamic))],
    );
    assert!(run.status.success(), "{}", diagnostic(&run));
    assert_eq!(fs::read(&dynamic_path).unwrap(), b"dynamic success");
    assert!(directory.path().join("dynamic-journal").exists());

    fs::write(&dynamic_path, b"still unchanged after invalid input").unwrap();
    let rejected_journal = directory.path().join("dynamic-rejected-journal");
    let invalid_dynamic = json!([{
        "action":"write",
        "path":dynamic_path.to_string_lossy(),
        "expected_content_hash":sha256(b"still unchanged after invalid input")
    }]);
    let source = format!(
        "accept operations as json\nrun fs.transaction with journal {} and operations operations\n",
        literal(&json!(rejected_journal.to_string_lossy()))
    );
    let checked = invoke(directory.path(), "check", &source, &[]);
    assert!(checked.status.success(), "{}", diagnostic(&checked));
    let run = invoke(
        directory.path(),
        "run",
        &source,
        &[format!("operations={}", literal(&invalid_dynamic))],
    );
    assert!(
        !run.status.success(),
        "runtime accepted malformed write operation"
    );
    assert!(
        diagnostic(&run).contains("must match exactly one schema variant"),
        "{}",
        diagnostic(&run)
    );
    assert_eq!(
        fs::read(&dynamic_path).unwrap(),
        b"still unchanged after invalid input"
    );
    assert!(
        !rejected_journal.exists(),
        "runtime rejection created the journal"
    );
}

#[test]
fn filter_variants_and_list_bounds_check_and_run_through_cli() {
    let directory = tempfile::tempdir().unwrap();
    let document = json!([{"active":true,"name":"Ada","score":7}]);
    let predicates = json!([
        {"pointer":"/active","exists":true},
        {"pointer":"/name","equals":"Ada"},
        {"pointer":"/missing","not_equals":"x"},
        {"pointer":"/score","type":"number"}
    ]);
    let valid_source = format!(
        "run json.filter with document {} and where {}\n",
        literal(&document),
        literal(&predicates)
    );
    let checked = invoke(directory.path(), "check", &valid_source, &[]);
    assert!(checked.status.success(), "{}", diagnostic(&checked));
    let run = invoke(directory.path(), "run", &valid_source, &[]);
    assert!(run.status.success(), "{}", diagnostic(&run));

    // Empty predicates are valid and select the whole array.
    let empty_source = format!(
        "run json.filter with document {} and where []\n",
        literal(&document)
    );
    assert!(invoke(directory.path(), "check", &empty_source, &[])
        .status
        .success());
    assert!(invoke(directory.path(), "run", &empty_source, &[])
        .status
        .success());

    let too_many = Value::Array((0..33).map(|_| json!({"exists":true})).collect());
    let invalid_predicates = [
        too_many,
        json!([1]),
        json!([{"type":"task"}]),
        json!([{"exists":"yes"}]),
        json!([{"exists":true,"equals":1}]),
        json!([{"equals":1,"extra":true}]),
    ];
    for predicates in &invalid_predicates {
        let source = format!(
            "run json.filter with document {} and where {}\n",
            literal(&document),
            literal(predicates)
        );
        for command in ["check", "run"] {
            let output = invoke(directory.path(), command, &source, &[]);
            assert!(
                !output.status.success(),
                "{command} accepted {predicates}: {}",
                diagnostic(&output)
            );
        }
    }

    // The input reference is deferred by check, then validated after input
    // parsing and before json.filter executes.
    let dynamic_source = format!(
        "accept predicates as json\nrun json.filter with document {} and where predicates\n",
        literal(&document)
    );
    let checked = invoke(directory.path(), "check", &dynamic_source, &[]);
    assert!(checked.status.success(), "{}", diagnostic(&checked));
    let valid_dynamic = json!([{"pointer":"/name","equals":"Ada"}]);
    let run = invoke(
        directory.path(),
        "run",
        &dynamic_source,
        &[format!("predicates={}", literal(&valid_dynamic))],
    );
    assert!(run.status.success(), "{}", diagnostic(&run));

    let invalid_dynamic = json!([{"type":"task"}]);
    let run = invoke(
        directory.path(),
        "run",
        &dynamic_source,
        &[format!("predicates={}", literal(&invalid_dynamic))],
    );
    assert!(
        !run.status.success(),
        "runtime accepted malformed predicate"
    );
    assert!(
        diagnostic(&run).contains("must match exactly one schema variant"),
        "{}",
        diagnostic(&run)
    );

    let too_many_dynamic = Value::Array((0..33).map(|_| json!({"exists":true})).collect());
    let run = invoke(
        directory.path(),
        "run",
        &dynamic_source,
        &[format!("predicates={}", literal(&too_many_dynamic))],
    );
    assert!(!run.status.success(), "runtime accepted 33 predicates");
    assert!(
        diagnostic(&run).contains("allows at most 32 item(s)"),
        "{}",
        diagnostic(&run)
    );
}
