#![cfg(feature = "records")]
//! `execute run --record DIR` and `execute show-run`: a failed run can be diagnosed
//! from its record, the record is private and bounded, and declared secrets never
//! appear in it.

#[path = "support/private_answers.rs"]
mod private_answers;

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap()
}

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

/// The executable path as program text. Forward slashes need no escaping.
fn executable_text() -> String {
    env!("CARGO_BIN_EXE_execute").replace('\\', "/")
}

/// A program whose second step fails: this executable rejects an unknown option.
fn failing_program() -> String {
    format!(
        "run printf with message \"before\"\nrun \"{}\" with arguments [\"--no-such-option\"]\n",
        executable_text()
    )
}

/// Every file under `directory`, with its relative path.
fn files_under(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = fs::read(&path).unwrap();
                found.push((path.strip_prefix(directory).unwrap().to_path_buf(), bytes));
            }
        }
    }
    found
}

#[test]
fn a_failed_run_is_diagnosed_from_its_record_alone() {
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("failing.program"), failing_program()).unwrap();
    let record = work.path().join("record");

    let run = execute(
        work.path(),
        &[
            "run",
            "failing.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(!run.status.success(), "the program should fail");
    assert!(
        record.join("run.json").is_file(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(record.join("trace.jsonl").is_file());
    assert!(record.join("program.program").is_file());

    let document: Value =
        serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
    assert_eq!(document["protocol"], "execute.record/1");
    assert_eq!(document["result"]["ok"], false);
    assert_eq!(document["steps"], 2);
    assert_eq!(document["truncated"], false);

    let summary = execute(
        work.path(),
        &["show-run", record.to_str().unwrap(), "--json"],
    );
    assert!(
        summary.status.success(),
        "{}",
        String::from_utf8_lossy(&summary.stderr)
    );
    let report: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert_eq!(report["protocol"], "execute.show-run/1");
    assert_eq!(report["ok"], false);
    assert_eq!(report["steps"], 2);
    assert_eq!(report["failed_step"]["index"], 1);
    assert_eq!(report["failed_step"]["line"], 2);
    assert_eq!(report["failed_step"]["status"], 1);
    assert_eq!(report["failed_step"]["kind"], "failure");
    assert!(report["failed_step"]["certainty"].is_string());
    assert_eq!(report["slowest_steps"].as_array().unwrap().len(), 2);

    let text = execute(work.path(), &["show-run", record.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("failed step: #1 line 2"), "{text}");
}

#[test]
fn a_successful_run_records_each_step_output_by_hash() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"hello\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hello\n");

    let document: Value =
        serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
    assert_eq!(document["result"]["ok"], true);
    // Unconsumed output streams to the terminal as it is produced, so the final
    // value is empty. The bytes are kept with the step that produced them.
    assert_eq!(document["result"]["output"]["bytes"], 0);
    assert_eq!(
        document["result"]["output"]["sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );

    let trace = fs::read_to_string(record.join("trace.jsonl")).unwrap();
    let step: Value = serde_json::from_str(trace.lines().next().unwrap()).unwrap();
    assert_eq!(step["line"], 1);
    assert_eq!(step["status"], 0);
    assert_eq!(step["stdout"]["bytes"], 6);
    assert!(step["wall_us"].is_u64());
    let blob = step["stdout"]["blob"].as_str().unwrap();
    assert_eq!(fs::read(record.join(blob)).unwrap(), b"hello\n");
}

#[test]
#[cfg(unix)]
fn sensitive_prompt_values_are_redacted_everywhere_in_the_record() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-SECRET-12345";
    fs::write(
        work.path().join("secret.program"),
        "accept token as text from secret prompt \"Access token?\"\nuse value token\nthen \"cat\"\n",
    )
    .unwrap();
    let answers = work.path().join("answers.json");
    fs::write(&answers, serde_json::json!({"token": canary}).to_string()).unwrap();
    private_answers::make_private(&answers);
    let record = work.path().join("record");
    let _run = execute(
        work.path(),
        &[
            "run",
            "secret.program",
            "--answers",
            answers.to_str().unwrap(),
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(record.join("trace.jsonl").is_file());
    for (_, bytes) in files_under(&record) {
        assert!(!String::from_utf8_lossy(&bytes).contains(canary));
    }
}

#[test]
#[cfg(unix)]
fn sensitive_environment_values_are_redacted_everywhere_in_the_record() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-ENV-777";
    fs::write(
        work.path().join("environment.program"),
        "accept key as text from environment \"API_KEY\"\nuse value key\nthen \"cat\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let _run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .arg("run")
        .arg("environment.program")
        .arg("--record")
        .arg(&record)
        .arg("--manifest-dir")
        .arg(manifests())
        .env("API_KEY", canary)
        .output()
        .unwrap();
    assert!(record.join("trace.jsonl").is_file());
    for (_, bytes) in files_under(&record) {
        assert!(!String::from_utf8_lossy(&bytes).contains(canary));
    }
}

#[test]
#[cfg(unix)]
fn runtime_marked_output_is_withheld_from_the_entire_record() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-RUNTIME-SECRET-991";
    fs::write(
        work.path().join("secret.program"),
        "run \"sh\" with arguments [\"-c\", \"printf '%s' \\\"$RUNTIME_SECRET\\\"\"] as token and keep secret\nuse value token\nthen \"wc\" with arguments [\"-c\"]\nrun \"sh\" with arguments [\"-c\", \"test -n \\\"$TOKEN\\\"\"] and environment {\"TOKEN\": token}\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .args([
            "run",
            "secret.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
        ])
        .arg(manifests())
        .env("RUNTIME_SECRET", canary)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    for (_, bytes) in files_under(&record) {
        assert!(!String::from_utf8_lossy(&bytes).contains(canary));
    }

    fs::write(
        work.path().join("argv.program"),
        "run \"sh\" with arguments [\"-c\", \"printf '%s' \\\"$RUNTIME_SECRET\\\"\"] as token and keep secret\nrun printf with message token\n",
    )
    .unwrap();
    let check = execute(
        work.path(),
        &[
            "check",
            "argv.program",
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(!check.status.success());
    assert!(
        String::from_utf8_lossy(&check.stderr).contains("cannot be passed as a command argument")
    );
}

#[test]
#[cfg(unix)]
fn secret_reusable_program_results_withhold_inner_outputs_from_the_record() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-NESTED-SECRET-315";
    fs::write(work.path().join("credential.txt"), canary).unwrap();
    fs::write(
        work.path().join("secret.program"),
        "define a program named reveal\n  run printf with message \"CANARY-NESTED-SOURCE-315\"\n  run \"cat\" with arguments [\"credential.txt\"]\nrun program reveal as token and keep secret\nuse value token\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .args(["run", "secret.program", "--record"])
        .arg(&record)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    for (path, bytes) in files_under(&record) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(canary)
                && !String::from_utf8_lossy(&bytes).contains("CANARY-NESTED-SOURCE-315"),
            "nested secret appeared in {}",
            path.display()
        );
    }
    let trace = fs::read_to_string(record.join("trace.jsonl")).unwrap();
    assert!(trace.contains("\"withheld\":\"policy\""), "{trace}");
}

#[test]
#[cfg(unix)]
fn task_group_secret_output_is_redacted_from_every_record_file() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-TASK-GROUP-OUTPUT-315";
    fs::write(
        work.path().join("secret.program"),
        format!(
            "parallel at most 1 as results\n  run printf with message \"{canary}\" as token and keep secret\nrun printf with message \"done\"\n"
        ),
    )
    .unwrap();
    let record = work.path().join("record");
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .args(["run", "secret.program", "--record"])
        .arg(&record)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    for (path, bytes) in files_under(&record) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(canary),
            "task-group secret appeared in {}",
            path.display()
        );
    }
}

