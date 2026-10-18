use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::thread;

fn run(directory: &Path, workflow: &Path, checkpoint: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(workflow)
        .arg("--checkpoint")
        .arg(checkpoint)
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .output()
        .expect("execute starts")
}

#[test]
fn checkpointed_try_and_on_exit_run_their_handlers() {
    let directory = tempfile::tempdir().unwrap();
    let workflow = directory.path().join("workflow.program");
    for (source, success, stdout) in [
        (
            "try\n  then fail\ncatch error\n  run printf with message \"caught\"\nfinally\n  run printf with message \"finally\"\n",
            true,
            "caught\nfinally\n",
        ),
        (
            "run printf with message \"before\"\non exit\n  run printf with message \"cleanup\"\n",
            true,
            "before\ncleanup\n",
        ),
        (
            "run printf with message \"before\"\nthen fail\non exit\n  run printf with message \"cleanup\"\n",
            false,
            "cleanup\n",
        ),
    ] {
        let checkpoint = directory.path().join("checkpoint");
        let _ = fs::remove_dir_all(&checkpoint);
        fs::write(&workflow, source).unwrap();
        let output = run(directory.path(), &workflow, &checkpoint);
        assert_eq!(
            output.status.success(),
            success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), stdout);
        assert!(checkpoint.join("journal.json").exists());
    }
}

#[test]
fn checkpointed_save_and_append_complete_their_nodes() {
    let directory = tempfile::tempdir().unwrap();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(
        &workflow,
        "run printf with message \"saved\"\nthen save to \"saved.txt\"\nrun printf with message \"appended\"\nthen append to \"saved.txt\"\n",
    )
    .unwrap();
    let output = run(directory.path(), &workflow, &checkpoint);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(directory.path().join("saved.txt")).unwrap(),
        b"saved\nappended\n"
    );
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(checkpoint.join("journal.json")).unwrap()).unwrap();
    assert_eq!(journal["data"]["status"], "completed");
    assert_eq!(journal["data"]["completed_count"], 4);
}

#[test]
fn checkpoint_journal_can_be_read_repeatedly_during_publication() {
    let directory = tempfile::tempdir().unwrap();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    let source = (0..80)
        .map(|index| format!("run printf with message \"node {index}\"\n"))
        .collect::<String>();
    fs::write(&workflow, source).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let failures = Arc::new(AtomicUsize::new(0));
    let reader_path = checkpoint.join("journal.json");
    let reader_stop = Arc::clone(&stop);
    let reader_failures = Arc::clone(&failures);
    let reader = thread::spawn(move || {
        while !reader_stop.load(Ordering::Relaxed) {
            if reader_path.exists() && fs::read(&reader_path).is_err() {
                reader_failures.fetch_add(1, Ordering::Relaxed);
            }
            thread::yield_now();
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(&workflow)
        .arg("--checkpoint")
        .arg(&checkpoint)
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .output()
        .expect("execute starts");
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let journal = fs::read(checkpoint.join("journal.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&journal).unwrap();
    assert_eq!(parsed["data"]["status"], "completed");

    assert_eq!(failures.load(Ordering::Relaxed), 0);
}

fn execute(directory: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(args)
        .output()
        .expect("execute starts")
}

fn loop_program(tag: &str, items: &str, fail_on: &str) -> String {
    format!(
        "for each name in [{items}]\n  run \"sh\" with arguments [\"-c\", \"echo {tag} $0 >> side.txt; [ $0 != {fail_on} ]\", name]\n"
    )
}

/// #144 and #142: committed elements stay committed across repeated failed resumes
/// with an edited body, and entries appended to the list run normally.
#[test]
fn committed_elements_survive_repeated_resumes_after_body_edits() {
    if Command::new("sh").arg("-c").arg("true").output().is_err() {
        eprintln!("skipped: no sh on PATH");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let program = path.join("p.program");
    let items = "\"a\", \"b\", \"c\", \"d\"";
    fs::write(&program, loop_program("v1", items, "c")).unwrap();
    assert!(!execute(path, &["run", "p.program", "--checkpoint", "ck"])
        .status
        .success());
    fs::write(&program, loop_program("v2", items, "c")).unwrap();
    for _ in 0..2 {
        let output = execute(
            path,
            &[
                "resume",
                "p.program",
                "--checkpoint",
                "ck",
                "--retry-uncertain",
            ],
        );
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("failed"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::write(
        &program,
        loop_program("v3", "\"a\", \"b\", \"c\", \"d\", \"e\"", "none"),
    )
    .unwrap();
    let output = execute(
        path,
        &[
            "resume",
            "p.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let side = fs::read_to_string(path.join("side.txt")).unwrap();
    let lines: Vec<&str> = side.lines().collect();
    assert_eq!(
        lines,
        ["v1 a", "v1 b", "v1 c", "v2 c", "v2 c", "v3 c", "v3 d", "v3 e"]
    );
}
