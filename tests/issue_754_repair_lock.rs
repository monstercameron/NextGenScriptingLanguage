//! Issue #754: a repair holds the checkpoint lock from read through commit, so a
//! second repair of the same checkpoint is refused while the first holds it, and
//! a plan whose program has changed is refused before anything is written.
//!
//! The repair CLI lives in the binary crate, so these tests drive the public
//! `flow::repair` API that the CLI calls, in the same order: acquire the lock,
//! plan, check the program is unchanged, then commit.

use flow::manifest::Registry;
use flow::repair::{self, Operation};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const SOURCE: &str = "run \"false\"\n";
const OPERATION: &str = "replace-command 1 run \"true\"";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn plan_for(source: &str, path: &Path, operation: &str) -> repair::Plan {
    let operations = [Operation::parse(operation).unwrap()];
    let boundary = repair::boundary_line_at(source, Some(path), 0).unwrap();
    repair::apply_all_at(
        source,
        Some(path),
        &operations,
        boundary,
        &Registry::default(),
    )
    .unwrap()
}

fn audit_lines(checkpoint: &Path) -> Vec<serde_json::Value> {
    let log = checkpoint.join(repair::REPAIRS_FILE);
    match fs::read_to_string(&log) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn a_second_repair_is_refused_while_the_first_holds_the_checkpoint_lock() {
    let work = tempfile::tempdir().unwrap();
    let checkpoint = work.path().join("checkpoint");
    fs::create_dir(&checkpoint).unwrap();
    let program = work.path().join("main.program");
    fs::write(&program, SOURCE).unwrap();

    // The first repair takes the lock before it reads the program.
    let first = repair::acquire_repair_lock(&checkpoint).unwrap();

    // A second repair, or a resume, of the same checkpoint must be refused with
    // the busy diagnostic while the first holds the lock.
    let error = repair::acquire_repair_lock(&checkpoint)
        .err()
        .expect("a second repair must be refused while the lock is held");
    assert!(format!("{error:#}").contains("locked"), "{error:#}");

    // The refused repair wrote nothing: the program and the audit log are unchanged.
    assert_eq!(fs::read_to_string(&program).unwrap(), SOURCE);
    assert!(audit_lines(&checkpoint).is_empty());
    assert!(!checkpoint.join(repair::REPAIRS_FILE).exists());

    // After the first repair releases the lock, the next repair can take it.
    drop(first);
    let second = repair::acquire_repair_lock(&checkpoint);
    assert!(second.is_ok(), "the lock must be free after release");
}

#[test]
fn a_committed_repair_records_one_sequence_with_matching_source_hashes() {
    let work = tempfile::tempdir().unwrap();
    let checkpoint = work.path().join("checkpoint");
    fs::create_dir(&checkpoint).unwrap();
    let program = work.path().join("main.program");
    fs::write(&program, SOURCE).unwrap();

    let lock = repair::acquire_repair_lock(&checkpoint).unwrap();
    let plan = plan_for(SOURCE, &program, OPERATION);
    repair::ensure_program_unchanged(&program, &plan).unwrap();
    let sequence = repair::commit(&checkpoint, &program, &plan, OPERATION).unwrap();
    drop(lock);

    assert_eq!(sequence, 1);
    let records = audit_lines(&checkpoint);
    assert_eq!(records.len(), 1, "exactly one audit entry: {records:?}");
    let record = &records[0];
    assert_eq!(record["sequence"], 1);
    assert_eq!(record["before_sha256"], sha(SOURCE.as_bytes()));
    assert_eq!(record["after_sha256"], sha(plan.after.as_bytes()));
    assert_eq!(record["operation"], OPERATION);
    // The installed program is exactly the text the audit names as after.
    assert_eq!(
        fs::read(&program).unwrap(),
        plan.after.as_bytes(),
        "the program must hold the recorded after text"
    );
    // The before copy is stored under the before hash.
    let copy = checkpoint
        .join(repair::REPAIR_COPIES_DIR)
        .join(format!("{}.program", sha(SOURCE.as_bytes())));
    assert_eq!(fs::read_to_string(&copy).unwrap(), SOURCE);

    // A second repair, planned from the new program, chains onto the first: its
    // sequence is 2 and its before hash is the first repair's after hash.
    let lock = repair::acquire_repair_lock(&checkpoint).unwrap();
    let current = fs::read_to_string(&program).unwrap();
    let second_operation = "replace-command 1 run \"echo\"";
    let next = plan_for(&current, &program, second_operation);
    repair::ensure_program_unchanged(&program, &next).unwrap();
    let sequence = repair::commit(&checkpoint, &program, &next, second_operation).unwrap();
    drop(lock);
    assert_eq!(sequence, 2);
    let records = audit_lines(&checkpoint);
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["sequence"], 2);
    assert_eq!(records[1]["before_sha256"], records[0]["after_sha256"]);
}

#[test]
fn a_stale_plan_is_refused_before_anything_is_written() {
    let work = tempfile::tempdir().unwrap();
    let checkpoint = work.path().join("checkpoint");
    fs::create_dir(&checkpoint).unwrap();
    let program = work.path().join("main.program");
    fs::write(&program, SOURCE).unwrap();

    let lock = repair::acquire_repair_lock(&checkpoint).unwrap();
    let plan = plan_for(SOURCE, &program, OPERATION);

    // The program changes after the plan was made from it.
    let edited = "run \"echo\"\n";
    fs::write(&program, edited).unwrap();

    let error = repair::ensure_program_unchanged(&program, &plan)
        .expect_err("a stale plan must be refused");
    assert!(format!("{error:#}").contains("changed"), "{error:#}");
    drop(lock);

    // Nothing was written: the edited program is kept and no audit entry or
    // before copy was made.
    assert_eq!(fs::read_to_string(&program).unwrap(), edited);
    assert!(audit_lines(&checkpoint).is_empty());
    assert!(!checkpoint.join(repair::REPAIR_COPIES_DIR).exists());
}