#[test]
#[cfg(unix)]
fn declared_secret_input_floor_withholds_transformed_output_from_records() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-DECLARED-SECRET-DERIVATION-315";
    let prefix = "CANARY-DECLARED";
    fs::write(
        work.path().join("secret.program"),
        "accept token as text from secret prompt \"Token?\"\nuse value token\nrun \"sh\" with arguments [\"-c\", \"read value; printf %.15s \\\"$value\\\"\"]\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .args(["run", "secret.program", "--record"])
        .arg(&record)
        .arg("--input")
        .arg(format!("token={canary}"))
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    for (path, bytes) in files_under(&record) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(prefix),
            "derived secret appeared in {}",
            path.display()
        );
    }
}

#[test]
#[cfg(unix)]
fn marked_reusable_call_scans_embedded_task_invocation_source() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-EMBEDDED-TASK-SOURCE-315";
    fs::write(
        work.path().join("secret.program"),
        format!(
            "define a program named worker\n  parallel at most 1 as results\n    run printf with message \"{canary}\" as token\nrun program worker as result and keep secret\nuse value result\n"
        ),
    )
    .unwrap();
    let record = work.path().join("record");
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .args(["run", "secret.program", "--record"])
        .arg(&record)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    for (path, bytes) in files_under(&record) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(canary),
            "embedded source secret appeared in {}",
            path.display()
        );
    }
}

