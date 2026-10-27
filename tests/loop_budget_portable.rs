//! Per-loop limits apply equally on native hosts, independent of shell tools.
use flow::{manifest::Registry, parser, runtime};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn execute(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(cwd)
        .env("XDG_STATE_HOME", cwd)
        .env("LOCALAPPDATA", cwd)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn sequential_sixty_thousand_iteration_loops_do_not_share_a_budget() {
    let dir = tempfile::tempdir().unwrap();
    let program = parser::parse(
        "repeat 60000 times\n  use value \"x\"\nrepeat 60000 times\n  use value \"done\"\n",
    )
    .unwrap();
    let result = runtime::run(
        &program,
        &Registry::default(),
        &runtime::RunOptions::new(dir.path().to_path_buf()),
    )
    .unwrap();
    assert_eq!(result.output, b"done");
}

#[test]
fn cli_loop_limit_rejects_oversized_loop_and_can_be_raised() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("limit.program"),
        "repeat 1000001 times\n  use value \"done\"\n",
    )
    .unwrap();
    let rejected = execute(dir.path(), &["run", "limit.program"]);
    assert!(!rejected.status.success());
    let message = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        message.contains("line 1") && message.contains("1000000"),
        "{message}"
    );
    let allowed = execute(
        dir.path(),
        &["run", "limit.program", "--max-loop-iterations", "1000001"],
    );
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert_eq!(allowed.stdout, b"done");
}

#[test]
fn cli_loop_limit_rejects_zero_and_duplicate_options() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("empty.program"), "use value \"done\"\n").unwrap();
    for args in [
        vec!["run", "empty.program", "--max-loop-iterations", "0"],
        vec![
            "run",
            "empty.program",
            "--max-loop-iterations",
            "1",
            "--max-loop-iterations",
            "2",
        ],
    ] {
        let output = execute(dir.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("max-loop-iterations"));
    }
}

#[test]
fn scheduled_handler_uses_the_callers_loop_limit() {
    let dir = tempfile::tempdir().unwrap();
    let program =
        parser::parse("every 1 millisecond\n  repeat 3 times\n    use value \"tick\"\n").unwrap();
    let mut options = runtime::RunOptions::new(dir.path().to_path_buf());
    options.max_loop_iterations = 2;
    let limits = runtime::ServiceLimits {
        max_runs: Some(1),
        max_record_bytes: 128,
    };
    let error = runtime::run_service(
        &program,
        &Registry::default(),
        &options,
        &limits,
        |_| Ok(()),
    )
    .err()
    .expect("the handler's loop should exceed the caller's limit");
    assert!(format!("{error:#}").contains("budget of 2"));
    options.max_loop_iterations = 3;
    let result = runtime::run_service(
        &program,
        &Registry::default(),
        &options,
        &limits,
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(result.runs, 1);
}