#[test]
#[cfg(unix)]
fn parallel_each_secret_items_are_redacted_from_every_record_file() {
    let work = tempfile::tempdir().unwrap();
    let canary = "CANARY-PARALLEL-EACH-ITEM-315";
    fs::write(
        work.path().join("secret.program"),
        "accept tokens as json from environment \"RUNTIME_TOKENS\"\nparallel for each item in tokens at most 1 as results\n  run number.add with left 1 and right 2\nrun printf with message \"done\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .args(["run", "secret.program", "--record"])
        .arg(&record)
        .arg("--manifest-dir")
        .arg(manifests())
        .env("RUNTIME_TOKENS", format!("[\"{canary}\"]"))
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    for (path, bytes) in files_under(&record) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(canary),
            "parallel-each secret appeared in {}",
            path.display()
        );
    }
}

#[test]
fn an_existing_non_empty_record_directory_is_not_reused() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"x\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    fs::create_dir(&record).unwrap();
    fs::write(record.join("keep.txt"), "mine").unwrap();
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("not empty"));
    assert_eq!(fs::read_to_string(record.join("keep.txt")).unwrap(), "mine");
}

#[cfg(unix)]
#[test]
fn record_files_are_private_to_the_owner() {
    use std::os::unix::fs::PermissionsExt;
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"x\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let directory_mode = fs::metadata(&record).unwrap().permissions().mode() & 0o777;
    assert_eq!(directory_mode, 0o700);
    for name in ["run.json", "trace.jsonl", "program.program"] {
        let mode = fs::metadata(record.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
}

#[test]
fn declared_secret_environment_values_never_appear_in_the_record() {
    let work = tempfile::tempdir().unwrap();
    let sentinel = "record-fixture-secret-must-not-appear";
    // Both shells expand the variable into the child's own stdout.
    let (binary, arguments) = if cfg!(windows) {
        (
            "cmd",
            serde_json::json!([{"literal": "/c"}, {"literal": "echo %RECORD_SENTINEL%"}]),
        )
    } else {
        (
            "/bin/sh",
            serde_json::json!([{"literal": "-c"}, {"literal": "printf '%s\\n' \"$RECORD_SENTINEL\""}]),
        )
    };
    let manifest = serde_json::json!({
        "name": "secret.echo",
        "binary": binary,
        "arguments": arguments,
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "environment": {"set": {"RECORD_SENTINEL": sentinel}}
        }
    });
    let manifest_dir = work.path().join("manifests");
    fs::create_dir(&manifest_dir).unwrap();
    fs::write(manifest_dir.join("secret.echo.json"), manifest.to_string()).unwrap();
    let policy = work.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"version":1,"secrets":{"environment":{"secret.echo":["RECORD_SENTINEL"]}}}"#,
    )
    .unwrap();
    // The policy loader accepts only an owner-private file: on Windows an
    // inherited tempdir DACL is refused before the record redaction runs.
    private_answers::make_private(&policy);
    fs::write(work.path().join("secret.program"), "run secret.echo\n").unwrap();
    let record = work.path().join("record");

    let run = execute(
        work.path(),
        &[
            "run",
            "secret.program",
            "--manifest-dir",
            manifest_dir.to_str().unwrap(),
            "--data-flow-policy",
            policy.to_str().unwrap(),
            "--record",
            record.to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    // The program still delivers the secret to its own output; only the record redacts it.
    assert!(String::from_utf8_lossy(&run.stdout).contains(sentinel));

    for (name, bytes) in files_under(&record) {
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(sentinel),
            "{} contains the secret",
            name.display()
        );
    }
    let summary = execute(work.path(), &["show-run", record.to_str().unwrap()]);
    assert!(!String::from_utf8_lossy(&summary.stdout).contains(sentinel));
    let document: Value =
        serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
    assert_eq!(
        document["redaction"]["declared_secret_names"][0],
        "RECORD_SENTINEL"
    );

    // The output of the secret-environment applet is withheld: no bytes, no hash, and
    // a marker naming the policy.
    let trace = fs::read_to_string(record.join("trace.jsonl")).unwrap();
    let secret_step: Value = trace
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|step| step["applets"][0] == "secret.echo")
        .expect("the secret applet step");
    assert!(secret_step["stdout"]["blob"].is_null(), "{secret_step}");
    assert_eq!(secret_step["stdout"]["withheld"], "policy");
    assert!(
        secret_step["stdout"]["sha256"].is_null(),
        "a withheld output has no hash"
    );
    assert_eq!(secret_step["stderr"]["tail"], "");
    assert_eq!(document["terminal"]["withheld"], "policy");
}

#[test]
fn explicitly_public_environment_values_can_be_recorded_and_passed_in_argv() {
    let work = tempfile::tempdir().unwrap();
    let public_home = "/public-home-record-fixture";
    fs::write(
        work.path().join("public.program"),
        "accept home_dir as text from public environment \"HOME\"\nrun \"printf\" with arguments [\"%s\", home_dir]\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            "public.program",
            "--record",
            record.to_str().unwrap(),
        ])
        .env("HOME", public_home)
        .current_dir(work.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, public_home.as_bytes());

    let trace = fs::read_to_string(record.join("trace.jsonl")).unwrap();
    assert!(trace.contains(public_home), "{trace}");
}

#[test]
fn successful_secret_pipeline_redacts_earlier_stage_stderr_from_the_record() {
    let work = tempfile::tempdir().unwrap();
    let canary = "PIPELINE-STDERR-CANARY-315";
    fs::write(
        work.path().join("secret.program"),
        "accept token as text from secret prompt \"Token?\"\nuse value token\npipe to \"sh\" with arguments [\"-c\", \"head -c 16 >&2; cat\"]\npipe to \"cat\" as echoed\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "secret.program",
            "--input",
            &format!("token={canary}"),
            "--record",
            record.to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let fragment = &canary[..16];
    for (path, bytes) in files_under(&record) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(fragment),
            "secret fragment appeared in {}",
            path.display()
        );
    }
}

#[cfg(feature = "records")]
#[test]
fn long_recorded_runs_stop_growing_the_trace_at_its_limit() {
    let work = tempfile::tempdir().unwrap();
    let manifest_dir = work.path().join("manifests");
    fs::create_dir(&manifest_dir).unwrap();
    let mut concat: Value =
        serde_json::from_slice(&fs::read(manifests().join("core.text_concat.json")).unwrap())
            .unwrap();
    concat.as_object_mut().unwrap().remove("description");
    fs::write(
        manifest_dir.join("text.concat.json"),
        serde_json::to_vec(&concat).unwrap(),
    )
    .unwrap();
    let program =
        "repeat 100000 times\r\n  run text.concat with parts [\"a\", \"b\"] and separator \"-\"";
    assert_eq!(program.len(), 78);
    fs::write(work.path().join("long.program"), program).unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "long.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifest_dir.to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let trace_bytes = fs::metadata(record.join("trace.jsonl")).unwrap().len();
    assert!(trace_bytes <= flow::record::MAX_TRACE_BYTES as u64);
    let document: Value =
        serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
    assert_eq!(document["truncated"], true);
    assert_eq!(document["steps"], 100000);
}

#[test]
fn record_and_replay_use_the_recorded_loop_limit() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("loop.program"),
        "repeat 4 times\n  use value \"ok\"\n",
    )
    .unwrap();
    let record = work.path().join("loop-record");
    let run = execute(
        work.path(),
        &[
            "run",
            "loop.program",
            "--record",
            record.to_str().unwrap(),
            "--max-loop-iterations",
            "4",
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let path = record.join("run.json");
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(document["limits"]["loop_iterations_max"], 4);
    assert_eq!(document["limits"]["loop_iterations_scope"], "per_loop");
    document["limits"]["loop_iterations_max"] = 3.into();
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let replay = execute(work.path(), &["replay", record.to_str().unwrap()]);
    assert!(
        !replay.status.success(),
        "replay should enforce the stored limit"
    );
    assert!(
        String::from_utf8_lossy(&replay.stderr).contains("budget of 3"),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
}

#[test]
fn legacy_record_replay_preserves_shared_loop_budget_across_loops() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("legacy.program"),
        "repeat 60000 times\n  use value \"first\"\nrepeat 60000 times\n  use value \"second\"\n",
    )
    .unwrap();
    let record = work.path().join("legacy-record");
    let run = execute(
        work.path(),
        &[
            "run",
            "legacy.program",
            "--record",
            record.to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let path = record.join("run.json");
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["limits"]
        .as_object_mut()
        .unwrap()
        .remove("loop_iterations_max");
    document["limits"]
        .as_object_mut()
        .unwrap()
        .remove("loop_iterations_scope");
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let replay = execute(work.path(), &["replay", record.to_str().unwrap()]);
    assert!(!replay.status.success());
    let stderr = String::from_utf8_lossy(&replay.stderr);
    assert!(
        stderr.contains("line 3") && stderr.contains("budget of 100000"),
        "{stderr}"
    );
}

#[test]
#[cfg(unix)]
fn a_real_denial_is_recorded_with_policy_identity_and_a_seal() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("denied.program"),
        "accept key as text from environment \"LANE_Q493_DENIED\"\nfail with message \"completed\"\n",
    )
    .unwrap();
    let policy = work.path().join("policy.json");
    // The decoy allowlist entry exercises privacy: it is policy material that
    // must never reach the record, while the denied variable stays denied.
    fs::write(
        &policy,
        r#"{"version":1,"applets":{},"host_environment":["CANARY_Q493_ALLOWED"]}"#,
    )
    .unwrap();
    private_answers::make_private(&policy);
    let record = work.path().join("record");

    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .args([
            "run",
            "denied.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ])
        .env("LANE_Q493_DENIED", "record-fixture-secret-must-not-appear")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("capability policy denies reading environment variable"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("record-fixture-secret-must-not-appear"),
        "stderr: {stderr}"
    );

    // The denial went through a real policy check, so the record names the
    // policy that produced it: status, version and a one-way content digest.
    let document: Value =
        serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
    assert_eq!(document["policy"]["execution_mode"], "untrusted");
    assert_eq!(
        document["policy"]["capability_policy"]["status"],
        "configured"
    );
    assert_eq!(document["policy"]["capability_policy"]["version"], 1);
    assert_eq!(
        document["policy"]["capability_policy"]["sha256"]
            .as_str()
            .map(str::len),
        Some(64),
        "{document}"
    );
    let denied = document["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|decision| decision["outcome"] == "denied")
        .expect("the real denial is in the persisted log");
    assert_eq!(denied["action"], "environment.read");
    assert!(
        document["audit_mac"].as_str().map(str::len) == Some(64),
        "{document}"
    );
    // The record names the policy by identity only: no grant, path, origin
    // or allowlist material — including the decoy entry above — reaches it.
    let raw = fs::read(record.join("run.json")).unwrap();
    assert!(
        !String::from_utf8_lossy(&raw).contains("CANARY_Q493_ALLOWED"),
        "run.json carries policy material"
    );
    let policy = &document["policy"];
    for name in ["capability_policy", "data_flow_policy"] {
        let identity = policy[name].as_object().expect("policy identity");
        assert_eq!(
            identity.keys().map(String::as_str).collect::<Vec<_>>(),
            ["sha256", "status", "version"],
            "{identity:?}"
        );
    }

    // `show-run` verifies the real seal against the same per-user key the
    // recording CLI sealed it with.
    let summary = execute(
        work.path(),
        &["show-run", record.to_str().unwrap(), "--json"],
    );
    assert!(
        summary.status.success(),
        "{}",
        String::from_utf8_lossy(&summary.stderr)
    );
    let report: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert_eq!(report["audit_verified"], true);
    assert_eq!(report["audit_error"], Value::Null);
    assert_eq!(report["audit_mac"], document["audit_mac"]);
    assert!(
        report["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|decision| decision["action"] == "environment.read"
                && decision["outcome"] == "denied"),
        "{report}"
    );
    let text = execute(work.path(), &["show-run", record.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("environment.read"), "{text}");
    assert!(text.contains("denied"), "{text}");
    assert!(!text.contains("warning"), "{text}");
}

#[test]
fn a_real_record_without_a_policy_is_sealed_and_shows_its_baseline() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"hello\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let document: Value =
        serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap();
    assert_eq!(document["policy"]["execution_mode"], "trusted");
    assert_eq!(document["policy"]["capability_policy"]["status"], "absent");
    assert_eq!(document["policy"]["data_flow_policy"]["status"], "absent");
    assert!(
        document["audit_mac"].as_str().map(str::len) == Some(64),
        "{document}"
    );

    let summary = execute(
        work.path(),
        &["show-run", record.to_str().unwrap(), "--json"],
    );
    assert!(
        summary.status.success(),
        "{}",
        String::from_utf8_lossy(&summary.stderr)
    );
    let report: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert_eq!(report["audit_verified"], true);
    assert_eq!(report["audit_error"], Value::Null);
    let text = execute(work.path(), &["show-run", record.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(
        text.contains("no confinement: no policy was active"),
        "{text}"
    );
}

#[test]
fn a_tampered_real_record_is_shown_as_untrusted_not_hidden() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"hello\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // Flip the first kept decision the way a naive edit would, without
    // recomputing its chain link or the seal.
    let path = record.join("run.json");
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(
        !document["decisions"].as_array().unwrap().is_empty(),
        "{document}"
    );
    let flipped = if document["decisions"][0]["outcome"] == "approved" {
        "denied"
    } else {
        "approved"
    };
    document["decisions"][0]["outcome"] = Value::String(flipped.to_owned());
    fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();

    // Warn-open: both renderings still succeed and label the audit untrusted,
    // with the tampered content left visible for review.
    let text = execute(work.path(), &["show-run", record.to_str().unwrap()]);
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("warning"), "{text}");
    assert!(text.contains("untrusted"), "{text}");
    let summary = execute(
        work.path(),
        &["show-run", record.to_str().unwrap(), "--json"],
    );
    assert!(
        summary.status.success(),
        "{}",
        String::from_utf8_lossy(&summary.stderr)
    );
    let report: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert_eq!(report["audit_verified"], false);
    assert!(
        report["audit_error"]
            .as_str()
            .is_some_and(|error| error.contains("modified")),
        "{report}"
    );
    assert_eq!(report["decisions"][0]["outcome"], flipped);
}
